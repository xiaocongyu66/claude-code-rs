//! `services/compaction2/session/display.ts` — V2 显示消息。
//!
//! query 循环己经把 legacy compact boundary 嵌进 `CompactionResult`，V2 只需
//! （a）把产出方法盖章到该边界，（b）为机械削减（shake）冒一次一次性通知。
//! 两者都是普通消息形状，复用既有 `compact_boundary` / `informational`
//! 渲染器。
//!
//! 边界是 system 消息：`normalize_messages_for_api` 丢弃除本地命令输出外的
//! 所有 system 消息，所以边界与通知都不会进入模型请求。

use crate::services::compact::compact::CompactionResult;
use crate::services::compact::{now_ts, uuid_v4, CompactMetadata, SystemMessage};
use crate::services::compaction2::contracts::{CompactionMethodName, TriggerReason};

/// TS `CompactionBoundaryInput.trigger`：`TriggerReason | 'auto' | 'manual'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBoundaryTrigger {
    Reason(TriggerReason),
    Auto,
    Manual,
}

impl CompactionBoundaryTrigger {
    /// 触发原因折叠为 legacy 的 `auto` / `manual` 对。
    pub fn as_legacy_str(&self) -> &'static str {
        match self {
            CompactionBoundaryTrigger::Manual
            | CompactionBoundaryTrigger::Reason(TriggerReason::Manual) => "manual",
            CompactionBoundaryTrigger::Auto | CompactionBoundaryTrigger::Reason(_) => "auto",
        }
    }
}

/// TS `CompactionBoundaryInput`。
#[derive(Debug, Clone)]
pub struct CompactionBoundaryInput {
    pub method: CompactionMethodName,
    pub trigger: CompactionBoundaryTrigger,
    /// 切点处的上下文大小。
    pub pre_tokens: i64,
    /// 方法移除的 token（已知时）。
    pub tokens_saved: Option<i64>,
    /// 进度护卫带回的告警。
    pub warning: Option<String>,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
}

/// TS `createCompactionBoundaryMessage(input)`。
///
/// 形状对齐 legacy 标记（`subtype: 'compact_boundary'`）外加
/// `compactMetadata.method`。
pub fn create_compaction_boundary_message(input: &CompactionBoundaryInput) -> SystemMessage {
    let mut metadata = CompactMetadata {
        trigger: Some(input.trigger.as_legacy_str().to_string()),
        pre_tokens: Some(input.pre_tokens),
        method: Some(input.method.as_str().to_string()),
        ..Default::default()
    };
    if let Some(tokens_saved) = input.tokens_saved {
        metadata.tokens_saved = Some(tokens_saved);
    }
    if let Some(warning) = input.warning.as_ref() {
        metadata.warning = Some(warning.clone());
    }
    SystemMessage {
        uuid: input.uuid.clone().unwrap_or_else(uuid_v4),
        timestamp: input.timestamp.clone().unwrap_or_else(now_ts),
        subtype: Some("compact_boundary".to_string()),
        content: "Conversation compacted".to_string(),
        level: Some("info".to_string()),
        is_meta: Some(false),
        compact_metadata: Some(metadata),
        snip_metadata: None,
        logical_parent_uuid: None,
    }
}

/// TS `CompactionMethodMetadata`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionMethodMetadata {
    pub tokens_saved: Option<i64>,
    pub warning: Option<String>,
}

/// TS `withCompactionMethod(boundary, method, extra?)`。
///
/// 把 `method` 盖到既有边界上，不丢失它已携带的任何元数据
/// （trigger、preTokens、preservedSegment、发现的工具）。
/// soft 的边界来自 legacy 压缩结果，故用此函数。
pub fn with_compaction_method(
    boundary: &SystemMessage,
    method: CompactionMethodName,
    extra: &CompactionMethodMetadata,
) -> SystemMessage {
    let mut boundary = boundary.clone();
    let mut metadata = boundary.compact_metadata.clone().unwrap_or_default();
    metadata.method = Some(method.as_str().to_string());
    if let Some(tokens_saved) = extra.tokens_saved {
        metadata.tokens_saved = Some(tokens_saved);
    }
    if let Some(warning) = extra.warning.as_ref() {
        metadata.warning = Some(warning.clone());
    }
    boundary.compact_metadata = Some(metadata);
    boundary
}

/// TS `annotateCompactionResult(result, method, extra?)`。
///
/// 用盖章后的边界克隆 legacy 结果。非边界标记原样返回，调用方无需对结果
/// 形状分支。
pub fn annotate_compaction_result(
    result: CompactionResult,
    method: CompactionMethodName,
    extra: CompactionMethodMetadata,
) -> CompactionResult {
    if result.boundary_marker.subtype.as_deref() != Some("compact_boundary") {
        return result;
    }
    let boundary = with_compaction_method(&result.boundary_marker, method, &extra);
    let mut result = result;
    result.boundary_marker = boundary;
    result
}

/// TS `ReducedNoticeInput`。
#[derive(Debug, Clone)]
pub struct ReducedNoticeInput {
    pub method: CompactionMethodName,
    pub tokens_saved: i64,
    pub uuid: Option<String>,
    pub timestamp: Option<String>,
}

/// TS `createReducedNoticeMessage(input)`：机械削减（shake）的一次性通知。
pub fn create_reduced_notice_message(input: &ReducedNoticeInput) -> SystemMessage {
    SystemMessage {
        uuid: input.uuid.clone().unwrap_or_else(uuid_v4),
        timestamp: input.timestamp.clone().unwrap_or_else(now_ts),
        subtype: Some("informational".to_string()),
        content: format!(
            "Context reduced by {} tokens ({})",
            input.tokens_saved,
            input.method.as_str()
        ),
        is_meta: Some(false),
        level: Some("notice".to_string()),
        compact_metadata: None,
        snip_metadata: None,
        logical_parent_uuid: None,
    }
}

/// TS `ReducedNoticeState`。
#[derive(Debug, Clone, Default)]
pub struct ReducedNoticeState {
    pub emitted: bool,
}

/// TS `createReducedNoticeState()`。
pub fn create_reduced_notice_state() -> ReducedNoticeState {
    ReducedNoticeState { emitted: false }
}

/// TS `takeReducedNotice(state, input)`。
///
/// 每个状态（一个 query 轮）至多返回一次通知。同轮后续削减——threshold、
/// mid-turn、overflow——保持安静。
pub fn take_reduced_notice(
    state: &mut ReducedNoticeState,
    input: &ReducedNoticeInput,
) -> Option<SystemMessage> {
    if state.emitted {
        return None;
    }
    state.emitted = true;
    Some(create_reduced_notice_message(input))
}
