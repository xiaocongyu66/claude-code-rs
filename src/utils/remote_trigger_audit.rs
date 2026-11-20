//! remote_trigger_audit.rs — 远程触发的审计日志（JSONL）。
//!
//! 重写自 `src/utils/remoteTriggerAudit.ts`。语义对齐：
//!
//! - 审计文件位于 `<projectRoot>/.claude/remote-trigger-audit.jsonl`，
//!   每行一条 JSON 记录；`append_remote_trigger_audit_record` 追加，
//!   `list_remote_trigger_audit_records` 读取并按 createdAt 倒序截断
//!   最近 200 条。
//! - 结构化字段缺省处理：auditId 缺省随机 UUID v4，createdAt 缺省当前
//!   毫秒时间戳；可选字段（triggerId/status/error）仅在设值时序列化
//!   （对齐 TS 的条件展开）。
//! - 全部异步 I/O（tokio::fs）。

use std::path::PathBuf;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::bootstrap::state;

const REMOTE_TRIGGER_AUDIT_REL: &str = ".claude/remote-trigger-audit.jsonl";
const MAX_AUDIT_RECORDS: usize = 200;

/// 一条审计记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteTriggerAuditRecord {
    pub audit_id: String,
    pub action: String,
    /// 可选；仅在设值时序列化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
    pub ok: bool,
    /// 可选；仅在设值时序列化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 可选；仅在设值时序列化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: i64,
}

/// 追加入参（不含 auditId / createdAt，二者自动填充）。
#[derive(Debug, Clone)]
pub struct RemoteTriggerAuditInput {
    pub action: String,
    pub trigger_id: Option<String>,
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub audit_id: Option<String>,
    pub created_at: Option<i64>,
}

/// 解析审计文件路径（root_dir 缺省取当前 project root）。
pub fn resolve_remote_trigger_audit_path(root_dir: Option<&str>) -> PathBuf {
    let root = root_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(state::get_project_root()));
    root.join(REMOTE_TRIGGER_AUDIT_REL)
}

/// 追加一条审计记录并返回完整记录。
pub async fn append_remote_trigger_audit_record(
    record: RemoteTriggerAuditInput,
    root_dir: Option<&str>,
) -> RemoteTriggerAuditRecord {
    let full_record = RemoteTriggerAuditRecord {
        audit_id: record.audit_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        action: record.action,
        trigger_id: record.trigger_id,
        ok: record.ok,
        status: record.status,
        error: record.error,
        created_at: record
            .created_at
            .unwrap_or_else(|| Utc::now().timestamp_millis()),
    };
    let path = resolve_remote_trigger_audit_path(root_dir);
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let mut line = serde_json::to_string(&full_record).unwrap_or_default();
    line.push('\n');
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        use tokio::io::AsyncWriteExt;
        let _ = file.write_all(line.as_bytes()).await;
    }
    full_record
}

/// 列出审计记录（按 createdAt 倒序，最多 MAX_AUDIT_RECORDS 条）。
///
/// 读取失败（文件不存在等）返回空列表；逐行解析，忽略格式错误的旧行。
pub async fn list_remote_trigger_audit_records(
    root_dir: Option<&str>,
) -> Vec<RemoteTriggerAuditRecord> {
    let path = resolve_remote_trigger_audit_path(root_dir);
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };

    let mut records = Vec::new();
    for line in raw.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            let audit_id = value.get("auditId").and_then(Value::as_str);
            let action = value.get("action").and_then(Value::as_str);
            let ok = value.get("ok").and_then(Value::as_bool);
            let created_at = value.get("createdAt").and_then(Value::as_i64);
            if let (Some(audit_id), Some(action), Some(ok), Some(created_at)) =
                (audit_id, action, ok, created_at)
            {
                records.push(RemoteTriggerAuditRecord {
                    audit_id: audit_id.to_string(),
                    action: action.to_string(),
                    trigger_id: value.get("triggerId").and_then(Value::as_str).map(str::to_string),
                    ok,
                    status: value.get("status").and_then(Value::as_i64).and_then(|n| u16::try_from(n).ok()),
                    error: value.get("error").and_then(Value::as_str).map(str::to_string),
                    created_at,
                });
            }
        }
    }
    records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    records.truncate(MAX_AUDIT_RECORDS);
    records
}

/// 把记录列表格式化成人类可读的状态摘要。
pub fn format_remote_trigger_audit_status(records: &[RemoteTriggerAuditRecord]) -> String {
    let failures = records.iter().filter(|r| !r.ok).count();
    let latest = records.first();
    let latest_line = match latest {
        Some(l) => {
            let trigger = l.trigger_id.as_deref().map(|t| format!(" {t}")).unwrap_or_default();
            let ok_str = if l.ok { "ok" } else { "failed" };
            let when = chrono::DateTime::from_timestamp_millis(l.created_at)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| l.created_at.to_string());
            format!("Latest: {}{} {} ({})", l.action, trigger, ok_str, when)
        }
        None => "Latest: none".to_string(),
    };
    format!(
        "RemoteTrigger audit records: {}\nFailures: {}\n{latest_line}",
        records.len(),
        failures
    )
}
