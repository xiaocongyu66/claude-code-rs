//! `services/compaction2/tokens/hfTokenizer.ts` — HuggingFace `tokenizer.json`
//! BPE tokenizer（DeepSeek / GLM / Qwen …）。
//!
//! 支持的形态：`model.type === "BPE"`、字节级词表键、可选的
//! NFC/NFD/NFKC/NFKD normalizer、由 `Split` 阶段（HF `Isolated` 语义）
//! 可选接 `ByteLevel` 组成的 pre-tokenizer。词表键保留在 GPT-2 字节级
//! 字母表（与 HF 存储一致），合并在同一字符串上跑（同参考实现）。
//! `ignore_merges`（GLM-5）生效。
//!
//! `Split` 模式从文件读取而非硬编码。Rust 移植注记：
//! - JS 的 `(?i:...)` 内联标志由本端口显式展开为大小写字符类；
//! - `\p{Han}` → `\p{Script=Han}`；
//! - 类交集（`&&[^...]`）在 Rust `regex` 不支持；`translate_hf_pattern`
//!   负责改写为等价形态，无法改写时返回错误，注册表回落到
//!   cl100k/adaptive。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::services::compaction2::tokens::manifest::{
    detect_family_from_tokenizer_json, extract_split_patterns,
};
use crate::services::compaction2::tokens::registry::LoadedTokenizer;
use crate::services::compaction2::tokens::tiktoken::split_by_regex;

const MERGE_RANK_INFINITY: usize = usize::MAX;

/// 构建 GPT-2 `bytes_to_unicode` 表（字节 → 映射字符）。
fn build_bytes_to_unicode() -> Vec<char> {
    let mut bytes: Vec<u32> = Vec::with_capacity(219);
    for byte in 0x21..=0x7e {
        bytes.push(byte);
    }
    for byte in 0xa1..=0xac {
        bytes.push(byte);
    }
    for byte in 0xae..=0xff {
        bytes.push(byte);
    }
    let mut codepoints = bytes.clone();
    let mut seen: Vec<bool> = vec![false; 256];
    for &byte in &bytes {
        seen[byte as usize] = true;
    }
    let mut n = 0u32;
    for byte in 0..256u32 {
        if !seen[byte as usize] {
            bytes.push(byte);
            codepoints.push(256 + n);
            n += 1;
        }
    }
    let mut table = vec!['\0'; 256];
    for index in 0..bytes.len() {
        table[bytes[index] as usize] =
            char::from_u32(codepoints[index]).unwrap_or('\u{fffd}');
    }
    table
}

static BYTES_TO_UNICODE: Lazy<Vec<char>> = Lazy::new(build_bytes_to_unicode);

/// TS `byteLevelEncode(text)`：把文本的 UTF-8 字节映射到字节级字母表。
pub fn byte_level_encode(text: &str) -> String {
    let mut chars = String::with_capacity(text.len());
    for byte in text.bytes() {
        chars.push(BYTES_TO_UNICODE[byte as usize]);
    }
    chars
}

/// TS `translateHfPattern(pattern)`：把 HF 正则改写为 Rust 兼容形式。
/// 只处理受支持词族用到的构造；调用方把抛错视作"不支持的 tokenizer"。
pub fn translate_hf_pattern(pattern: &str) -> Result<String, String> {
    // `(?i:...)` → 显式大小写字符类（内层无嵌套小括号）。
    let inline_flag = regex::Regex::new(r"\(\?i:((?:[^()\\]|\\.)*)\)").map_err(|e| e.to_string())?;
    let mut out = pattern.to_string();
    loop {
        let (whole, replacement): (String, String) = {
            let Some(captures) = inline_flag.captures(&out) else {
                break;
            };
            let whole = captures
                .get(0)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let inner = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            let mut expanded = String::with_capacity(inner.len() * 4);
            for ch in inner.chars() {
                if ch.is_ascii_alphabetic() {
                    expanded.push('[');
                    expanded.push(ch.to_ascii_lowercase());
                    expanded.push(ch.to_ascii_uppercase());
                    expanded.push(']');
                } else {
                    expanded.push(ch);
                }
            }
            (whole, format!("(?:{expanded})"))
        };
        out = out.replace(&whole, &replacement);
    }
    if out.contains("(?i:") {
        return Err("hf tokenizer: unsupported nested inline flags".to_string());
    }
    let out = out.replace("\\p{Han}", "\\p{Script=Han}");
    // `[A&&[^B]]`（单码点类交集）→ `((?!(?:B))[A])`。
    // 注：Rust `regex` crate 不支持 lookahead，含此构造的译文会编译
    // 失败——`compile_splitters` 把编译失败视作"不支持该 tokenizer"，
    // 注册表回落 cl100k/adaptive（与 TS 抛错回落同一效果）。
    let intersection =
        regex::Regex::new(r"\[([^\[\]]*)&&\[\^([^\[\]]*)\]\]").map_err(|e| e.to_string())?;
    let mut replaced = out;
    loop {
        let (whole, replacement): (String, String) = {
            let Some(captures) = intersection.captures(&replaced) else {
                break;
            };
            let whole = captures
                .get(0)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let class_a = captures.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
            let class_b = captures.get(2).map(|m| m.as_str()).unwrap_or_default().to_string();
            (
                whole,
                format!("((?!(?:{class_b}))[{class_a}])"),
            )
        };
        replaced = replaced.replace(&whole, &replacement);
    }
    Ok(replaced)
}

/// 拒绝无法精确复现的 pre-tokenizer 阶段，而不是用错误规则静默计费。
fn assert_supported_pre_tokenizer(node: Option<&Value>) -> Result<(), String> {
    let Some(node) = node else {
        return Ok(());
    };
    let Some(node) = node.as_object() else {
        return Err("hf tokenizer: malformed pre_tokenizer".to_string());
    };
    match node.get("type").and_then(Value::as_str) {
        Some("Sequence") => {
            let Some(children) = node.get("pretokenizers") else {
                return Err("hf tokenizer: malformed Sequence pre_tokenizer".to_string());
            };
            let Some(children) = children.as_array() else {
                return Err("hf tokenizer: malformed Sequence pre_tokenizer".to_string());
            };
            for child in children {
                assert_supported_pre_tokenizer(Some(child))?;
            }
            Ok(())
        }
        Some("Split") => {
            if let Some(behavior) = node.get("behavior").and_then(Value::as_str) {
                if behavior != "Isolated" {
                    return Err(format!(
                        "hf tokenizer: unsupported Split behavior {behavior}"
                    ));
                }
            }
            if node.get("invert").and_then(Value::as_bool) == Some(true) {
                return Err("hf tokenizer: inverted Split unsupported".to_string());
            }
            Ok(())
        }
        Some("ByteLevel") => {
            if node.get("use_regex").and_then(Value::as_bool) == Some(true) {
                return Err("hf tokenizer: ByteLevel use_regex unsupported".to_string());
            }
            Ok(())
        }
        Some(other) => Err(format!(
            "hf tokenizer: unsupported pre_tokenizer {other}"
        )),
        None => Err("hf tokenizer: malformed pre_tokenizer".to_string()),
    }
}

fn compile_splitters(pre_tokenizer: Option<&Value>) -> Result<Vec<Regex>, String> {
    assert_supported_pre_tokenizer(pre_tokenizer)?;
    let patterns = extract_split_patterns(pre_tokenizer);
    let mut splitters = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let translated = translate_hf_pattern(&pattern)?;
        let re = Regex::new(&translated)
            .map_err(|_| format!("hf tokenizer: unsupported split pattern {pattern}"))?;
        splitters.push(re);
    }
    Ok(splitters)
}

fn read_normalizer(node: Option<&Value>) -> Result<Option<&'static str>, String> {
    let Some(node) = node else {
        return Ok(None);
    };
    let Some(node) = node.as_object() else {
        return Err("hf tokenizer: malformed normalizer".to_string());
    };
    let Some(typ) = node.get("type").and_then(Value::as_str) else {
        return Err("hf tokenizer: normalizer without type".to_string());
    };
    match typ {
        "NFC" | "NFD" | "NFKC" | "NFKD" => Ok(Some(match typ {
            "NFC" => "NFC",
            "NFD" => "NFD",
            "NFKC" => "NFKC",
            _ => "NFKD",
        })),
        "Sequence" => {
            let Some(children) = node.get("normalizers").and_then(Value::as_array) else {
                return Err("hf tokenizer: malformed normalizer".to_string());
            };
            let mut found: Option<&'static str> = None;
            for child in children {
                let child_name = read_normalizer(Some(child))?;
                let Some(child_name) = child_name else {
                    continue;
                };
                if let Some(found) = found {
                    if found != child_name {
                        return Err(
                            "hf tokenizer: multiple normalizers unsupported".to_string()
                        );
                    }
                } else {
                    found = Some(child_name);
                }
            }
            Ok(found)
        }
        other => Err(format!("hf tokenizer: unsupported normalizer {other}")),
    }
}

fn normalize(text: &str, normalizer: Option<&str>) -> String {
    match normalizer {
        Some("NFC") => text.nfc().collect(),
        Some("NFD") => text.nfd().collect(),
        Some("NFKC") => text.nfkc().collect(),
        Some("NFKD") => text.nfkd().collect(),
        _ => text.to_string(),
    }
}

fn parse_merges(raw: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let Some(raw) = raw else {
        return Err("hf tokenizer: model.merges is missing".to_string());
    };
    let Some(merges) = raw.as_array() else {
        return Err("hf tokenizer: model.merges is missing".to_string());
    };
    let mut parsed = Vec::with_capacity(merges.len());
    for merge in merges {
        match merge {
            Value::Array(pair) if pair.len() == 2 => {
                let (Some(left), Some(right)) = (pair[0].as_str(), pair[1].as_str()) else {
                    return Err("hf tokenizer: malformed merge entry".to_string());
                };
                parsed.push((left.to_string(), right.to_string()));
            }
            Value::String(merge) => {
                let Some(space) = merge.find(' ') else {
                    return Err("hf tokenizer: malformed merge entry".to_string());
                };
                if space == 0 {
                    return Err("hf tokenizer: malformed merge entry".to_string());
                }
                parsed.push((merge[..space].to_string(), merge[space + 1..].to_string()));
            }
            _ => return Err("hf tokenizer: malformed merge entry".to_string()),
        }
    }
    Ok(parsed)
}

fn parse_vocab(raw: Option<&Value>) -> Result<HashMap<String, i64>, String> {
    let Some(raw) = raw else {
        return Err("hf tokenizer: model.vocab is missing".to_string());
    };
    let Some(vocab) = raw.as_object() else {
        return Err("hf tokenizer: model.vocab is missing".to_string());
    };
    let mut parsed = HashMap::with_capacity(vocab.len());
    for (token, id) in vocab {
        let id = id
            .as_i64()
            .ok_or_else(|| format!("hf tokenizer: bad id for token {token}"))?;
        parsed.insert(token.clone(), id);
    }
    if parsed.is_empty() {
        return Err("hf tokenizer: empty vocabulary".to_string());
    }
    Ok(parsed)
}

/// 合并键：字节级符号不含 U+0000，故作安全分隔符。
fn pair_key(left: &str, right: &str) -> String {
    format!("{left}\u{0}{right}")
}

/// HF BPE 合并循环：反复合并每一次出现的、合并 rank 最低的相邻对，
/// 返回最终符号。
fn merge_symbols(mapped: &str, pair_ranks: &HashMap<String, usize>) -> Vec<String> {
    let mut symbols: Vec<String> = mapped.chars().map(|ch| ch.to_string()).collect();
    while symbols.len() > 1 {
        let mut best_rank = MERGE_RANK_INFINITY;
        let mut best_left: Option<String> = None;
        let mut best_right: Option<String> = None;
        for index in 0..symbols.len() - 1 {
            if let Some(rank) = pair_ranks.get(&pair_key(&symbols[index], &symbols[index + 1])) {
                if *rank < best_rank {
                    best_rank = *rank;
                    best_left = Some(symbols[index].clone());
                    best_right = Some(symbols[index + 1].clone());
                }
            }
        }
        let (Some(best_left), Some(best_right)) = (best_left, best_right) else {
            break;
        };
        if best_rank == MERGE_RANK_INFINITY {
            break;
        }

        let merged = format!("{best_left}{best_right}");
        let mut next: Vec<String> = Vec::with_capacity(symbols.len());
        let mut index = 0;
        while index < symbols.len() {
            if index + 1 < symbols.len()
                && symbols[index] == best_left
                && symbols[index + 1] == best_right
            {
                next.push(merged.clone());
                index += 2;
            } else {
                next.push(symbols[index].clone());
                index += 1;
            }
        }
        symbols = next;
    }
    symbols
}

/// 已加载的 HuggingFace BPE tokenizer。
pub struct HfTokenizer {
    id: String,
    family: String,
    vocab: HashMap<String, i64>,
    pair_ranks: HashMap<String, usize>,
    splitters: Vec<Regex>,
    normalizer: Option<&'static str>,
    ignore_merges: bool,
}

impl HfTokenizer {
    fn split(&self, text: &str) -> Vec<String> {
        let normalized = normalize(text, self.normalizer);
        let mut pieces: Vec<String> = vec![normalized];
        for splitter in &self.splitters {
            let mut next: Vec<String> = Vec::new();
            for piece in &pieces {
                next.extend(split_by_regex(splitter, piece).iter().map(|s| s.to_string()));
            }
            pieces = next;
        }
        pieces
    }

    fn symbols(&self, piece: &str) -> Vec<String> {
        let mapped = byte_level_encode(piece);
        if self.ignore_merges && self.vocab.contains_key(&mapped) {
            return vec![mapped];
        }
        merge_symbols(&mapped, &self.pair_ranks)
    }

    /// text 的 token id（普通编码，无 added/special token）。
    pub fn encode_text(&self, text: &str) -> Result<Vec<i64>, String> {
        let mut ids = Vec::new();
        for piece in self.split(text) {
            for symbol in self.symbols(&piece) {
                let id = self
                    .vocab
                    .get(&symbol)
                    .ok_or_else(|| format!("hf tokenizer {}: symbol {symbol} not in vocab", self.id))?;
                ids.push(*id);
            }
        }
        Ok(ids)
    }

    /// text 的 token 数。
    pub fn count_text(&self, text: &str) -> usize {
        let mut total = 0usize;
        for piece in self.split(text) {
            total += self.symbols(&piece).len();
        }
        total
    }
}

impl LoadedTokenizer for HfTokenizer {
    fn id(&self) -> &str {
        &self.id
    }

    fn family(&self) -> &str {
        &self.family
    }

    fn count(&self, text: &str) -> usize {
        self.count_text(text)
    }

    fn encode(&self, text: &str) -> Vec<i64> {
        self.encode_text(text).unwrap_or_default()
    }
}

/// TS `createHfTokenizer(id, content)`。
pub fn create_hf_tokenizer(id: &str, content: &str) -> Result<HfTokenizer, String> {
    let json: Value = serde_json::from_str(content)
        .map_err(|_| format!("hf tokenizer: {id} content is not valid JSON"))?;
    let model = json.get("model");
    let model_type_ok = model
        .and_then(|model| model.get("type"))
        .and_then(Value::as_str)
        == Some("BPE");
    if !model_type_ok {
        let model_type = model
            .and_then(|model| model.get("type"))
            .and_then(Value::as_str)
            .map(String::from);
        return Err(format!(
            "hf tokenizer: {} model.type is not BPE ({})",
            id,
            model_type.unwrap_or_else(|| "missing".to_string())
        ));
    }
    let vocab = parse_vocab(model.and_then(|model| model.get("vocab")))?;
    let mut pair_ranks = HashMap::new();
    for (rank, (left, right)) in parse_merges(model.and_then(|model| model.get("merges")))?
        .into_iter()
        .enumerate()
    {
        pair_ranks.insert(pair_key(&left, &right), rank);
    }
    let splitters = compile_splitters(json.get("pre_tokenizer"))?;
    let normalizer = read_normalizer(json.get("normalizer"))?;
    let ignore_merges = model
        .and_then(|model| model.get("ignore_merges"))
        .and_then(Value::as_bool)
        == Some(true);
    let family = detect_family_from_tokenizer_json(&json);
    Ok(HfTokenizer {
        id: id.to_string(),
        family: family.as_str().to_string(),
        vocab,
        pair_ranks,
        splitters,
        normalizer,
        ignore_merges,
    })
}
