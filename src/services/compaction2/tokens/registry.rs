//! `services/compaction2/tokens/registry.ts` — tokenizer 数据目录注册表。
//!
//! 词表从不打包：从 `CCH_TOKENIZER_DIR`（含 `manifest.json` 与词表文件的
//! 目录）加载。目录/manifest/匹配条目/文件缺失（或文件畸形）时解析返回
//! None，调用方回落 cl100k 原生计数 / 内容自适应估算。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use crate::services::compaction2::tokens::hf_tokenizer::create_hf_tokenizer;
use crate::services::compaction2::tokens::manifest::{
    parse_manifest, select_manifest_entry, ManifestEntry, TokenizerManifest,
};
use crate::services::compaction2::tokens::tiktoken::{
    create_tiktoken_tokenizer, PretokenizerName,
};

/// 命名 tokenizer 数据目录的环境变量。
pub const TOKENIZER_DIR_ENV: &str = "CCH_TOKENIZER_DIR";

/// 已加载的 tokenizer，与磁盘格式无关。
pub trait LoadedTokenizer: Send + Sync {
    fn id(&self) -> &str;
    fn family(&self) -> &str;
    fn count(&self, text: &str) -> usize;
    fn encode(&self, text: &str) -> Vec<i64>;
}

static TOKENIZER_CACHE: Lazy<Mutex<HashMap<String, Option<Arc<dyn LoadedTokenizer>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static MANIFEST_CACHE: Lazy<Mutex<HashMap<String, Option<TokenizerManifest>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// TS `getTokenizerDir()`：已配置的数据目录，未设置返回 None。
pub fn get_tokenizer_dir() -> Option<PathBuf> {
    let raw = std::env::var(TOKENIZER_DIR_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// TS `resetTokenizerCache()`：丢弃全部缓存。
pub fn reset_tokenizer_cache() {
    if let Ok(mut cache) = TOKENIZER_CACHE.lock() {
        cache.clear();
    }
    if let Ok(mut cache) = MANIFEST_CACHE.lock() {
        cache.clear();
    }
}

/// TS `loadManifest(dir)`：加载并校验 `<dir>/manifest.json`。
pub fn load_manifest(dir: &Path) -> Option<TokenizerManifest> {
    let key = dir.to_string_lossy().to_string();
    if let Ok(cache) = MANIFEST_CACHE.lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }
    let path = dir.join("manifest.json");
    let manifest = (|| {
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        let raw: serde_json::Value = serde_json::from_str(&content).ok()?;
        parse_manifest(&raw).ok()
    })();
    if let Ok(mut cache) = MANIFEST_CACHE.lock() {
        cache.insert(key, manifest.clone());
    }
    manifest
}

fn load_entry(dir: &Path, entry: &ManifestEntry) -> Option<Arc<dyn LoadedTokenizer>> {
    let cache_key = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        dir.display(),
        entry.id,
        entry.format,
        entry.file,
        entry.pretokenizer.as_deref().unwrap_or_default()
    );
    {
        if let Ok(cache) = TOKENIZER_CACHE.lock() {
            if let Some(cached) = cache.get(&cache_key) {
                return cached.clone();
            }
        }
    }

    // manifest 路径是不可信输入；永远不读到数据目录之外。
    let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let file_path = dir.join(&entry.file);
    let canonical = file_path.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        if let Ok(mut cache) = TOKENIZER_CACHE.lock() {
            cache.insert(cache_key, None);
        }
        return None;
    }
    let content = fs::read_to_string(&canonical).ok()?;

    let loaded: Option<Box<dyn LoadedTokenizer>> = match entry.format.as_str() {
        "hf" => create_hf_tokenizer(&entry.id, &content)
            .ok()
            .map(|tokenizer| Box::new(tokenizer) as Box<dyn LoadedTokenizer>),
        _ => {
            let pretokenizer = entry.pretokenizer.as_deref().unwrap_or("cl100k");
            let pretokenizer = match pretokenizer {
                "o200k" => PretokenizerName::O200k,
                "kimi" => PretokenizerName::Kimi,
                _ => PretokenizerName::Cl100k,
            };
            create_tiktoken_tokenizer(&entry.id, &content, pretokenizer)
                .ok()
                .map(|tokenizer| Box::new(tokenizer) as Box<dyn LoadedTokenizer>)
        }
    };

    match loaded {
        Some(loaded) => {
            let loaded: Arc<dyn LoadedTokenizer> = Arc::from(loaded);
            if let Ok(mut cache) = TOKENIZER_CACHE.lock() {
                cache.insert(cache_key, Some(loaded.clone()));
            }
            Some(loaded)
        }
        None => {
            if let Ok(mut cache) = TOKENIZER_CACHE.lock() {
                cache.insert(cache_key, None);
            }
            None
        }
    }
}

/// TS `resolveTokenizer(model)`：从已配置数据目录解析模型的 tokenizer。
/// 任何环节缺失返回 None；调用方必须回落。
pub fn resolve_tokenizer(model: &str) -> Option<Arc<dyn LoadedTokenizer>> {
    let dir = get_tokenizer_dir()?;
    let manifest = load_manifest(&dir)?;
    let entry = select_manifest_entry(&manifest, model)?;
    load_entry(&dir, &entry)
}

/// TS `resolveTokenizerById(id)`：按 tokenizer id 解析（测试/工具用）。
pub fn resolve_tokenizer_by_id(id: &str) -> Option<Arc<dyn LoadedTokenizer>> {
    let dir = get_tokenizer_dir()?;
    let manifest = load_manifest(&dir)?;
    let entry = manifest
        .tokenizers
        .iter()
        .find(|candidate| candidate.id == id)?;
    load_entry(&dir, entry)
}
