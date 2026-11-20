//! native_token_cache.rs — 原生（Rust BPE）token 计数的字节预算 LRU 缓
//! 存（重写自 `src/services/nativeTokenCache.ts`）。
//!
//! 为什么用短哈希键而不是原始内容串：Map 的键是强引用，MEMORY_
//! PROJECTION 释放 tool_result 原文后，缓存会成为数百 KB 文本的最后
//! 强引用持有者。三个对策（对齐仓库缓存 doctrine）：
//! 1. 键缩短：`hashContent(content):length`（wyhash64/sha256 回退），
//!    全文不再被缓存持有，GC 可在 projection 清除后回收；
//! 2. 字节预算：每条按其内容字节数记账，总量默认 8 MiB，单条超过
//!    maxItemBytes（默认 1 MiB）只记账不存储（每次重算）；
//! 3. LRU 逐出：get 刷新最近性，超预算时逐出最久未用。
//!
//! 语义对齐 utils/cache_budget.rs（ByteBudgetCache）与 TS
//! `createByteBudgetCache`：get 刷新最近性；set 在单项超预算时返回
//! false（不存），总量超预算时 LRU 逐出。

use std::sync::Mutex;

use crate::utils::cache_budget::{ByteBudgetCache, DEFAULT_MAX_ITEM_BYTES, DEFAULT_MAX_TOTAL_BYTES};
use crate::utils::hash::hash_content;

/// 每条条目的平摊开销（哈希键 + {count, contentBytes} 包装）。
const KEY_WRAPPER_OVERHEAD_BYTES: usize = 64;

#[derive(Debug, Clone, Copy)]
struct NativeTokenCacheEntry {
    count: usize,
    /// 计算来源的内容长度——预算的记账单位。
    content_bytes: usize,
}

/// 字节预算的原生 token 计数缓存。
pub struct NativeTokenCache {
    inner: Mutex<ByteBudgetCache<String, NativeTokenCacheEntry>>,
}

#[derive(Debug, Clone, Default)]
pub struct NativeTokenCacheOptions {
    pub max_total_bytes: Option<usize>,
    pub max_item_bytes: Option<usize>,
}

impl NativeTokenCache {
    /// `content` 的缓存计数；未命中返回 None。命中刷新 LRU 最近性。
    pub fn get(&self, content: &str) -> Option<usize> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&native_token_cache_key(content))
            .map(|entry| entry.count)
    }

    /// 存计数；内容超过单项预算时返回 false（不存储）。调用两种结果都
    /// 保留新算的计数。
    pub fn set(&self, content: &str, count: usize) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set(
                native_token_cache_key(content),
                NativeTokenCacheEntry {
                    count,
                    content_bytes: content.len(),
                },
            )
    }

    /// 清空缓存。
    pub fn clear(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 当前条目数。
    pub fn size(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 当前字节预算运行总量。
    pub fn bytes(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).bytes()
    }
}

/// 短缓存键：64 位内容哈希 + 长度（绝不持有全文）。
pub fn native_token_cache_key(content: &str) -> String {
    format!("{}:{}", hash_content(content), content.len())
}

/// 创建字节预算的原生 token 计数缓存。
pub fn create_native_token_cache(opts: NativeTokenCacheOptions) -> NativeTokenCache {
    let cache = ByteBudgetCache::new(
        |entry: &NativeTokenCacheEntry, _key: &String| {
            entry.content_bytes + KEY_WRAPPER_OVERHEAD_BYTES
        },
        opts.max_total_bytes.unwrap_or(DEFAULT_MAX_TOTAL_BYTES),
        opts.max_item_bytes.unwrap_or(DEFAULT_MAX_ITEM_BYTES),
    );
    NativeTokenCache {
        inner: Mutex::new(cache),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_key_never_contains_content() {
        let key = native_token_cache_key("super secret content");
        assert!(!key.contains("super"));
    }

    #[test]
    fn set_get_roundtrip() {
        let cache = create_native_token_cache(NativeTokenCacheOptions::default());
        assert!(cache.get("hello").is_none());
        assert!(cache.set("hello", 5));
        assert_eq!(cache.get("hello"), Some(5));
        assert_eq!(cache.size(), 1);
    }

    #[test]
    fn oversize_items_not_stored() {
        let cache = create_native_token_cache(NativeTokenCacheOptions {
            max_total_bytes: Some(1024 * 1024),
            max_item_bytes: Some(64),
            ..Default::default()
        });
        let big = "x".repeat(65);
        assert!(!cache.set(&big, 1));
        assert!(cache.get(&big).is_none());
    }

    #[test]
    fn clear_resets_state() {
        let cache = create_native_token_cache(NativeTokenCacheOptions::default());
        cache.set("a", 1);
        cache.clear();
        assert_eq!(cache.size(), 0);
        assert!(cache.get("a").is_none());
    }
}
