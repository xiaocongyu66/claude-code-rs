//! `services/compaction2/methods/snapcompact/index.ts` — snapcompact 压缩
//! 方法（W4，移植自 oh-my-pi，MIT）：把丢弃的历史归档为视觉模型可读回的
//! 位图 PNG 帧。整个过程本地且确定——不调 LLM、不调 API、不走网络。
//!
//! 当归档主要是非 ASCII（序列化字符的 >5%）时，内置的 ASCII-only 位图
//! 字体会把它渲染成 `?` 噪声。此时方法不跳过，而是回落为文本归档：
//! 同样的切点与序列化预算，但只保留头/尾文本（没有 PNG 帧）。消费方按
//! 持久化的 `mode` 分支，无帧时只用 `text`。
//!
//! 门控（CCH 没有可靠的 modality 元数据）：
//! - 必须设置 `CCH_SNAPCOMPACT_ENABLE=1`，且
//! - 调用方必须显式声明支持图片输入（`declaresImageInput`），且
//! - 必须显式指明 provider/API family（deps 或
//!   `CCH_SNAPCOMPACT_FAMILY`），以便选择帧形状与价格。
//!
//! 任何一项未解析都使 `isAvailable` 为 false；orchestrator 随即 fall
//! through 到下一个配置的方法。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub mod font;
pub mod planner;
pub mod png;
pub mod serializer;

use serde_json::{Map, Value};

use crate::services::compact::{now_ts, uuid_v4, Message, SystemMessage};
use crate::services::compaction2::contracts::{
    CompactionContext, CompactionMethod, CompactionMethodName, CompactionOutcome, CutPoint,
};
use crate::services::compaction2::methods::snapcompact::font::{five_by_eight_font, FontProvider};
use crate::services::compaction2::methods::snapcompact::planner::{
    frame_data_bytes, plan_archive, resolve_profile, ArchivePlan,
    BudgetOptions, FamilyProfile, FramePlan, ProviderFamily,
};
use crate::services::compaction2::methods::snapcompact::png::{render_frame, RenderedFrame};
use crate::services::compaction2::methods::snapcompact::serializer::{
    normalize_for_frame, serialize_conversation, SerializeOptions,
};

/// TS `SnapcompactMethodDeps`。
pub struct SnapcompactMethodDeps {
    /// 环境映射；默认 `process.env`。测试显式传映射。
    pub env: Option<HashMap<String, Option<String>>>,
    /// 活跃模型接受图片输入的显式声明。CCH 没有可信的 modality 元数据，
    /// 因此默认 false。
    pub declares_image_input: Option<Box<dyn Fn(&CompactionContext) -> bool + Send + Sync>>,
    /// 显式 provider/API family；优先于 `CCH_SNAPCOMPACT_FAMILY`。
    pub provider_family:
        Option<Box<dyn Fn(&CompactionContext) -> Option<ProviderFamily> + Send + Sync>>,
    /// 显式形状名；优先于 `CCH_SNAPCOMPACT_SHAPE`。
    pub shape: Option<String>,
    /// 显式帧边长（像素）。
    pub frame_size: Option<i64>,
    /// 光栅化字形所用字体；默认公共领域 5x8 字体。
    pub font: Option<Box<dyn FontProvider>>,
    /// 调用方的帧数上限（仍钳制到 80 与各预算）。
    pub max_frames: Option<usize>,
    /// 归档可用的 token 预算。
    pub token_budget: Option<Box<dyn Fn(&CompactionContext) -> Option<i64> + Send + Sync>>,
    /// base64 载荷预算。
    pub data_budget_bytes: Option<i64>,
    /// 序列化预算。
    pub serialize_options: Option<SerializeOptions>,
    /// 随归档存储的短恢复引导语。
    pub summary: Option<String>,
}

/// TS `SNAPCOMPACT_PRESERVE_KEY`：`CompactionOutcome.preserveData` 下的
/// 持久化归档键。
pub const SNAPCOMPACT_PRESERVE_KEY: &str = "snapcompact";

/// TS `SnapcompactArchiveBase`。
#[derive(Debug, Clone, serde::Serialize)]
struct SnapcompactArchiveBase {
    version: u32,
    #[serde(rename = "totalChars")]
    total_chars: usize,
    #[serde(rename = "truncatedChars")]
    truncated_chars: usize,
    /// 后续压缩时重渲染的有界源文本。
    text: String,
    #[serde(rename = "textHead")]
    text_head: String,
    #[serde(rename = "textTail")]
    text_tail: String,
    summary: String,
}

/// TS `SnapcompactImageArchive`。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapcompactImageArchive {
    #[serde(flatten)]
    base: SnapcompactArchiveBase,
    mode: &'static str,
    family: ProviderFamily,
    shape: &'static str,
    #[serde(rename = "frameSize")]
    frame_size: usize,
    frames: Vec<RenderedFrame>,
}

/// TS `SnapcompactTextArchive`：位图字体无法渲染的归档的纯文本回落。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapcompactTextArchive {
    #[serde(flatten)]
    base: SnapcompactArchiveBase,
    mode: &'static str,
}

/// TS `SnapcompactArchive`。
#[derive(Debug, Clone)]
pub enum SnapcompactArchive {
    Image(SnapcompactImageArchive),
    Text(SnapcompactTextArchive),
}

fn default_env() -> HashMap<String, Option<String>> {
    let keys = [
        "CCH_SNAPCOMPACT_ENABLE",
        "CCH_SNAPCOMPACT_FAMILY",
        "CCH_SNAPCOMPACT_API_FAMILY",
        "CCH_SNAPCOMPACT_SHAPE",
        "CCH_SNAPCOMPACT_FRAME_SIZE",
        "CCH_SNAPCOMPACT_FRAME_TOKENS",
        "CCH_SNAPCOMPACT_IMAGE_BUDGET",
    ];
    let mut env = HashMap::new();
    for key in keys {
        env.insert(key.to_string(), std::env::var(key).ok());
    }
    env
}

fn resolve_env(deps: &SnapcompactMethodDeps) -> HashMap<String, Option<String>> {
    deps.env.clone().unwrap_or_else(default_env)
}

fn declared_image_input(deps: &SnapcompactMethodDeps, ctx: &CompactionContext) -> bool {
    deps.declares_image_input
        .as_ref()
        .map(|declared| declared(ctx))
        .unwrap_or(false)
}

fn resolve_family(deps: &SnapcompactMethodDeps, ctx: &CompactionContext) -> Option<ProviderFamily> {
    deps.provider_family
        .as_ref()
        .and_then(|family| family(ctx))
}

fn resolve_profile_for(
    deps: &SnapcompactMethodDeps,
    ctx: &CompactionContext,
) -> Option<FamilyProfile> {
    resolve_profile(&crate::services::compaction2::methods::snapcompact::planner::ProfileInput {
        env: Some(&resolve_env(deps)),
        family: resolve_family(deps, ctx),
        shape: deps.shape.as_deref(),
        frame_size: deps.frame_size,
    })
}

fn enabled(deps: &SnapcompactMethodDeps) -> bool {
    resolve_env(deps)
        .get("CCH_SNAPCOMPACT_ENABLE")
        .and_then(|value| value.clone())
        .as_deref()
        == Some("1")
}

fn resolve_token_budget(
    deps: &SnapcompactMethodDeps,
    ctx: &CompactionContext,
) -> Option<i64> {
    deps.token_budget
        .as_ref()
        .and_then(|budget| budget(ctx))
}

/// TS `selectCut(ctx)`。
///
/// 选切点：保留 token 计数能放进 `keepRecentTokens` 的最新后缀，且至少
/// 保留最后一条消息。
fn select_cut(ctx: &CompactionContext) -> usize {
    let messages = &ctx.messages;
    if messages.is_empty() {
        return 0;
    }
    let keep = ctx.settings.keep_recent_tokens.max(0);
    let mut low = 0usize;
    let mut high = messages.len();
    while low < high {
        let mid = (low + high) / 2;
        if ctx.token_count_of(&messages[mid..]) <= keep {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low.min(messages.len() - 1)
}

/// `/isToolResultUserMessage`：保留侧 user 消息以 tool_result 块开头时为真。
fn is_tool_result_user_message(message: Option<&Message>) -> bool {
    let Some(message) = message else {
        return false;
    };
    let Some(crate::services::compact::Content::Blocks(blocks)) = message.content() else {
        return false;
    };
    blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// TS `alignCutForToolPairs(messages, cut)`。
///
/// 把切点回拉，直到保留侧不再以孤立 tool_result 开头：配对的 tool_use
/// 也必须留在保留侧。
fn align_cut_for_tool_pairs(messages: &[Message], cut: usize) -> usize {
    let mut aligned = cut;
    while aligned > 0 && is_tool_result_user_message(messages.get(aligned)) {
        aligned -= 1;
    }
    aligned
}

/// TS `NON_ASCII_RATIO_LIMIT`：序列化非 ASCII 占比超过该值后帧会变成
/// `?` 噪声。
const NON_ASCII_RATIO_LIMIT: f64 = 0.05;

/// TS `nonAsciiRatio(text)`：序列化字符中 U+007F 以上的占比。
fn non_ascii_ratio(text: &str) -> f64 {
    let total = text.chars().count();
    if total == 0 {
        return 0.0;
    }
    let non_ascii = text.chars().filter(|character| (*character as u32) > 0x7f).count();
    non_ascii as f64 / total as f64
}

fn premature_abort(ctx: &CompactionContext) -> Option<&'static str> {
    if ctx.abort_signal.as_ref().map(|signal| signal.aborted()).unwrap_or(false) {
        Some("snapcompact-aborted")
    } else {
        None
    }
}

fn create_boundary_message(
    trigger: crate::services::compaction2::contracts::TriggerReason,
    pre_tokens: i64,
) -> SystemMessage {
    SystemMessage {
        uuid: uuid_v4(),
        timestamp: now_ts(),
        subtype: Some("compact_boundary".to_string()),
        content: "Conversation compacted".to_string(),
        level: Some("info".to_string()),
        is_meta: Some(false),
        compact_metadata: Some(crate::services::compact::CompactMetadata {
            trigger: Some(
                if trigger
                    == crate::services::compaction2::contracts::TriggerReason::Manual
                {
                    "manual".to_string()
                } else {
                    "auto".to_string()
                },
            ),
            pre_tokens: Some(pre_tokens),
            ..Default::default()
        }),
        snip_metadata: None,
        logical_parent_uuid: None,
    }
}

fn budget_options(
    deps: &SnapcompactMethodDeps,
    ctx: &CompactionContext,
) -> BudgetOptions {
    BudgetOptions {
        max_frames: deps.max_frames,
        data_budget_bytes: deps.data_budget_bytes,
        token_budget: resolve_token_budget(deps, ctx),
    }
}

/// TS `renderPlannedFrames(plan, font, ctx)`。
fn render_planned_frames(
    plan: &[FramePlan],
    font: &dyn FontProvider,
    ctx: &CompactionContext,
) -> Result<Vec<RenderedFrame>, &'static str> {
    let mut rendered: Vec<RenderedFrame> = Vec::new();
    for frame in plan {
        if premature_abort(ctx).is_some() {
            return Err("snapcompact-aborted");
        }
        rendered.push(render_frame(&frame.text, &frame.shape, font));
    }
    Ok(rendered)
}

/// TS `renderWithinPayloadBudget`。
///
/// 渲染计划；若实际 base64 载荷超过 `data_budget`，用放得下的帧数重新
/// 计划并再渲染（至多两次）。单帧允许超过估算——它无法再缩小。
fn render_within_payload_budget(
    normalized: &str,
    profile: &FamilyProfile,
    options: &BudgetOptions,
    font: &dyn FontProvider,
    ctx: &CompactionContext,
    data_budget: i64,
) -> Result<(ArchivePlan, Vec<RenderedFrame>), &'static str> {
    let mut plan = plan_archive(normalized, profile, options);
    let mut frames = render_planned_frames(&plan.frames, font, ctx)?;
    for _attempt in 0..2 {
        if frames.len() <= 1 {
            break;
        }
        let payload = frame_data_bytes(&frames);
        if payload <= data_budget {
            break;
        }
        let target = std::cmp::max(
            1,
            (frames.len() as i64 * data_budget / payload) as usize,
        );
        plan = plan_archive(
            normalized,
            profile,
            &BudgetOptions {
                max_frames: Some(plan.max_frames.min(target)),
                ..options.clone()
            },
        );
        frames = render_planned_frames(&plan.frames, font, ctx)?;
    }
    Ok((plan, frames))
}

fn build_archive(
    plan: &ArchivePlan,
    profile: &FamilyProfile,
    frames: Vec<RenderedFrame>,
    summary: &str,
) -> SnapcompactImageArchive {
    SnapcompactImageArchive {
        base: SnapcompactArchiveBase {
            version: 1,
            total_chars: plan.kept_text.chars().count(),
            truncated_chars: plan.truncated_chars,
            text: plan.kept_text.clone(),
            text_head: plan.text_head.clone(),
            text_tail: plan.text_tail.clone(),
            summary: summary.to_string(),
        },
        mode: "image",
        family: profile.family,
        shape: profile.high.name,
        frame_size: profile.frame_size,
        frames,
    }
}

/// TS `TextEdges`。
struct TextEdges {
    text_head: String,
    text_tail: String,
    kept_text: String,
    truncated_chars: usize,
}

/// TS `textArchiveCharBudget(deps, ctx, profile)`。
///
/// 文本模式的字符预算：给了 token 预算（4 字符/token）用它，否则用图片
/// 归档保留的两个逐字边缘区域。
fn text_archive_char_budget(
    deps: &SnapcompactMethodDeps,
    ctx: &CompactionContext,
    profile: &FamilyProfile,
) -> usize {
    let token_budget = resolve_token_budget(deps, ctx);
    if let Some(token_budget) = token_budget {
        if token_budget > 0 {
            return std::cmp::max(2, token_budget.saturating_mul(4) as usize);
        }
    }
    let capacity = crate::services::compaction2::methods::snapcompact::planner::geometry(
        &profile.high,
        None,
    )
    .capacity;
    std::cmp::max(2, 2 * capacity)
}

/// TS `splitTextEdges(text, budgetChars)`：在 `budgetChars` 内保留最旧与
/// 最新切片，中间省略。
fn split_text_edges(text: &str, budget_chars: usize) -> TextEdges {
    let total = text.chars().count();
    if total <= budget_chars {
        return TextEdges {
            text_head: text.to_string(),
            text_tail: String::new(),
            kept_text: text.to_string(),
            truncated_chars: 0,
        };
    }
    let head_chars = (budget_chars + 1) / 2;
    let tail_chars = budget_chars - head_chars;
    let chars: Vec<char> = text.chars().collect();
    let text_head: String = chars[..head_chars].iter().collect();
    let text_tail: String = if tail_chars > 0 {
        chars[total - tail_chars..].iter().collect()
    } else {
        String::new()
    };
    TextEdges {
        kept_text: format!("{text_head}{text_tail}"),
        truncated_chars: total - head_chars - tail_chars,
        text_head,
        text_tail,
    }
}

/// TS `buildTextArchive(edges, summary)`：纯文本归档，不伪造帧。
fn build_text_archive(edges: &TextEdges, summary: &str) -> SnapcompactTextArchive {
    SnapcompactTextArchive {
        base: SnapcompactArchiveBase {
            version: 1,
            total_chars: edges.kept_text.chars().count(),
            truncated_chars: edges.truncated_chars,
            text: edges.kept_text.clone(),
            text_head: edges.text_head.clone(),
            text_tail: edges.text_tail.clone(),
            summary: summary.to_string(),
        },
        mode: "text",
    }
}

fn build_cut_point(
    ctx: &CompactionContext,
    kept: &[Message],
    archived: &[Message],
    tokens_before: i64,
) -> CutPoint {
    let first_kept_entry_id = kept
        .first()
        .map(|message| message.uuid().to_string())
        .or_else(|| archived.last().map(|message| message.uuid().to_string()))
        .unwrap_or_default();
    CutPoint {
        first_kept_entry_id,
        tokens_before,
        boundary_message: create_boundary_message(ctx.trigger, tokens_before),
    }
}

fn compacted_outcome(
    ctx: &CompactionContext,
    cut_point: CutPoint,
    tokens_saved: i64,
    summary: &str,
    archive: SnapcompactArchive,
) -> CompactionOutcome {
    let after_tokens = std::cmp::max(0, cut_point.tokens_before - tokens_saved);
    let threshold = ctx.settings.threshold_tokens;
    let mut preserve_data = Map::new();
    let archive_value = match &archive {
        SnapcompactArchive::Image(image) => serde_json::to_value(image),
        SnapcompactArchive::Text(text) => serde_json::to_value(text),
    };
    if let Ok(value) = archive_value {
        preserve_data.insert(SNAPCOMPACT_PRESERVE_KEY.to_string(), value);
    }
    CompactionOutcome::Compacted {
        method: CompactionMethodName::Snapcompact,
        cut_point,
        tokens_saved,
        headroom_tokens: threshold.map(|threshold| threshold - after_tokens).unwrap_or(0),
        summary: Some(summary.to_string()),
        preserve_data: Some(preserve_data),
        warning: None,
    }
}

/// TS `createSnapcompactMethod(deps = {})`。
pub fn create_snapcompact_method(
    deps: Option<SnapcompactMethodDeps>,
) -> Box<dyn CompactionMethod> {
    struct SnapcompactMethod {
        deps: Option<SnapcompactMethodDeps>,
    }

    impl CompactionMethod for SnapcompactMethod {
        fn name(&self) -> CompactionMethodName {
            CompactionMethodName::Snapcompact
        }

        fn is_available(&self, ctx: &CompactionContext) -> bool {
            let Some(deps) = self.deps.as_ref() else {
                return false;
            };
            if !enabled(deps) {
                return false;
            }
            if !declared_image_input(deps, ctx) {
                return false;
            }
            if ctx
                .abort_signal
                .as_ref()
                .map(|signal| signal.aborted())
                .unwrap_or(false)
            {
                return false;
            }
            if ctx.messages.is_empty() {
                return false;
            }
            resolve_profile_for(deps, ctx).is_some()
        }

        fn run<'a>(
            &'a self,
            ctx: &'a CompactionContext,
        ) -> Pin<Box<dyn Future<Output = CompactionOutcome> + Send + 'a>> {
            Box::pin(async move {
                let Some(deps) = self.deps.as_ref() else {
                    return CompactionOutcome::Skipped {
                        method: CompactionMethodName::Snapcompact,
                        reason: "snapcompact-disabled".to_string(),
                    };
                };
                run_snapcompact(deps, ctx)
            })
        }
    }

    Box::new(SnapcompactMethod { deps })
}

/// 方法体（同步决策图；TS 的 try/catch 在此映射为 `Result`）。
fn run_snapcompact(deps: &SnapcompactMethodDeps, ctx: &CompactionContext) -> CompactionOutcome {
    let skipped = |reason: &str| CompactionOutcome::Skipped {
        method: CompactionMethodName::Snapcompact,
        reason: reason.to_string(),
    };
    if !enabled(deps) {
        return skipped("snapcompact-disabled");
    }
    if !declared_image_input(deps, ctx) {
        return skipped("snapcompact-image-input-not-declared");
    }
    let Some(profile) = resolve_profile_for(deps, ctx) else {
        return skipped("snapcompact-family-unresolved");
    };
    if ctx.messages.is_empty() {
        return skipped("snapcompact-empty");
    }

    match run_snapcompact_inner(deps, ctx, &profile) {
        Ok(outcome) => outcome,
        Err(reason) => CompactionOutcome::Failed {
            method: CompactionMethodName::Snapcompact,
            reason,
        },
    }
}

fn run_snapcompact_inner(
    deps: &SnapcompactMethodDeps,
    ctx: &CompactionContext,
    profile: &FamilyProfile,
) -> Result<CompactionOutcome, String> {
    let skipped = |reason: &str| -> CompactionOutcome {
        CompactionOutcome::Skipped {
            method: CompactionMethodName::Snapcompact,
            reason: reason.to_string(),
        }
    };
    let cut = align_cut_for_tool_pairs(&ctx.messages, select_cut(ctx));
    if cut == 0 {
        return Ok(skipped("snapcompact-nothing-to-archive"));
    }
    let archived: Vec<Message> = ctx.messages[..cut].to_vec();
    let kept: Vec<Message> = ctx.messages[cut..].to_vec();
    let serialized = serialize_conversation(&archived, deps.serialize_options.as_ref());
    let tokens_before = ctx.token_count_of(&ctx.messages);
    let archived_tokens = ctx.token_count_of(&archived);
    let cut_point = build_cut_point(ctx, &kept, &archived, tokens_before);

    // 内置 5x8 位图字体只覆盖 ASCII；渲染 CJK/emoji 会悄悄把归档变成
    // `?` 噪声。回落为纯文本归档，而不是丢掉会话。
    if non_ascii_ratio(&serialized) > NON_ASCII_RATIO_LIMIT {
        if let Some(reason) = premature_abort(ctx) {
            return Err(reason.to_string());
        }
        if serialized.trim().is_empty() {
            return Ok(skipped("snapcompact-empty-archive"));
        }
        let edges = split_text_edges(
            &serialized,
            text_archive_char_budget(deps, ctx, profile),
        );
        let summary = match deps.summary.as_ref() {
            Some(deps_summary) => format!("{deps_summary} [snapcompact text mode]"),
            None => format!(
                "[snapcompact text archive] {} chars kept, {} chars elided.",
                edges.kept_text.chars().count(),
                edges.truncated_chars
            ),
        };
        let text_tokens =
            ((edges.kept_text.chars().count() + summary.chars().count() + 3) / 4) as i64;
        let tokens_saved = std::cmp::max(0, archived_tokens - text_tokens);
        if tokens_saved <= 0 {
            return Ok(skipped("snapcompact-no-savings"));
        }
        return Ok(compacted_outcome(
            ctx,
            cut_point,
            tokens_saved,
            &summary,
            SnapcompactArchive::Text(build_text_archive(&edges, &summary)),
        ));
    }

    let normalized = normalize_for_frame(&serialized);
    if normalized.is_empty() {
        return Ok(skipped("snapcompact-empty-archive"));
    }

    let default_font = five_by_eight_font();
    let font: &dyn FontProvider = deps
        .font
        .as_deref()
        .unwrap_or(default_font.as_ref());
    let options = budget_options(deps, ctx);
    let data_budget = deps
        .data_budget_bytes
        .unwrap_or(crate::services::compaction2::methods::snapcompact::planner::DEFAULT_FRAME_DATA_BYTES_BUDGET);
    let (plan, frames) =
        render_within_payload_budget(&normalized, profile, &options, font, ctx, data_budget)?;
    if frames.len() > 1 && frame_data_bytes(&frames) > data_budget {
        return Ok(skipped("snapcompact-payload-over-budget"));
    }

    let summary = deps.summary.clone().unwrap_or_else(|| {
        format!(
            "[snapcompact archive] {} frame(s) ({}), {} chars kept, {} chars elided.",
            frames.len(),
            profile.family.as_str(),
            plan.kept_text.chars().count(),
            plan.truncated_chars
        )
    });
    let text_tokens = ((plan.text_head.chars().count()
        + plan.text_tail.chars().count()
        + summary.chars().count()
        + 3) / 4) as i64;
    let frame_tokens = frames.len() as i64 * profile.frame_token_estimate;
    let tokens_saved = std::cmp::max(0, archived_tokens - text_tokens - frame_tokens);
    if tokens_saved <= 0 {
        return Ok(skipped("snapcompact-no-savings"));
    }

    Ok(compacted_outcome(
        ctx,
        cut_point,
        tokens_saved,
        &summary,
        SnapcompactArchive::Image(build_archive(&plan, profile, frames, &summary)),
    ))
}
