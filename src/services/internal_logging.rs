//! internal_logging.rs — ant 内部日志服务（重写自
//! `src/services/internalLogging.ts`）。
//!
//! 职责（对齐 TS）：
//! - `getKubernetesNamespace`：读
//!   `/var/run/secrets/kubernetes.io/serviceaccount/namespace`，不存在/读
//!   失败返回 "namespace not found"；非 ant 返回 None。
//! - `getContainerId`：从 `/proc/self/mountinfo` 匹配 Docker /
//!   containerd(CRI-O) 的 64 位十六进制容器 id（devbox 可观测性用）。
//! - `logPermissionContextForAnts`：记录工具权限上下文事件
//!   （`tengu_internal_record_permission_context`），含 moment /
//!   namespace / toolPermissionContext / containerId。
//!
//! 已知偏差：
//! - TS 用 lodash memoize（同进程只算一次）；Rust 用 `OnceLock` 等价。
//! - `jsonStringify(toolPermissionContext)` 走 `serde_json::to_value` →
//!   `json_stringify`（等价 JSON 序列化）。

use std::fs;
use std::sync::OnceLock;


use crate::tool::ToolPermissionContext;
use crate::utils::slow_operations::json_stringify;

use super::analytics::index::{log_event, LogEventMetadata, MetadataValue};

/// 记录时机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntLogMoment {
    /// 权限摘要。
    Summary,
    /// 初始化。
    Initialization,
}

impl AntLogMoment {
    fn as_str(self) -> &'static str {
        match self {
            AntLogMoment::Summary => "summary",
            AntLogMoment::Initialization => "initialization",
        }
    }
}

/// 当前 Kubernetes namespace：笔记本/本地开发返回 None，devbox 返回
/// "default" / "ts" 等；读失败返回 "namespace not found"。
pub fn get_kubernetes_namespace() -> Option<String> {
    static CACHE: OnceLock<Option<Option<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
                return None;
            }
            let namespace_path = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
            match fs::read_to_string(namespace_path) {
                Ok(content) => Some(Some(content.trim().to_string())),
                Err(_) => Some(Some("namespace not found".to_string())),
            }
        })
        .clone()
        .flatten()
}

/// 从运行容器内取 OCI 容器 id：失败返回 "container ID not found"，mountinfo
/// 中无匹配返回 "container ID not found in mountinfo"。
pub fn get_container_id() -> Option<String> {
    static CACHE: OnceLock<Option<Option<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
                return None;
            }
            let container_id_path = "/proc/self/mountinfo";
            match fs::read_to_string(container_id_path) {
                Ok(content) => {
                    // Docker: /docker/containers/[64-hex]
                    // Containerd/CRI-O: /sandboxes/[64-hex]
                    let pattern = match regex::Regex::new(
                        r"(?:/docker/containers/|/sandboxes/)([0-9a-f]{64})",
                    ) {
                        Ok(pattern) => pattern,
                        Err(_) => return Some(Some("container ID not found".to_string())),
                    };
                    for line in content.lines() {
                        if let Some(captured) = pattern.captures(line) {
                            if let Some(id) = captured.get(1) {
                                return Some(Some(id.as_str().to_string()));
                            }
                        }
                    }
                    Some(Some("container ID not found in mountinfo".to_string()))
                }
                Err(_) => Some(Some("container ID not found".to_string())),
            }
        })
        .clone()
        .flatten()
}

/// 带当前 namespace 与工具权限上下文记录事件（仅 ant）。
pub fn log_permission_context_for_ants(
    tool_permission_context: &ToolPermissionContext,
    moment: AntLogMoment,
) {
    if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
        return;
    }

    let namespace = get_kubernetes_namespace();
    let container_id = get_container_id();
    let context_json = serde_json::to_value(tool_permission_context)
        .map(|value| json_stringify(&value))
        .unwrap_or_else(|_| "unavailable".to_string());

    let mut metadata: LogEventMetadata = LogEventMetadata::new();
    metadata.insert(
        "moment".to_string(),
        Some(MetadataValue::String(moment.as_str().to_string())),
    );
    if let Some(namespace) = namespace {
        metadata.insert(
            "namespace".to_string(),
            Some(MetadataValue::String(namespace)),
        );
    }
    metadata.insert(
        "toolPermissionContext".to_string(),
        Some(MetadataValue::String(context_json)),
    );
    if let Some(container_id) = container_id {
        metadata.insert(
            "containerId".to_string(),
            Some(MetadataValue::String(container_id)),
        );
    }

    log_event("tengu_internal_record_permission_context", &metadata);
}
