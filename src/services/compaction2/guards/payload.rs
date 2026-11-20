//! `services/compaction2/guards/payload.rs` — 载荷拒绝护卫（oh-my-pi 平级端口）。
//!
//! HTTP 413 / 媒体预算拒绝不自动算作 token 溢出：只有 provider 上报的
//! 输入用量能证明时才算；否则阻断自动续跑（或窗口未知时仍允许尝试一个
//! 可运行的溢出方法），并排除会**增加**图片字节的 snapcompact。

use once_cell::sync::Lazy;
use regex::Regex;

/// 编译失败时回落"永不匹配"的模式。内层 unwrap 针对常量字面量
/// （`"$^"` 必然合法），运行时不可能触发。
fn compile_or_never(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|_| Regex::new("$^").unwrap())
}

/// 载荷形 413 与溢出共享文本模式；本地占用低于该上限（>=10% 余量）且
/// 上报用量在窗口内时，信任载荷分类。
pub const PAYLOAD_REJECTION_OCCUPANCY_CEILING: f64 = 0.9;

/// 窄匹配媒体上限证据：名词必须与计数/限额共现。
static PAYLOAD_MEDIA_LIMIT_EVIDENCE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    compile_or_never(
        r#"(?i)\btoo many (?:images?|frames?|pixels?)\b|\b(?:images?|frames?|pixels?)\s*(?:count|limit)\b|\blimit of \d+\s*(?:images?|frames?|pixels?)\b|\bmaximum(?: of \d+)? (?:images?|frames?|pixels?)\b|\b(?:number|count) of (?:images?|frames?|pixels?)\b|\b(?:images?|frames?|pixels?)\b.{0,20}\bexceeds?\b.{0,20}\bmaximum\b|\b(?:images?|frames?) (?:is |are )?too large\b|\b(?:images?|frames?) dimensions?\b.{0,30}\bexceeds?\b.{0,30}\b(?:pixels?|\d+)\b"#,
    )
    
});

/// TS `PayloadRejectionEvidence`。
#[derive(Debug, Clone, Default)]
pub struct PayloadRejectionEvidence {
    pub status: Option<i64>,
    pub message: Option<String>,
    /// 被拒请求的 provider 上报输入 token（已知时）。
    pub provider_input_tokens: Option<i64>,
    pub context_window: Option<i64>,
    /// 被拒请求的本地估算。
    pub token_count: Option<i64>,
    /// 会话中是否存在压缩轮可回收的历史。
    pub has_reclaimable_history: Option<bool>,
}

/// 文本同时证明 token 溢出的 413 不算纯载荷情形。
static TOKEN_OVERFLOW_TEXT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    compile_or_never(
        r#"(?i)\bprompt is too long\b|\bcontext length exceeded\b|\bmaximum context length\b|\b\d+\s*tokens?\s*>\s*\d+\b"#,
    )
    
});

static PAYLOAD_REJECTION_PATTERN: Lazy<Regex> = Lazy::new(|| {
    compile_or_never(
        r#"(?i)\b413\b|request(?:\s+entity)?\s+too\s+large|payload\s+too\s+large|request_too_large|too\s+many\s+(?:images?|frames?|pixels?)|image\s+count\s+exceeds"#,
    )
    
});

/// TS `PayloadGuardDecision`。
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadGuardDecision {
    pub is_payload_rejection: bool,
    pub media_evidence: bool,
    /// 仅当 provider 用量证明请求超出窗口时为 true。
    pub token_overflow_proven: bool,
    /// 为 true 时自动续跑/压缩不得执行。
    pub block_compaction: bool,
    /// 字节/媒体形拒绝：snapcompact 必须跳过。
    pub exclude_media_methods: bool,
    pub reason: String,
}

/// TS `hasExplicitMediaRejectionEvidence(message?)`。
pub fn has_explicit_media_rejection_evidence(message: Option<&str>) -> bool {
    match message {
        Some(message) => PAYLOAD_MEDIA_LIMIT_EVIDENCE_PATTERN.is_match(message),
        None => false,
    }
}

/// TS `isPayloadRejectionError(status?, message?)`。
pub fn is_payload_rejection_error(status: Option<i64>, message: Option<&str>) -> bool {
    if status == Some(413) {
        return true;
    }
    match message {
        None => false,
        Some(message) => PAYLOAD_REJECTION_PATTERN.is_match(message),
    }
}

/// TS `evaluatePayloadGuard(evidence)`。
pub fn evaluate_payload_guard(evidence: &PayloadRejectionEvidence) -> PayloadGuardDecision {
    let is_payload_rejection =
        is_payload_rejection_error(evidence.status, evidence.message.as_deref());
    let media_evidence =
        has_explicit_media_rejection_evidence(evidence.message.as_deref());
    let window_tokens = evidence.context_window.unwrap_or(0);
    let local_tokens = evidence.token_count.unwrap_or(0);
    let provider_tokens = evidence.provider_input_tokens;

    let token_overflow_proven = match evidence.message.as_deref() {
        Some(message) => TOKEN_OVERFLOW_TEXT_PATTERN.is_match(message),
        None => false,
    } || match (provider_tokens, window_tokens) {
        (Some(provider_tokens), window_tokens) if window_tokens > 0 => {
            provider_tokens > window_tokens
        }
        _ => false,
    };

    if !is_payload_rejection {
        return PayloadGuardDecision {
            is_payload_rejection: false,
            media_evidence,
            token_overflow_proven,
            block_compaction: false,
            exclude_media_methods: false,
            reason: "not-payload-rejection".to_string(),
        };
    }

    if token_overflow_proven {
        return PayloadGuardDecision {
            is_payload_rejection: true,
            media_evidence,
            token_overflow_proven,
            block_compaction: false,
            exclude_media_methods: media_evidence,
            reason: "usage-proven-token-overflow".to_string(),
        };
    }

    if media_evidence {
        return PayloadGuardDecision {
            is_payload_rejection: true,
            media_evidence,
            token_overflow_proven: false,
            block_compaction: true,
            exclude_media_methods: true,
            reason: "media-limit-evidence".to_string(),
        };
    }

    let occupancy = if window_tokens > 0 {
        local_tokens as f64 / window_tokens as f64
    } else {
        f64::INFINITY
    };
    let local_headroom = window_tokens > 0 && occupancy <= PAYLOAD_REJECTION_OCCUPANCY_CEILING;
    let usage_within_window = match (provider_tokens, window_tokens) {
        (Some(provider_tokens), window_tokens) if window_tokens > 0 => {
            provider_tokens <= window_tokens
        }
        (None, _) => true,
        _ => false,
    };

    if window_tokens > 0 && local_headroom && usage_within_window {
        let can_reclaim = evidence.has_reclaimable_history == Some(true);
        return PayloadGuardDecision {
            is_payload_rejection: true,
            media_evidence: false,
            token_overflow_proven: false,
            block_compaction: !can_reclaim,
            exclude_media_methods: true,
            reason: if can_reclaim {
                "payload-with-reclaimable-history".to_string()
            } else {
                "payload-budget-with-headroom".to_string()
            },
        };
    }

    // 未知窗口：维护轮仍可尝试一个可运行的溢出方法。
    PayloadGuardDecision {
        is_payload_rejection: true,
        media_evidence: false,
        token_overflow_proven: false,
        block_compaction: false,
        exclude_media_methods: true,
        reason: "unknown-window-attempt".to_string(),
    }
}
