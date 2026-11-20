//! system_prompt_sections.rs — 系统提示段落注册与缓存。
//!
//! 重写自 `src/constants/systemPromptSections.ts`。计算一次、缓存到
//! /clear 或 /compact 的段落经 `bootstrap::state` 的全局缓存管理；
//! `DANGEROUS_uncachedSystemPromptSection` 每轮重新计算（会打断
//! prompt cache，需给出理由）。

use std::future::Future;
use std::pin::Pin;

use crate::bootstrap::state::{
    clear_beta_header_latches, clear_system_prompt_section_state, get_system_prompt_section_cache,
    set_system_prompt_section_cache_entry,
};

/// 计算函数的异步返回值：`string | null`。
type ComputeResult = Option<String>;

/// 异步计算句柄（对齐 TS 的 `Promise<string | null>`）。
pub type ComputeFut = Pin<Box<dyn Future<Output = ComputeResult> + Send>>;

/// 段落计算函数类型（对齐 TS 的 `() => string | null | Promise<string | null>`）。
pub type ComputeFn = Box<dyn Fn() -> ComputeFut + Send + Sync>;

pub struct SystemPromptSection {
    pub name: String,
    /// 段落计算函数；可能同步返回也可能返回 Future，统一装箱为异步句柄。
    pub compute: ComputeFn,
    /// true 表示每轮重新计算，会打断 prompt cache。
    pub cache_break: bool,
}

/// 创建带缓存的系统提示段落。
/// 计算一次，缓存到 /clear 或 /compact。
pub fn system_prompt_section<F, Fut>(name: impl Into<String>, compute: F) -> SystemPromptSection
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ComputeResult> + Send + 'static,
{
    SystemPromptSection {
        name: name.into(),
        compute: Box::new(move || Box::pin(compute())),
        cache_break: false,
    }
}

/// 创建每轮重新计算的易变系统提示段落。
/// 值变化时会打断 prompt cache；`_reason` 需说明缓存打断的必要性。
pub fn dangerous_uncached_system_prompt_section<F, Fut>(
    name: impl Into<String>,
    compute: F,
    _reason: impl Into<String>,
) -> SystemPromptSection
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ComputeResult> + Send + 'static,
{
    SystemPromptSection {
        name: name.into(),
        compute: Box::new(move || Box::pin(compute())),
        cache_break: true,
    }
}

/// 解析全部系统提示段落，返回提示字符串（null 表示该段落不存在）。
pub async fn resolve_system_prompt_sections(
    sections: &[SystemPromptSection],
) -> Vec<ComputeResult> {
    let mut results = Vec::with_capacity(sections.len());
    for section in sections {
        // cache.get() 返回 Option<Option<String>>：外层表示条目是否存在，
        // 内层对齐 TS 的 `cache.get(name) ?? null`。
        let cached: Option<ComputeResult> = {
            let cache = get_system_prompt_section_cache();
            if !section.cache_break && cache.has(&section.name) {
                cache.get(&section.name)
            } else {
                None
            }
        };
        match cached {
            Some(value) => results.push(value),
            None => {
                let value = (section.compute)().await;
                set_system_prompt_section_cache_entry(&section.name, value.clone());
                results.push(value);
            }
        }
    }
    results
}

/// 清除全部系统提示段落状态。/clear 与 /compact 时调用。
/// 同时复位 beta header latch，让新会话获得对 AFK/fast-mode/
/// cache-editing header 的全新评估。
pub fn clear_system_prompt_sections() {
    clear_system_prompt_section_state();
    clear_beta_header_latches();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // tests 共享 bootstrap::state 的全局 section 缓存，并行运行会互相清场；
    // 用文件级互斥锁串行化（持有跨 .await 安全：竞争者整线程阻塞，不占运行时）。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_state() {
        clear_system_prompt_sections();
    }

    fn make_cached_section(name: &'static str, calls: Arc<AtomicUsize>) -> SystemPromptSection {
        system_prompt_section(name, move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Some("value".to_string())
            }
        })
    }

    fn make_null_section(name: &'static str, calls: Arc<AtomicUsize>) -> SystemPromptSection {
        system_prompt_section(name, move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            }
        })
    }

    fn make_uncached_section(name: &'static str, calls: Arc<AtomicUsize>) -> SystemPromptSection {
        dangerous_uncached_system_prompt_section(
            name,
            move || {
                let calls = calls.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    Some(format!("turn-{n}"))
                }
            },
            "tests",
        )
    }

    #[tokio::test]
    async fn cached_section_computes_once() {
        let _guard = lock();
        clear_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let first =
            resolve_system_prompt_sections(&[make_cached_section("test-cached", calls.clone())])
                .await;
        let second =
            resolve_system_prompt_sections(&[make_cached_section("test-cached", calls.clone())])
                .await;
        assert_eq!(first, vec![Some("value".to_string())]);
        assert_eq!(second, vec![Some("value".to_string())]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        clear_state();
    }

    #[tokio::test]
    async fn uncached_section_recomputes_every_turn() {
        let _guard = lock();
        clear_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let first =
            resolve_system_prompt_sections(&[make_uncached_section("test-uncached", calls.clone())])
                .await;
        let second =
            resolve_system_prompt_sections(&[make_uncached_section("test-uncached", calls.clone())])
                .await;
        assert_eq!(first, vec![Some("turn-0".to_string())]);
        assert_eq!(second, vec![Some("turn-1".to_string())]);
        clear_state();
    }

    #[tokio::test]
    async fn cached_null_value_is_reused() {
        let _guard = lock();
        clear_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let first =
            resolve_system_prompt_sections(&[make_null_section("test-null", calls.clone())]).await;
        let second =
            resolve_system_prompt_sections(&[make_null_section("test-null", calls.clone())]).await;
        assert_eq!(first, vec![None]);
        assert_eq!(second, vec![None]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        clear_state();
    }

    #[tokio::test]
    async fn order_is_preserved() {
        let _guard = lock();
        clear_state();
        let calls = Arc::new(AtomicUsize::new(0));
        let sections = vec![
            make_cached_section("order-a", calls.clone()),
            make_null_section("order-b", calls.clone()),
            make_cached_section("order-c", calls.clone()),
        ];
        let resolved = resolve_system_prompt_sections(&sections).await;
        assert_eq!(
            resolved,
            vec![
                Some("value".to_string()),
                None,
                Some("value".to_string()),
            ]
        );
        clear_state();
    }
}
