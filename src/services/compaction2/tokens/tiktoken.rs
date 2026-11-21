//! `services/compaction2/tokens/tiktoken.ts` — tiktoken 格式 BPE tokenizer
//! （cl100k_base / o200k_base / kimi）。
//!
//! 文件格式每行一对 `base64(token bytes) rank`——正是 `openai/tiktoken`
//! 下载的格式，也是 oh-my-pi 为 Kimi 附带的格式。合并优先级即 rank：
//! tiktoken 的 `byte_pair_merge` 算法反复合并拼接后 rank 最低的相邻对。
//!
//! 预分词与词族相关、且 tiktoken 文件中没有表示，因此词族（cl100k /
//! o200k / kimi）由 manifest 条目提供。模式是参考正则的手工移植；Rust
//! `regex` crate 支持 `\p{Lu}` 等 Unicode 属性转义。
//!
//! 语义对齐注记：
//! - JS 正则的 `u` flag + lastIndexed exec 循环对应 Rust 的
//!   `find_iter`（无状态），配合 `split_by_regex` 复刻 HF `Split`
//!   的"匹配段 + 未匹配间隙"语义。
//! - Kimi 的 `[\p{Lu}...&&[^\p{Han}]]` 类交集在 Rust `regex` 不支持；
//!   改为在 Han/非 Han 段边界内侧跑去除 Han 减法的同一模式——Han 字符
//!   只能由第一分支 `\p{Script=Han}+` 匹配，故分段等价。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::services::compaction2::tokens::registry::LoadedTokenizer;

/// TS `PretokenizerName`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PretokenizerName {
    Cl100k,
    O200k,
    Kimi,
}

impl PretokenizerName {
    pub fn as_str(&self) -> &'static str {
        match self {
            PretokenizerName::Cl100k => "cl100k",
            PretokenizerName::O200k => "o200k",
            PretokenizerName::Kimi => "kimi",
        }
    }
}

/// 大小写不敏感的缩写后缀 `(?i:'s|'t|'re|'ve|'m|'ll|'d)`（手工展开）。
const CONTRACTION_CI: &str = "(?:'[sS]|'[tT]|'[rR][eE]|'[vV][eE]|'[mM]|'[lL][lL]|'[dD])";

const UPPER: &str = r"[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]";
const LOWER: &str = r"[\p{Ll}\p{Lm}\p{Lo}\p{M}]";

const CL100K_PATTERN: &str = concat!(
    r"(?:'[sS]|'[tT]|'[rR][eE]|'[vV][eE]|'[mM]|'[lL][lL]|'[dD])",
    r"|[^\r\n\p{L}\p{N}]?\p{L}+",
    r"|\p{N}{1,3}",
    r"| ?[^\s\p{L}\p{N}]+[\r\n]*",
    r"|\s*[\r\n]+|\s+(?!\S)|\s+"
);

const O200K_PATTERN: &str = concat!(
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+(?:'[sS]|'[tT]|'[rR][eE]|'[vV][eE]|'[mM]|'[lL][lL]|'[dD])?",
    r"|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*(?:'[sS]|'[tT]|'[rR][eE]|'[vV][eE]|'[mM]|'[lL][lL]|'[dD])?",
    r"|\p{N}{1,3}",
    r"| ?[^\s\p{L}\p{N}]+[\r\n/]*",
    r"|\s*[\r\n]+|\s+(?!\S)|\s+"
);

/// TS `PRETOKENIZERS`。
static PRETOKENIZER_CL100K: Lazy<Regex> = Lazy::new(|| compile_or_error(CL100K_PATTERN));
static PRETOKENIZER_O200K: Lazy<Regex> = Lazy::new(|| compile_or_error(O200K_PATTERN));
static HAN_RUN: Lazy<Regex> = Lazy::new(|| compile_or_error(r"\p{Script=Han}+"));
static KIMI_NONHAN: Lazy<Regex> = Lazy::new(|| {
    // Kimi 模式的非 Han 段形态：与 KIMI_PATTERN 相同，UPPER_NO_HAN /
    // LOWER_NO_HAN 回退为 UPPER/LOWER（段内无 Han 字符，减法无副作用）。
    compile_or_error(&format!(
        "{contraction}|[^\\r\\n\\p{{L}}\\p{{N}}]?{UPPER}*{LOWER}+{contraction}?|[^\\r\\n\\p{{L}}\\p{{N}}]?{UPPER}+{LOWER}*{contraction}?|\\p{{N}}{{1,3}}| ?[^\\s\\p{{L}}\\p{{N}}]+[\\r\\n]*|\\s*[\\r\\n]+|\\s+(?!\\S)|\\s+",
        contraction = CONTRACTION_CI
    ))
});

/// 编译预分词正则；内层 unwrap 针对常量字面量，运行时不可能触发。
fn compile_or_error(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|_| Regex::new("$^").unwrap())
}

/// TS `splitByRegex(re, text)`：HF `Split(Isolated)` 语义——每个匹配成为
/// 一片，未匹配间隙也保留为片。模式永不匹配空串（空匹配是 bug）。
pub fn split_by_regex<'a>(re: &Regex, text: &'a str) -> Vec<&'a str> {
    let mut pieces: Vec<&str> = Vec::new();
    let mut last = 0usize;
    for mat in re.find_iter(text) {
        let (start, end) = (mat.start(), mat.end());
        if end == start {
            continue;
        }
        if start > last {
            pieces.push(&text[last..start]);
        }
        pieces.push(&text[start..end]);
        last = end;
    }
    if last < text.len() {
        pieces.push(&text[last..]);
    }
    pieces
}

/// `utf8ToLatin1`：把 UTF-8 字节当作 latin-1 字符查看（Rust 侧以
/// `Vec<char>` 表达——每 char 一个字节，超出 UTF-8 编码但语义等价）。
fn utf8_to_latin1(text: &str) -> Vec<char> {
    text.bytes().map(|byte| byte as char).collect()
}

/// TS `parseTiktokenVocab(content)`：解码 tiktoken 词表为
/// token-bytes → rank 映射。
pub fn parse_tiktoken_vocab(content: &str) -> Result<HashMap<String, i64>, String> {
    let mut ranks = HashMap::new();
    for line in content.split('\n') {
        if line.is_empty() {
            continue;
        }
        let Some(space) = line.find(' ') else {
            continue;
        };
        if space == 0 {
            continue;
        }
        let rank = match line[space + 1..].parse::<i64>() {
            Ok(rank) if rank >= 0 => rank,
            _ => continue,
        };
        let token_bytes = base64_decode_standard(&line[..space])?;
        let token: String = token_bytes.iter().map(|byte| *byte as char).collect();
        ranks.insert(token, rank);
    }
    Ok(ranks)
}

/// 标准 base64 解码（无填充容错）。tiktoken 词表与 contextCount 的图片
/// 头部解析共用。
pub fn base64_decode_standard(input: &str) -> Result<Vec<u8>, String> {
    let table: Vec<u8> = {
        let mut table = vec![255u8; 256];
        for (index, byte) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
            .iter()
            .enumerate()
        {
            table[*byte as usize] = index as u8;
        }
        table
    };
    let mut decoded = Vec::with_capacity(input.len() * 3 / 4 + 1);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let value = table[ch as usize];
        if value == 255 {
            return Err(format!("base64: invalid character 0x{ch:02x}"));
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(decoded)
}

const INFINITY_RANK: i64 = i64::MAX;

fn get_rank(
    parts: &[(usize, i64)],
    k: usize,
    piece: &[char],
    rank_of: &dyn Fn(&str) -> Option<i64>,
) -> i64 {
    if k + 3 >= parts.len() {
        return INFINITY_RANK;
    }
    let span: String = piece[parts[k].0..parts[k + 3].0].iter().collect();
    rank_of(&span).unwrap_or(INFINITY_RANK)
}

/// tiktoken 的 `byte_pair_merge`：反复合并相邻 pair 中 rank 最低者，
/// 返回最终 `[start, end)` 片段。线性重扫是参考算法；预分词后的片段
/// 很短，是正确的取舍。
fn merge_spans(piece: &[char], rank_of: &dyn Fn(&str) -> Option<i64>) -> Vec<(usize, usize)> {
    let len = piece.len();
    if len == 0 {
        return Vec::new();
    }
    if len == 1 {
        return vec![(0, 1)];
    }

    // parts[k] = (起始偏移, part k 与 part k+1 合并的 rank)。
    let mut parts: Vec<(usize, i64)> = Vec::with_capacity(len + 2);
    let mut min_rank = INFINITY_RANK;
    let mut min_index: isize = -1;
    for i in 0..len - 1 {
        let span: String = piece[i..i + 2].iter().collect();
        let rank = rank_of(&span).unwrap_or(INFINITY_RANK);
        if rank < min_rank {
            min_rank = rank;
            min_index = i as isize;
        }
        parts.push((i, rank));
    }
    parts.push((len - 1, INFINITY_RANK));
    parts.push((len, INFINITY_RANK));

    while min_rank != INFINITY_RANK {
        let i = min_index as usize;
        if i > 0 {
            parts[i - 1].1 = get_rank(&parts, i - 1, piece, rank_of);
        }
        parts[i].1 = get_rank(&parts, i, piece, rank_of);
        parts.remove(i + 1);

        min_rank = INFINITY_RANK;
        min_index = -1;
        for k in 0..parts.len().saturating_sub(1) {
            if parts[k].1 < min_rank {
                min_rank = parts[k].1;
                min_index = k as isize;
            }
        }
    }

    let mut spans = Vec::with_capacity(parts.len().saturating_sub(1));
    for k in 0..parts.len().saturating_sub(1) {
        spans.push((parts[k].0, parts[k + 1].0));
    }
    spans
}

/// tiktoken 格式的已加载 tokenizer。`encode` 返回 token rank（词表 id）
/// 供调试/夹具；`count` 不物化 id 列表。
pub struct TiktokenTokenizer {
    id: String,
    family: PretokenizerName,
    ranks: HashMap<String, i64>,
}

impl TiktokenTokenizer {
    fn rank_of(&self, span: &str) -> Option<i64> {
        self.ranks.get(span).copied()
    }

    fn encode_piece(&self, piece: &str) -> Vec<i64> {
        let bytes = utf8_to_latin1(piece);
        let mut ids = Vec::new();
        let rank_of = |span: &str| self.rank_of(span);
        for (start, end) in merge_spans(&bytes, &rank_of) {
            let span: String = bytes[start..end].iter().collect();
            if let Some(rank) = self.ranks.get(&span) {
                ids.push(*rank);
            }
        }
        ids
    }

    /// text 的 token id（普通编码，无 special token）。
    pub fn encode_text(&self, text: &str) -> Vec<i64> {
        let mut ids = Vec::new();
        for piece in self.split(text) {
            ids.extend_from_slice(&self.encode_piece(piece));
        }
        ids
    }

    /// text 的 token 数（不物化 id 列表）。
    pub fn count_text(&self, text: &str) -> usize {
        let mut total = 0usize;
        for piece in self.split(text) {
            let bytes = utf8_to_latin1(piece);
            let rank_of = |span: &str| self.rank_of(span);
            total += merge_spans(&bytes, &rank_of).len();
        }
        total
    }

    fn split<'a>(&self, text: &'a str) -> Vec<&'a str> {
        match self.family {
            PretokenizerName::Cl100k => split_by_regex(&PRETOKENIZER_CL100K, text),
            PretokenizerName::O200k => split_by_regex(&PRETOKENIZER_O200K, text),
            PretokenizerName::Kimi => kimi_split(text),
        }
    }
}

/// Kimi 分词：Han 连读段与非 Han 段分别处理。Han 只能由 `\p{Script=Han}+`
/// 分支匹配，类交集减法在非 Han 段内为无操作，故分段等价。
fn kimi_split(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut last = 0usize;
    for mat in HAN_RUN.find_iter(text) {
        if mat.start() > last {
            pieces.extend(split_by_regex(&KIMI_NONHAN, &text[last..mat.start()]));
        }
        pieces.push(&text[mat.start()..mat.end()]);
        last = mat.end();
    }
    if last < text.len() {
        pieces.extend(split_by_regex(&KIMI_NONHAN, &text[last..]));
    }
    pieces
}

impl LoadedTokenizer for TiktokenTokenizer {
    fn id(&self) -> &str {
        &self.id
    }

    fn family(&self) -> &str {
        // manifest/container 以字符串携带词族。
        match self.family {
            PretokenizerName::Cl100k => "cl100k",
            PretokenizerName::O200k => "o200k",
            PretokenizerName::Kimi => "kimi",
        }
    }

    fn count(&self, text: &str) -> usize {
        self.count_text(text)
    }

    fn encode(&self, text: &str) -> Vec<i64> {
        self.encode_text(text)
    }
}

/// TS `createTiktokenTokenizer(id, content, pretokenizer)`。
pub fn create_tiktoken_tokenizer(
    id: &str,
    content: &str,
    pretokenizer: PretokenizerName,
) -> Result<TiktokenTokenizer, String> {
    let ranks = parse_tiktoken_vocab(content)?;
    if ranks.is_empty() {
        return Err(format!("tiktoken: {id} has no vocabulary entries"));
    }
    Ok(TiktokenTokenizer {
        id: id.to_string(),
        family: pretokenizer,
        ranks,
    })
}
