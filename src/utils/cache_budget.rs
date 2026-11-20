//! cache_budget.rs — 按字节预算限制的 Map 型 LRU 缓存。
//!
//! 重写自 `src/utils/cacheBudget.ts`（TS 端基于 lru-cache 的
//! maxSize + maxEntrySize 记账）。Rust 端手写等价结构：链表式 LRU +
//! 字节计数。Cargo.toml 无 lru-cache 依赖。
//!
//! 语义：
//! - `get` 刷新 LRU 最近性并返回值（不存在返回 None）
//! - `set`：单个条目估算字节数超过单项预算时返回 false（不存）；
//!   总字节数超过总预算时逐出最久未用条目直至装得下

use std::collections::HashMap;
use std::hash::Hash;

/// 默认总预算：组件级缓存最坏保留 ~8 MiB。
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// 默认单项预算：超过 1 MiB 的单个条目不再值得缓存。
pub const DEFAULT_MAX_ITEM_BYTES: usize = 1024 * 1024;

/// 按字节预算限制的 LRU 缓存。
pub struct ByteBudgetCache<K, V> {
    map: HashMap<K, (V, u64)>, // key → (value, insertion sequence)
    order: Vec<K>,             // LRU 队列：头部最久未用
    next_seq: u64,
    max_total_bytes: usize,
    max_item_bytes: usize,
    estimate_size: Box<dyn Fn(&V, &K) -> usize + Send + Sync>,
    bytes: usize,
}

impl<K: Eq + Hash + Clone, V> ByteBudgetCache<K, V> {
    /// 创建缓存。`estimate_size` 为值的估算占用字节数（必须确定性：
    /// 插入与逐出记账都用它；估算允许不精确，只需随真实占用缩放）。
    pub fn new<F>(estimate_size: F, max_total_bytes: usize, max_item_bytes: usize) -> Self
    where
        F: Fn(&V, &K) -> usize + Send + Sync + 'static,
    {
        // 单项限额钳到总预算内，与 TS 端 Math.min 一致
        let item_limit = max_item_bytes.min(max_total_bytes);
        Self {
            map: HashMap::new(),
            order: Vec::new(),
            next_seq: 0,
            max_total_bytes,
            max_item_bytes: item_limit,
            estimate_size: Box::new(estimate_size),
            bytes: 0,
        }
    }

    /// 查询并刷新最近性。
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if let Some((value, seq)) = self.map.get(key) {
            self.touch(key, *seq);
            Some(value)
        } else {
            None
        }
    }

    fn touch(&mut self, key: &K, _seq: u64) {
        // O(n) 移动到队尾；条目量级与 TS lru-cache 的双向链表不同，
        // 但 8MiB 预算下条目数有上界。
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos);
            self.order.push(k);
        }
    }

    /// 插入条目。值超过单项预算时返回 false（不存）；
    /// 否则逐出 LRU 条目直至总预算容纳，返回 true。
    pub fn set(&mut self, key: K, value: V) -> bool {
        let size = (self.estimate_size)(&value, &key).max(1);
        if size > self.max_item_bytes {
            return false;
        }
        // 已存在则先移除旧条目记账
        if let Some((old_value, _)) = self.map.remove(&key) {
            let old_size = (self.estimate_size)(&old_value, &key).max(1);
            self.bytes -= old_size;
            self.order.retain(|k| k != &key);
        }

        // 逐出最久未用直至装得下（size ≤ max_total_bytes 才可能成功）
        while self.bytes + size > self.max_total_bytes {
            match self.order.first().cloned() {
                Some(evict_key) => {
                    self.order.remove(0);
                    if let Some((evict_value, _)) = self.map.remove(&evict_key) {
                        self.bytes -= (self.estimate_size)(&evict_value, &evict_key).max(1);
                    }
                }
                None => break,
            }
        }

        if self.bytes + size > self.max_total_bytes {
            // size 本身就超过总预算（单项限额已钳到总预算内，正常不会到这）
            return false;
        }

        self.next_seq += 1;
        self.order.push(key.clone());
        self.map.insert(key, (value, self.next_seq));
        self.bytes += size;
        true
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// 删除条目；存在返回 true。
    pub fn delete(&mut self, key: &K) -> bool {
        if let Some((value, _)) = self.map.remove(key) {
            self.bytes -= (self.estimate_size)(&value, key).max(1);
            self.order.retain(|k| k != key);
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 当前保留条目的 estimateSize() 运行总和。
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(
        size_of: fn(&String, &String) -> usize,
        total: usize,
        item: usize,
    ) -> ByteBudgetCache<String, String> {
        ByteBudgetCache::new(size_of, total, item)
    }

    fn len_size(v: &String, _k: &String) -> usize {
        v.len()
    }

    #[test]
    fn stores_and_gets() {
        let mut cache = make(len_size, 1024, 128);
        assert!(cache.set("k1".into(), "v1".into()));
        assert_eq!(cache.get(&"k1".to_string()), Some(&"v1".to_string()));
        assert!(cache.contains(&"k1".to_string()));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), 2);
    }

    #[test]
    fn oversized_item_not_stored() {
        let mut cache = make(len_size, 1024, 4);
        assert!(!cache.set("big".into(), "toolongvalue".into()));
        assert!(cache.is_empty());
    }

    #[test]
    fn evicts_lru_on_budget_overflow() {
        let mut cache = make(len_size, 8, 8);
        assert!(cache.set("a".into(), "aaaa".into())); // 4 bytes
        assert!(cache.set("b".into(), "aaaa".into())); // 4 bytes（满）
        // 再插 4 字节：a（最旧）被逐出
        assert!(cache.set("c".into(), "aaaa".into()));
        assert!(!cache.contains(&"a".to_string()));
        assert!(cache.contains(&"b".to_string()));
        assert!(cache.contains(&"c".to_string()));
        assert_eq!(cache.bytes(), 8);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = make(len_size, 8, 8);
        cache.set("a".into(), "aaaa".into());
        cache.set("b".into(), "aaaa".into());
        // 访问 a 后，b 成为最旧
        assert_eq!(cache.get(&"a".to_string()), Some(&"aaaa".to_string()));
        cache.set("c".into(), "aaaa".into());
        assert!(cache.contains(&"a".to_string()));
        assert!(!cache.contains(&"b".to_string()));
    }

    #[test]
    fn delete_and_clear() {
        let mut cache = make(len_size, 1024, 128);
        cache.set("a".into(), "v".into());
        assert!(cache.delete(&"a".to_string()));
        assert!(!cache.delete(&"a".to_string()));
        cache.set("b".into(), "v".into());
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }
}
