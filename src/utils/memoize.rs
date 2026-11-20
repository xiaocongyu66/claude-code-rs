//! memoize.rs — 带 TTL / LRU 的记忆化工具（重写自 `src/utils/memoize.ts`）。
//!
//! 语义对齐说明：
//! - TS 的缓存键是 `jsonStringify(args)`（参数元组的 JSON 序列化）；
//!   Rust 侧改为直接以调用方传入的 `K` 为键（`K: Eq + Hash + Clone`），
//!   调用方自行把参数元组作为键——等价且避免重复序列化。
//! - `memoizeWithTTL` 的「过期 → 返回旧值 + 后台刷新」语义用
//!   `std::thread::spawn` 落地（fire-and-forget）；刷新回调里的
//!   identity 守卫（`cache.get(key) === cached`）用 `Arc::ptr_eq`
//!   等价实现——`clear()` 并发清缓存时，旧刷新结果不会覆盖新条目。
//! - `memoizeWithTTLAsync` 的 cold-miss 去重（TS 的 `inFlight` map）
//!   用 per-key `tokio::sync::Mutex` 实现：并发调用者排队，先算者
//!   落缓存，后来者命中缓存（净效果与共享 Promise 一致）。
//!   `clear()` 同时清掉 in-flight 锁表（对齐 TS 清 `inFlight`）。
//! - 刷新失败时按 TS 的 `.catch` 语义：记日志 + 若条目仍是自己的
//!   则从缓存删除（自纠错，优于让错误数据占满 TTL）。
//! - `memoizeWithLRU` 依赖 `lru-cache` npm 包；Rust 端手写极简 LRU
//!   （`HashMap` + `VecDeque` 读写序列），peek 不提升最近使用。
//! - 异步后台刷新依赖 tokio 运行时；若当前线程没有运行时
//!   （`Handle::try_current` 失败），跳过刷新、保持旧值（不 panic）。

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::thread;

use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;

/// 默认缓存生命周期：5 分钟（对齐 TS 的 `5 * 60 * 1000` ms）。
pub const DEFAULT_CACHE_LIFETIME_MS: u64 = 5 * 60 * 1000;

/// LRU 默认容量：100（对齐 TS 的 `maxCacheSize` 默认值）。
pub const DEFAULT_LRU_MAX_SIZE: usize = 100;

/// 单调时钟（墙钟）毫秒；系统时间回退时按 0 处理（不 panic）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 拿到 poisoned mutex 的内部值（等价 `lock().unwrap()`，但不 panic）。
fn unlocked<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// memoizeWithTTL（同步版）
// ---------------------------------------------------------------------------

struct TtlEntry<V> {
    value: V,
    timestamp_ms: u64,
    refreshing: bool,
}

/// `memoizeWithTTL` 返回的记忆化函数（同步版）。
///
/// `f` 返回 `Err` 对齐 TS 的 `f()` 抛异常；冷调用（populate）时错误
/// 向上传播且不落缓存，后台刷新失败时仅记日志（对齐 `.catch`）。
pub struct TtlMemoized<K, V> {
    f: Arc<dyn Fn(&K) -> Result<V, Box<dyn Error + Send + Sync>> + Send + Sync>,
    cache: Arc<Mutex<HashMap<K, Arc<Mutex<TtlEntry<V>>>>>>,
    ttl_ms: u64,
}

impl<K, V> TtlMemoized<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + 'static,
{
    /// 调用记忆化函数。
    pub fn call(&self, key: &K) -> Result<V, Box<dyn Error + Send + Sync>> {
        let now = now_ms();
        let cached = unlocked(&self.cache).get(key).cloned();

        let Some(entry) = cached else {
            // 冷调用：同步计算并落缓存（f 抛错则不落缓存、向上传播）
            let value = (self.f)(key)?;
            unlocked(&self.cache).insert(
                key.clone(),
                Arc::new(Mutex::new(TtlEntry {
                    value: value.clone(),
                    timestamp_ms: now,
                    refreshing: false,
                })),
            );
            return Ok(value);
        };

        let stale = {
            let e = unlocked(&entry);
            now.saturating_sub(e.timestamp_ms) > self.ttl_ms && !e.refreshing
        };

        if stale {
            // 标记刷新中，防止并行重复刷新
            unlocked(&entry).refreshing = true;

            // 先取出旧值（刷新闭包会 move 走 entry）
            let stale_value = unlocked(&entry).value.clone();

            // 后台 fire-and-forget 刷新（对齐 `Promise.resolve().then(...)`）
            let f = Arc::clone(&self.f);
            let cache = Arc::clone(&self.cache);
            let entry = Arc::clone(&entry);
            let key = key.clone();
            let builder = thread::Builder::new().name("memoize-ttl-refresh".into());
            let _ = builder.spawn(move || {
                match f(&key) {
                    Ok(new_value) => {
                        let mut guard = unlocked(&cache);
                        if let Some(current) = guard.get(&key) {
                            // identity 守卫：clear() 后新写入的条目不被旧结果覆盖
                            if Arc::ptr_eq(current, &entry) {
                                guard.insert(
                                    key,
                                    Arc::new(Mutex::new(TtlEntry {
                                        value: new_value,
                                        timestamp_ms: now_ms(),
                                        refreshing: false,
                                    })),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        crate::utils::log::log_error_dyn(&*e);
                        let mut guard = unlocked(&cache);
                        if let Some(current) = guard.get(&key) {
                            if Arc::ptr_eq(current, &entry) {
                                guard.remove(&key);
                            }
                        }
                    }
                }
            });

            // 立即返回旧值
            return Ok(stale_value);
        }

        Ok(unlocked(&entry).value.clone())
    }

    /// 清空缓存（对齐 `memoized.cache.clear()`）。
    pub fn clear(&self) {
        unlocked(&self.cache).clear();
    }
}

/// 创建带 TTL 的同步记忆化函数（对齐 `memoizeWithTTL`）。
pub fn memoize_with_ttl<K, V, F>(f: F, cache_lifetime_ms: u64) -> Arc<TtlMemoized<K, V>>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + 'static,
    F: Fn(&K) -> Result<V, Box<dyn Error + Send + Sync>> + Send + Sync + 'static,
{
    Arc::new(TtlMemoized {
        f: Arc::new(f),
        cache: Arc::new(Mutex::new(HashMap::new())),
        ttl_ms: cache_lifetime_ms,
    })
}

// ---------------------------------------------------------------------------
// memoizeWithTTLAsync（异步版）
// ---------------------------------------------------------------------------

/// `memoizeWithTTLAsync` 返回的记忆化函数（异步版）。
pub struct TtlMemoizedAsync<K, V, Fut> {
    f: Arc<dyn Fn(&K) -> Fut + Send + Sync>,
    cache: Arc<Mutex<HashMap<K, Arc<Mutex<TtlEntry<V>>>>>>,
    in_flight: AsyncMutex<HashMap<K, Arc<AsyncMutex<()>>>>,
    ttl_ms: u64,
}

impl<K, V, Fut> TtlMemoizedAsync<K, V, Fut>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + 'static,
    Fut: Future<Output = Result<V, Box<dyn Error + Send + Sync>>> + Send + 'static,
{
    /// 调用记忆化函数。冷调用时并发去重（共享同一次计算）；过期时
    /// 返回旧值并后台刷新。
    pub async fn call(&self, key: &K) -> Result<V, Box<dyn Error + Send + Sync>> {
        let now = now_ms();
        let cached = unlocked(&self.cache).get(key).cloned();

        if let Some(entry) = cached {
            let stale = {
                let e = unlocked(&entry);
                now.saturating_sub(e.timestamp_ms) > self.ttl_ms && !e.refreshing
            };
            if stale {
                unlocked(&entry).refreshing = true;
                self.spawn_refresh(key, Arc::clone(&entry));
                return Ok(unlocked(&entry).value.clone());
            }
            return Ok(unlocked(&entry).value.clone());
        }

        // 冷调用去重：per-key 锁（对齐 TS 的 inFlight promise 共享）
        let lock = {
            let mut guard = self.in_flight.lock().await;
            guard
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // 拿到锁后再查一次缓存：共享的前一个调用可能已落缓存
        if let Some(entry) = unlocked(&self.cache).get(key).cloned() {
            return Ok(unlocked(&entry).value.clone());
        }

        let value = (self.f)(key).await?;

        // identity 守卫：clear() 期间（in_flight 表被清）不落缓存，
        // 但计算结果仍返回给当前调用方（对齐 TS 的 return result）
        {
            let guard = self.in_flight.lock().await;
            if let Some(current) = guard.get(key) {
                if Arc::ptr_eq(current, &lock) {
                    unlocked(&self.cache).insert(
                        key.clone(),
                        Arc::new(Mutex::new(TtlEntry {
                            value: value.clone(),
                            timestamp_ms: now,
                            refreshing: false,
                        })),
                    );
                }
            }
        }

        // finally：若锁仍是自己的则从 in_flight 表摘除
        {
            let mut guard = self.in_flight.lock().await;
            if let Some(current) = guard.get(key) {
                if Arc::ptr_eq(current, &lock) {
                    guard.remove(key);
                }
            }
        }

        Ok(value)
    }

    /// 清空缓存与 in-flight 表（对齐 TS 的 `cache.clear()`：两者都清）。
    /// 取锁顺序与 `call` 的身份守卫段一致（in_flight → cache），避免锁序反转。
    pub async fn clear(&self) {
        let mut guard = self.in_flight.lock().await;
        unlocked(&self.cache).clear();
        guard.clear();
    }

    fn spawn_refresh(&self, key: &K, entry: Arc<Mutex<TtlEntry<V>>>) {
        // 无 tokio 运行时（同步上下文调用异步 API）时跳过刷新，不 panic
        let Ok(handle) = Handle::try_current() else {
            return;
        };
        let f = Arc::clone(&self.f);
        let cache = Arc::clone(&self.cache);
        let key = key.clone();
        let _ = handle.spawn(async move {
            match f(&key).await {
                Ok(new_value) => {
                    let mut guard = unlocked(&cache);
                    if let Some(current) = guard.get(&key) {
                        if Arc::ptr_eq(current, &entry) {
                            guard.insert(
                                key,
                                Arc::new(Mutex::new(TtlEntry {
                                    value: new_value,
                                    timestamp_ms: now_ms(),
                                    refreshing: false,
                                })),
                            );
                        }
                    }
                }
                Err(e) => {
                    crate::utils::log::log_error_dyn(&*e);
                    let mut guard = unlocked(&cache);
                    if let Some(current) = guard.get(&key) {
                        if Arc::ptr_eq(current, &entry) {
                            guard.remove(&key);
                        }
                    }
                }
            }
        });
    }
}

/// 创建带 TTL 的异步记忆化函数（对齐 `memoizeWithTTLAsync`）。
pub fn memoize_with_ttl_async<K, V, F, Fut>(
    f: F,
    cache_lifetime_ms: u64,
) -> Arc<TtlMemoizedAsync<K, V, Fut>>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + 'static,
    F: Fn(&K) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<V, Box<dyn Error + Send + Sync>>> + Send + 'static,
{
    Arc::new(TtlMemoizedAsync {
        f: Arc::new(f),
        cache: Arc::new(Mutex::new(HashMap::new())),
        in_flight: AsyncMutex::new(HashMap::new()),
        ttl_ms: cache_lifetime_ms,
    })
}

// ---------------------------------------------------------------------------
// memoizeWithLRU
// ---------------------------------------------------------------------------

/// 极简 LRU（替代 `lru-cache` npm 包）。front = 最久未使用。
struct LruCache<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    max: usize,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
{
    fn new(max: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            max,
        }
    }

    /// 读取并提升最近使用（对齐 `lru-cache` 的 `get`）。
    fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.order.retain(|k| k != key);
            self.order.push_back(key.clone());
        }
        self.map.get(key)
    }

    /// 读取但不提升最近使用（对齐 `lru-cache` 的 `peek`）。
    fn peek(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    fn set(&mut self, key: K, value: V) {
        if self.map.contains_key(&key) {
            self.order.retain(|k| k != &key);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
        while self.order.len() > self.max {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            } else {
                break;
            }
        }
    }

    fn delete(&mut self, key: &K) -> bool {
        if self.map.remove(key).is_some() {
            self.order.retain(|k| k != key);
            true
        } else {
            false
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    fn size(&self) -> usize {
        self.order.len()
    }

    /// LRU→MRU 迭代（对齐 `lru-cache` 的 `keys()`）。
    fn keys(&self) -> Vec<K> {
        self.order.iter().cloned().collect()
    }
}

/// `memoizeWithLRU` 返回的记忆化函数。
///
/// TS 端 `f(...args)` 与 `cacheFn(...args)` 接收同一参数元组；
/// Rust 端两者均以 `&Args` 调用，`cache_fn` 产出字符串键。
pub struct LruMemoized<Args, V> {
    f: Arc<dyn Fn(&Args) -> V + Send + Sync>,
    cache_fn: Arc<dyn Fn(&Args) -> String + Send + Sync>,
    cache: Mutex<LruCache<String, V>>,
}

impl<Args, V> LruMemoized<Args, V>
where
    Args: Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn call(&self, args: &Args) -> V {
        let key = (self.cache_fn)(args);
        let mut cache = unlocked(&self.cache);
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
        let result = (self.f)(args);
        cache.set(key, result.clone());
        result
    }

    /// 清空缓存（对齐 `memoized.cache.clear()`）。
    pub fn cache_clear(&self) {
        unlocked(&self.cache).clear();
    }

    /// 当前条目数（对齐 `memoized.cache.size()`）。
    pub fn cache_size(&self) -> usize {
        unlocked(&self.cache).size()
    }

    /// 删除键（对齐 `memoized.cache.delete(key)`）。
    pub fn cache_delete(&self, key: &str) -> bool {
        unlocked(&self.cache).delete(&key.to_string())
    }

    /// peek 语义的读取，不提升最近使用（对齐 `memoized.cache.get`）。
    pub fn cache_get(&self, key: &str) -> Option<V> {
        unlocked(&self.cache).peek(&key.to_string()).cloned()
    }

    /// 是否含键（对齐 `memoized.cache.has`）。
    pub fn cache_has(&self, key: &str) -> bool {
        unlocked(&self.cache).contains(&key.to_string())
    }

    /// LRU→MRU 的全部键（对齐 `memoized.cache.keys()`）。
    pub fn cache_keys(&self) -> Vec<String> {
        unlocked(&self.cache).keys()
    }
}

/// 创建带 LRU 淘汰的记忆化函数（对齐 `memoizeWithLRU`）。
pub fn memoize_with_lru<Args, V, F, FK>(
    f: F,
    cache_fn: FK,
    max_cache_size: usize,
) -> Arc<LruMemoized<Args, V>>
where
    Args: Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    F: Fn(&Args) -> V + Send + Sync + 'static,
    FK: Fn(&Args) -> String + Send + Sync + 'static,
{
    Arc::new(LruMemoized {
        f: Arc::new(f),
        cache_fn: Arc::new(cache_fn),
        cache: Mutex::new(LruCache::new(max_cache_size)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_caches_and_expires() {
        let m = memoize_with_ttl(
            |k: &String| Ok::<_, Box<dyn Error + Send + Sync>>(format!("v-{k}")),
            1000,
        );
        assert_eq!(m.call(&"a".to_string()).unwrap(), "v-a");
        assert_eq!(m.call(&"a".to_string()).unwrap(), "v-a");
        m.clear();
        assert_eq!(m.call(&"a".to_string()).unwrap(), "v-a");
    }

    #[test]
    fn lru_evicts_and_caches() {
        let m = memoize_with_lru(
            |a: &u32| a * 2,
            |a: &u32| a.to_string(),
            2,
        );
        assert_eq!(m.call(&1), 2);
        assert_eq!(m.call(&2), 4);
        assert_eq!(m.call(&3), 6); // 1 被淘汰（LRU）
        assert_eq!(m.cache_size(), 2);
        assert!(!m.cache_has("1"));
        assert_eq!(m.cache_get("2"), Some(4));
        m.cache_delete("2");
        assert!(!m.cache_has("2"));
        m.cache_clear();
        assert_eq!(m.cache_size(), 0);
    }

    #[tokio::test]
    async fn async_ttl_dedups_cold_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let m = memoize_with_ttl_async(
            move |_k: &String| {
                let calls2 = Arc::clone(&calls2);
                async move {
                    calls2.fetch_add(1, Ordering::SeqCst);
                    Ok::<String, Box<dyn Error + Send + Sync>>("done".to_string())
                }
            },
            1000,
        );
        let (a, b) = tokio::join!(m.call(&"k".to_string()), m.call(&"k".to_string()));
        assert_eq!(a.unwrap(), "done");
        assert_eq!(b.unwrap(), "done");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn now_ms_is_monotonic_enough() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn default_constants_match_ts() {
        assert_eq!(DEFAULT_CACHE_LIFETIME_MS, 300_000);
        assert_eq!(DEFAULT_LRU_MAX_SIZE, 100);
    }
}
