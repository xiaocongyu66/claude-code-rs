//! `services/compaction2/tokens/index.ts` — 真实 tokenizer 层（模块根）。
//!
//! `estimateTokens` 在模型经 manifest 命中时用精确 tokenizer；否则优雅
//! 降级：先到原生 cl100k BPE（`token-counter-napi`，Rust 侧未接线），
//! 再到内容自适应估算。后两者都报 `exact: false`。

pub mod context_count;
pub mod hf_tokenizer;
pub mod manifest;
pub mod message_count;
pub mod registry;
pub mod tiktoken;

pub use manifest::{
    detect_family_from_patterns, detect_family_from_tokenizer_json, extract_split_patterns,
    parse_manifest, select_manifest_entry, ManifestEntry, TokenizerFamily, TokenizerManifest,
};
pub use registry::{
    get_tokenizer_dir, load_manifest, reset_tokenizer_cache, resolve_tokenizer,
    resolve_tokenizer_by_id, LoadedTokenizer, TokenizerDirEnv,
};
pub use tiktoken::{create_tiktoken_tokenizer, TiktokenTokenizer};

/// TS `TokenEstimate`。
#[derive(Debug, Clone, PartialEq)]
pub struct TokenEstimate {
    pub tokens: usize,
    /// tokenizer id，或降级策略名。
    pub tokenizer: String,
    /// 仅当使用了真实词表时为 true。
    pub exact: bool,
}

/// TS `contentAdaptiveEstimate(text)`：内容自适应降级估算。
///
/// 拉丁文本平均 ~4 字符/token；CJK 接近 1.5；其余介于其间。
pub fn content_adaptive_estimate(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut cjk = 0usize;
    let mut other = 0usize;
    for code_point in text.chars() {
        let code_point = code_point as u32;
        if code_point < 0x80 {
            ascii += 1;
        } else if is_cjk(code_point) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    let estimate = ascii as f64 / 4.0 + cjk as f64 / 1.5 + other as f64 / 3.0;
    std::cmp::max(1, estimate.round() as usize)
}

fn is_cjk(code_point: u32) -> bool {
    (0x3000..=0x30ff).contains(&code_point)
        || (0x3400..=0x4dbf).contains(&code_point)
        || (0x4e00..=0x9fff).contains(&code_point)
        || (0xac00..=0xd7af).contains(&code_point)
        || (0xf900..=0xfaff).contains(&code_point)
        || (0x20000..=0x2fa1f).contains(&code_point)
}

/// TS `estimateTokens(text, model)`。
///
/// 模型配置了 tokenizer 时计数对词表精确（`exact: true`）；否则返回
/// cl100k 原生计数或内容自适应估算，`exact: false`。
pub fn estimate_tokens(text: &str, model: &str) -> TokenEstimate {
    if text.is_empty() {
        return TokenEstimate {
            tokens: 0,
            tokenizer: "empty".to_string(),
            exact: true,
        };
    }

    let tokenizer = resolve_tokenizer(model);
    if let Some(tokenizer) = tokenizer {
        return TokenEstimate {
            tokens: tokenizer.count(text),
            tokenizer: tokenizer.id().to_string(),
            exact: true,
        };
    }

    // 原生 cl100k（token-counter-napi）在 Rust 侧未接线；直接走自适应
    // 估算（与 TS 的降级链同尾）。
    TokenEstimate {
        tokens: content_adaptive_estimate(text),
        tokenizer: "adaptive".to_string(),
        exact: false,
    }
}
