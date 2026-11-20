//! `services/compaction2/tokens/manifest.ts` — tokenizer manifest 与数据驱动的
//! 词族选择。
//!
//! tokenizer 数据目录（`CCH_TOKENIZER_DIR`）含 `manifest.json` 与词表文件。
//! 本模块不认识模型名：条目可选的 `models` glob 是 manifest 数据，HF
//! tokenizer 的词族从其自身 `pre_tokenizer` 模式推断；tiktoken 文件没有
//! 元数据，词族来自 manifest 的 `pretokenizer` 字段。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

/// TS `TokenizerFormat`。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenizerFormat {
    Tiktoken,
    Hf,
}

/// TS `TokenizerFamily`。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenizerFamily {
    Cl100k,
    O200k,
    Kimi,
    Deepseek,
    Qwen,
    Glm,
    Unknown,
}

impl TokenizerFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenizerFamily::Cl100k => "cl100k",
            TokenizerFamily::O200k => "o200k",
            TokenizerFamily::Kimi => "kimi",
            TokenizerFamily::Deepseek => "deepseek",
            TokenizerFamily::Qwen => "qwen",
            TokenizerFamily::Glm => "glm",
            TokenizerFamily::Unknown => "unknown",
        }
    }
}

/// TS `ManifestEntry`。
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    /// `estimateTokens` 报告的稳定 id。
    pub id: String,
    pub format: TokenizerFormat,
    /// 词表文件名，相对数据目录。
    pub file: String,
    /// tiktoken 文件必需；HF 忽略（模式在文件内）。
    pub pretokenizer: Option<String>,
    /// 可选覆盖；否则 HF 推断、tiktoken 复制。
    pub family: Option<TokenizerFamily>,
    /// 对模型 id 匹配的 glob 模式（数据，不是代码）。
    pub models: Option<Vec<String>>,
}

/// TS `TokenizerManifest`。
#[derive(Debug, Clone, PartialEq)]
pub struct TokenizerManifest {
    pub version: i64,
    pub tokenizers: Vec<ManifestEntry>,
}

const PRETOKENIZERS: [&str; 3] = ["cl100k", "o200k", "kimi"];

/// TS `parseManifest(raw)`：校验解析出的 `manifest.json`，非法数据返回错。
pub fn parse_manifest(raw: &Value) -> Result<TokenizerManifest, String> {
    let Some(doc) = raw.as_object() else {
        return Err("tokenizer manifest: expected an object".to_string());
    };
    let version = doc
        .get("version")
        .and_then(Value::as_i64)
        .ok_or("tokenizer manifest: missing numeric version")?;
    let entries = doc
        .get("tokenizers")
        .ok_or_else(|| "tokenizer manifest: missing tokenizers array".to_string())?;
    let Some(entries) = entries.as_array() else {
        return Err("tokenizer manifest: missing tokenizers array".to_string());
    };

    let mut tokenizers = Vec::with_capacity(entries.len());
    for (index, value) in entries.iter().enumerate() {
        let entry = value
            .as_object()
            .ok_or_else(|| format!("tokenizer manifest: entry {index} is not an object"))?;
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("tokenizer manifest: entry {index} has no id"))?
            .to_string();
        let format = match entry.get("format").and_then(Value::as_str) {
            Some("tiktoken") => TokenizerFormat::Tiktoken,
            Some("hf") => TokenizerFormat::Hf,
            _ => return Err(format!("tokenizer manifest: {id} has bad format")),
        };
        let file = entry
            .get("file")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
            .ok_or_else(|| format!("tokenizer manifest: {id} has no file"))?
            .to_string();
        let pretokenizer = match entry.get("pretokenizer") {
            Some(Value::String(pretokenizer)) => {
                if !PRETOKENIZERS.contains(&pretokenizer.as_str()) {
                    return Err(format!(
                        "tokenizer manifest: {id} has bad pretokenizer"
                    ));
                }
                Some(pretokenizer.clone())
            }
            Some(_) => {
                return Err(format!(
                    "tokenizer manifest: {id} has bad pretokenizer"
                ))
            }
            None => None,
        };
        if format == TokenizerFormat::Tiktoken && pretokenizer.is_none() {
            return Err(format!(
                "tokenizer manifest: tiktoken entry {id} needs a pretokenizer"
            ));
        }
        let models = match entry.get("models") {
            Some(Value::Array(models)) => {
                if models.iter().all(|model| model.is_string()) {
                    Some(
                        models
                            .iter()
                            .filter_map(|model| model.as_str().map(String::from))
                            .collect(),
                    )
                } else {
                    return Err(format!("tokenizer manifest: {id} has bad models"));
                }
            }
            Some(_) => return Err(format!("tokenizer manifest: {id} has bad models")),
            None => None,
        };
        let family = entry
            .get("family")
            .and_then(Value::as_str)
            .map(|family| match family {
                "cl100k" => TokenizerFamily::Cl100k,
                "o200k" => TokenizerFamily::O200k,
                "kimi" => TokenizerFamily::Kimi,
                "deepseek" => TokenizerFamily::Deepseek,
                "qwen" => TokenizerFamily::Qwen,
                "glm" => TokenizerFamily::Glm,
                _ => TokenizerFamily::Unknown,
            });
        tokenizers.push(ManifestEntry {
            id,
            format,
            file,
            pretokenizer,
            family,
            models,
        });
    }
    Ok(TokenizerManifest { version, tokenizers })
}

static GLOB_CACHE: Lazy<Mutex<HashMap<String, Result<Regex, String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// glob → 正则：`*` → `.*`、`?` → `.`，其余元字符转义；锚定 + 大小写
/// 不敏感（对齐 TS `new RegExp('^' + source + '$', 'i')`）。
fn glob_to_regexp(glob: &str) -> Result<Regex, String> {
    if let Ok(cache) = GLOB_CACHE.lock() {
        if let Some(cached) = cache.get(glob) {
            return cached.clone();
        }
    }
    let mut source = String::with_capacity(glob.len() * 2);
    for ch in glob.chars() {
        match ch {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                source.push('\\');
                source.push(ch);
            }
            '*' => source.push_str(".*"),
            '?' => source.push('.'),
            _ => source.push(ch),
        }
    }
    let re = Regex::new(&format!("(?i)^{source}$")).map_err(|e| e.to_string());
    if let Ok(mut cache) = GLOB_CACHE.lock() {
        cache.insert(glob.to_string(), re.clone());
    }
    re
}

/// TS `selectManifestEntry(manifest, model)`：取第一个 `models` glob 命中
/// 模型（完整 id 或最后一个 `/` 后的段）或 id 恰等模型的条目。
pub fn select_manifest_entry<'a>(
    manifest: &'a TokenizerManifest,
    model: &str,
) -> Option<&'a ManifestEntry> {
    let short = match model.rfind('/') {
        Some(slash) => &model[slash + 1..],
        None => model,
    };
    for entry in &manifest.tokenizers {
        if entry.id == model {
            return Some(entry);
        }
        let Some(models) = &entry.models else {
            continue;
        };
        if models.is_empty() {
            continue;
        }
        for pattern in models {
            if let Ok(re) = glob_to_regexp(pattern) {
                if re.is_match(model) || re.is_match(short) {
                    return Some(entry);
                }
            }
        }
    }
    None
}

/// HF pre_tokenizer 树节点（`Split` 的正则、`Sequence` 的子节点）。
fn as_node(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn split_pattern(node: &serde_json::Map<String, Value>) -> Option<String> {
    match node.get("pattern") {
        Some(Value::String(pattern)) => Some(pattern.clone()),
        Some(pattern) => pattern
            .get("Regex")
            .and_then(Value::as_str)
            .map(String::from),
        None => None,
    }
}

/// TS `extractSplitPatterns(preTokenizer)`：展平 HF `pre_tokenizer` 树中
/// 的 `Split` 正则。
pub fn extract_split_patterns(pre_tokenizer: Option<&Value>) -> Vec<String> {
    let Some(node) = pre_tokenizer.and_then(as_node) else {
        return Vec::new();
    };
    match node.get("type").and_then(Value::as_str) {
        Some("Sequence") => match node.get("pretokenizers") {
            Some(Value::Array(children)) => children
                .iter()
                .flat_map(|child| extract_split_patterns(Some(child)))
                .collect(),
            _ => Vec::new(),
        },
        Some("Split") => split_pattern(node).into_iter().collect(),
        _ => Vec::new(),
    }
}

static DEEPSEEK_DIGITS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\\p\{N\}\{1,3\}").unwrap_or_else(|_| Regex::new("$^").unwrap())
});
static QWEN_LETTERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\\p\{L\}\\p\{M\}\]\+").unwrap_or_else(|_| Regex::new("$^").unwrap())
});
static QWEN_DIGITS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\\p\{N\}(?!\{)").unwrap_or_else(|_| Regex::new("$^").unwrap())
});
static O200K_CLASS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\\p\{Lu\}\\p\{Lt\}\\p\{Lm\}\\p\{Lo\}\\p\{M\}\]")
        .unwrap_or_else(|_| Regex::new("$^").unwrap())
});
static GLM_LETTERS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\\p\{L\}\+").unwrap_or_else(|_| Regex::new("$^").unwrap()));
static GLM_DIGITS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\\p\{N\}\{1,3\}").unwrap_or_else(|_| Regex::new("$^").unwrap()));

/// TS `detectFamilyFromPatterns(patterns, ignoreMerges)`：从 pre-tokenizer
/// 的正则形状推断词族——数据驱动的词族选择：不看模型名，只看 tokenizer
/// 文件自己声明的模式。
pub fn detect_family_from_patterns(
    patterns: &[String],
    ignore_merges: bool,
) -> TokenizerFamily {
    let joined = patterns.join("\n");
    if patterns.len() >= 3 {
        let digits = &patterns[0];
        let cjk = &patterns[1];
        if DEEPSEEK_DIGITS.is_match(digits) && cjk.contains("一-龥") {
            return TokenizerFamily::Deepseek;
        }
    }
    if QWEN_LETTERS.is_match(&joined) && QWEN_DIGITS.is_match(&joined) {
        return TokenizerFamily::Qwen;
    }
    if joined.contains("\\p{Han}") || joined.contains("一-龥") {
        return TokenizerFamily::Kimi;
    }
    if O200K_CLASS.is_match(&joined) {
        return TokenizerFamily::O200k;
    }
    if GLM_LETTERS.is_match(&joined) && GLM_DIGITS.is_match(&joined) {
        return if ignore_merges {
            TokenizerFamily::Glm
        } else {
            TokenizerFamily::Cl100k
        };
    }
    TokenizerFamily::Unknown
}

/// TS `detectFamilyFromTokenizerJson(json)`。
pub fn detect_family_from_tokenizer_json(json: &Value) -> TokenizerFamily {
    let Some(doc) = json.as_object() else {
        return TokenizerFamily::Unknown;
    };
    let ignore_merges = doc
        .get("model")
        .and_then(as_node)
        .and_then(|model| model.get("ignore_merges"))
        .and_then(Value::as_bool)
        == Some(true);
    detect_family_from_patterns(
        &extract_split_patterns(doc.get("pre_tokenizer")),
        ignore_merges,
    )
}
