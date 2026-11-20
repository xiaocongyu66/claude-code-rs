//! file_state_cache.rs — 读文件状态缓存（LRU，按大小淘汰）。
//!
//! 重写自 `src/utils/fileStateCache.ts`。语义对齐：
//!
//! - 所有 key 访问前做路径规范化（normalize），保证相对/绝对、
//!   冗余段（/foo/../bar）、Windows 混合分隔符下命中一致。
//! - LRUCache 的 `max` 条目上限 + `maxSize` 字节上限：触发任一上限即
//!   淘汰最久未用（大小按 content 的 UTF-8 字节数计，最少 1）。
//! - `create_file_state_cache_with_size_limit` 默认 25MB、100 条；
//!   ASK 场景常用 10 条。
//! - merge_file_state_caches 以 timestamp 更新的条目覆盖。
//!
//! 偏差：lru-cache 不在 Cargo.toml；LRU 用 VecDeque + HashMap 手写
//! （O(1) 均摊：HashMap 存值与在队列中的位置索引，get/set 移动到队尾）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// 单个文件的读状态。
#[derive(Debug, Clone)]
pub struct FileState {
    pub content: String,
    pub timestamp: i64,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// 被 auto-injection（如 CLAUDE.md）填充且注入内容与磁盘不一致
    /// （剥注释/剥 frontmatter/截断）时为 true：模型只看到了部分视图，
    /// Edit/Write 必须先要求显式 Read。`content` 存的是磁盘原始字节。
    pub is_partial_view: bool,
}

/// 默认最大条目数。
pub const READ_FILE_STATE_CACHE_SIZE: usize = 100;

/// 默认字节上限（25MB），防止大文件内容无限吃内存。
const DEFAULT_MAX_CACHE_SIZE_BYTES: usize = 25 * 1024 * 1024;

struct LruEntry {
    value: FileState,
    /// 在 deque 中的位置（失效时重建整个索引，淘汰频率低）。
    deque_idx: usize,
}

/// 路径键做规范化（normalize）的文件状态缓存。
pub struct FileStateCache {
    max_entries: usize,
    max_size_bytes: usize,
    map: HashMap<String, LruEntry>,
    order: VecDeque<String>,
    /// 当前累计字节数。
    total_size: usize,
}

fn size_of(value: &FileState) -> usize {
    std::cmp::max(1, value.content.len())
}

/// 词法规范化（对齐 TS `path.normalize`）：去掉冗余分隔符与 `.`，
/// 解析 `..`，保留结尾语义。仅作缓存键归一，不触碰文件系统。
fn normalize_key(key: &str) -> String {
    let is_absolute = Path::new(key).is_absolute() || key.starts_with('/');
    let mut parts: Vec<String> = Vec::new();
    for seg in key.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                parts.push("..".to_string());
            }
            s => parts.push(s.to_string()),
        }
    }
    let joined = parts.join(std::path::MAIN_SEPARATOR_STR);
    match (is_absolute, joined.is_empty()) {
        (true, true) => std::path::MAIN_SEPARATOR_STR.to_string(),
        (true, false) => format!("{}{joined}", std::path::MAIN_SEPARATOR_STR),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

impl FileStateCache {
    pub fn new(max_entries: usize, max_size_bytes: usize) -> Self {
        Self {
            max_entries,
            max_size_bytes,
            map: HashMap::new(),
            order: VecDeque::new(),
            total_size: 0,
        }
    }

    fn touch(&mut self, key: &str) {
        // 移动到队尾：先找位置再挪
        if let Some(idx) = self.map.get(key).map(|e| e.deque_idx) {
            if let Some(removed) = self.order.remove(idx) {
                self.order.push_back(removed);
                let new_idx = self.order.len() - 1;
                if let Some(entry) = self.map.get_mut(key) {
                    entry.deque_idx = new_idx;
                }
            }
        }
    }

    /// 淘汰最久未用条目直到同时满足条目与字节上限。
    fn evict_if_needed(&mut self) {
        while self.order.len() > self.max_entries || self.total_size > self.max_size_bytes {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some(entry) = self.map.remove(&key) {
                self.total_size -= size_of(&entry.value);
            }
        }
    }

    pub fn get(&mut self, key: &str) -> Option<FileState> {
        let normalized = normalize_key(key);
        let exists = self.map.contains_key(&normalized);
        if exists {
            self.touch(&normalized);
            self.map.get(&normalized).map(|e| e.value.clone())
        } else {
            None
        }
    }

    pub fn set(&mut self, key: &str, value: FileState) {
        let normalized = normalize_key(key);
        let size = size_of(&value);
        if self.map.contains_key(&normalized) {
            self.touch(&normalized);
            if let Some(entry) = self.map.get_mut(&normalized) {
                self.total_size -= size_of(&entry.value);
                self.total_size += size;
                entry.value = value;
            }
        } else {
            self.order.push_back(normalized.clone());
            let deque_idx = self.order.len() - 1;
            self.total_size += size;
            self.map.insert(normalized, LruEntry { value, deque_idx });
        }
        self.evict_if_needed();
    }

    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(&normalize_key(key))
    }

    pub fn delete(&mut self, key: &str) -> bool {
        let normalized = normalize_key(key);
        if let Some(entry) = self.map.remove(&normalized) {
            self.total_size -= size_of(&entry.value);
            let _ = self.order.remove(entry.deque_idx);
            self.reindex();
            true
        } else {
            false
        }
    }

    fn reindex(&mut self) {
        for (idx, key) in self.order.iter().enumerate() {
            if let Some(entry) = self.map.get_mut(key) {
                entry.deque_idx = idx;
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.total_size = 0;
    }

    pub fn size(&self) -> usize {
        self.map.len()
    }

    pub fn max(&self) -> usize {
        self.max_entries
    }

    pub fn max_size(&self) -> usize {
        self.max_size_bytes
    }

    /// 当前累计字节数（对齐 TS LruCache.calculatedSize）。
    pub fn calculated_size(&self) -> usize {
        self.total_size
    }

    /// 全部键（LRU→MRU）。
    pub fn keys(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }

    /// 全部条目（LRU→MRU）。
    pub fn entries(&self) -> Vec<(String, FileState)> {
        self.order
            .iter()
            .filter_map(|k| self.map.get(k).map(|e| (k.clone(), e.value.clone())))
            .collect()
    }

    pub fn dump(&self) -> Vec<(String, FileState)> {
        self.entries()
    }

    pub fn load(&mut self, entries: Vec<(String, FileState)>) {
        for (key, value) in entries {
            self.set(&key, value);
        }
    }
}

/// 创建带大小上限的 FileStateCache。
///
/// 大文件（图片不缓存，见 FileReadTool）主要靠字节上限拦截。
pub fn create_file_state_cache_with_size_limit(
    max_entries: usize,
    max_size_bytes: Option<usize>,
) -> FileStateCache {
    FileStateCache::new(
        max_entries,
        max_size_bytes.unwrap_or(DEFAULT_MAX_CACHE_SIZE_BYTES),
    )
}

/// 缓存转对象（compact.ts 用）。
pub fn cache_to_object(cache: &FileStateCache) -> HashMap<String, FileState> {
    cache.entries().into_iter().collect()
}

/// 全部键（多个组件用）。
pub fn cache_keys(cache: &FileStateCache) -> Vec<String> {
    cache.keys()
}

/// 克隆缓存（保留大小上限配置）。
pub fn clone_file_state_cache(cache: &FileStateCache) -> FileStateCache {
    let mut cloned =
        create_file_state_cache_with_size_limit(cache.max(), Some(cache.max_size()));
    cloned.load(cache.dump());
    cloned
}

/// 合并两个缓存：timestamp 更新的条目覆盖较旧的。
pub fn merge_file_state_caches(
    first: &FileStateCache,
    second: &FileStateCache,
) -> FileStateCache {
    let mut merged = clone_file_state_cache(first);
    for (file_path, file_state) in second.entries() {
        let override_entry = match merged.get(&file_path) {
            Some(existing) => file_state.timestamp > existing.timestamp,
            None => true,
        };
        if override_entry {
            merged.set(&file_path, file_state);
        }
    }
    merged
}
