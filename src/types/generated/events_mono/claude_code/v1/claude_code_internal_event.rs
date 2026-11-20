//! claude_code_internal_event.rs — protobuf
//! `events_mono/claude_code/v1/claude_code_internal_event.proto` 的 Rust 映射。
//!
//! 重写自 `src/types/generated/events_mono/claude_code/v1/`
//! `claude_code_internal_event.ts`（protoc-gen-ts_proto v2.6.1 生成代码）。
//! 语义对齐说明：
//! - TS 的 `MessageFns` 由 serde + `Default` 覆盖；
//! - proto3 缺失字段等于默认值，标量字段不用 `Option`；
//! - 时间字段在 TS 侧为 `Date`（JSON 映射为 RFC 3339 字符串），映射为
//!   `Option<DateTime<Utc>>`；
//! - 嵌套消息（`env` / `auth` / `slack` / `github_actions_metadata`）用
//!   `Option` 区分缺失。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::generated::events_mono::common::v1::auth::PublicApiAuth;

/// GitHub Actions 环境信息。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct GitHubActionsMetadata {
    /// 触发 workflow 的用户 ID。
    #[serde(default)]
    pub actor_id: String,
    /// 仓库 ID。
    #[serde(default)]
    pub repository_id: String,
    /// 仓库所有者 ID。
    #[serde(default)]
    pub repository_owner_id: String,
}

/// 环境与运行时元数据。
/// 字段来源对应 claude-cli-internal/src/services/statsig.ts。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub node_version: String,
    #[serde(default)]
    pub terminal: String,
    #[serde(default)]
    pub package_managers: String,
    #[serde(default)]
    pub runtimes: String,
    #[serde(default)]
    pub is_running_with_bun: bool,
    #[serde(default)]
    pub is_ci: bool,
    #[serde(default)]
    pub is_claubbit: bool,
    #[serde(default)]
    pub is_github_action: bool,
    #[serde(default)]
    pub is_claude_code_action: bool,
    #[serde(default)]
    pub is_claude_ai_auth: bool,
    #[serde(default)]
    pub version: String,
    /// GitHub Actions 专有字段（仅当 is_github_action 为 true 时存在）。
    #[serde(default)]
    pub github_event_name: String,
    #[serde(default)]
    pub github_actions_runner_environment: String,
    #[serde(default)]
    pub github_actions_runner_os: String,
    #[serde(default)]
    pub github_action_ref: String,
    /// WSL 专有字段。
    #[serde(default)]
    pub wsl_version: String,
    /// GitHub 元数据（仅当 is_github_action 为 true 时存在）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_actions_metadata: Option<GitHubActionsMetadata>,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub is_claude_code_remote: bool,
    #[serde(default)]
    pub remote_environment_type: String,
    #[serde(default)]
    pub claude_code_container_id: String,
    #[serde(default)]
    pub claude_code_remote_session_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub deployment_environment: String,
    #[serde(default)]
    pub is_conductor: bool,
    #[serde(default)]
    pub version_base: String,
    #[serde(default)]
    pub coworker_type: String,
    #[serde(default)]
    pub build_time: String,
    #[serde(default)]
    pub is_local_agent_mode: bool,
    #[serde(default)]
    pub linux_distro_id: String,
    #[serde(default)]
    pub linux_distro_version: String,
    #[serde(default)]
    pub linux_kernel: String,
    #[serde(default)]
    pub vcs: String,
    #[serde(default)]
    pub platform_raw: String,
}

/// Claude-in-Slack (CIS) 每个事件都携带的上下文字段。
/// 事件专属字段（errorType、durationMs、httpStatus 等）放在
/// `ClaudeCodeInternalEvent.additional_metadata` 的 JSON 中。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct SlackContext {
    #[serde(default)]
    pub slack_team_id: String,
    #[serde(default)]
    pub is_enterprise_install: bool,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub creation_method: String,
}

/// Claude Code 通过 Statsig 上报的内部事件。
/// schema 与 claude-cli-internal/src/services/statsig.ts 一致；
/// 目标表：proj-product-data-nhme.raw_statsig_internal_tools.events。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClaudeCodeInternalEvent {
    /// 事件名（如 "tengu_binary_feedback"、"tengu_api_success"）。
    #[serde(default)]
    pub event_name: String,
    /// 事件时间戳。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub user_type: String,
    #[serde(default)]
    pub betas: String,
    /// 环境与运行时信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvironmentMetadata>,
    #[serde(default)]
    pub entrypoint: String,
    #[serde(default)]
    pub agent_sdk_version: String,
    #[serde(default)]
    pub is_interactive: bool,
    #[serde(default)]
    pub client_type: String,
    /// 进程指标 JSON 字符串（ant-only）：uptime、rss、heapTotal、heapUsed、
    /// external、arrayBuffers、constrainedMemory、cpuUsage。
    #[serde(default)]
    pub process: String,
    /// logEvent 附加元数据（事件专属）：msg_id_A、msg_id_B、gitBranch、
    /// gitHead 等随事件类型变化的字段。
    #[serde(default)]
    pub additional_metadata: String,
    /// API 端自动注入的鉴权上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<PublicApiAuth>,
    /// API 端自动注入的服务端时间戳。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_timestamp: Option<DateTime<Utc>>,
    /// 事件唯一标识（API 端点自动生成）。
    #[serde(default)]
    pub event_id: String,
    /// 客户端设备标识。
    #[serde(default)]
    pub device_id: String,
    // ---- SWE-bench 字段 ----
    #[serde(default)]
    pub swe_bench_run_id: String,
    #[serde(default)]
    pub swe_bench_instance_id: String,
    #[serde(default)]
    pub swe_bench_task_id: String,
    #[serde(default)]
    pub email: String,
    /// Swarm/team agent 分析归因用的标识。
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub parent_session_id: String,
    #[serde(default)]
    pub agent_type: String,
    /// Claude-in-Slack 上下文（仅 cis_* 事件携带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackContext>,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub skill_name: String,
    #[serde(default)]
    pub plugin_name: String,
    #[serde(default)]
    pub marketplace_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_actions_metadata_defaults() {
        let m = GitHubActionsMetadata::default();
        assert_eq!(m.actor_id, "");
        assert_eq!(m.repository_id, "");
        assert_eq!(m.repository_owner_id, "");
    }

    #[test]
    fn environment_metadata_defaults() {
        let e = EnvironmentMetadata::default();
        assert_eq!(e.platform, "");
        assert!(!e.is_running_with_bun);
        assert!(e.tags.is_empty());
        assert_eq!(e.github_actions_metadata, None);
    }

    #[test]
    fn slack_context_defaults() {
        let s = SlackContext::default();
        assert_eq!(s.slack_team_id, "");
        assert!(!s.is_enterprise_install);
    }

    #[test]
    fn internal_event_defaults() {
        let e = ClaudeCodeInternalEvent::default();
        assert_eq!(e.event_name, "");
        assert_eq!(e.client_timestamp, None);
        assert_eq!(e.env, None);
        assert_eq!(e.auth, None);
        assert_eq!(e.slack, None);
        assert_eq!(e.server_timestamp, None);
        assert!(!e.is_interactive);
    }

    #[test]
    fn json_roundtrip_full_event() {
        let event = ClaudeCodeInternalEvent {
            event_name: "tengu_api_success".into(),
            client_timestamp: Some(
                "2024-01-15T01:30:15.01Z".parse::<DateTime<Utc>>().unwrap(),
            ),
            env: Some(EnvironmentMetadata {
                platform: "darwin".into(),
                is_running_with_bun: true,
                tags: vec!["a".into(), "b".into()],
                ..Default::default()
            }),
            auth: Some(PublicApiAuth {
                account_id: 42,
                organization_uuid: "org".into(),
                account_uuid: "acct".into(),
            }),
            slack: Some(SlackContext {
                slack_team_id: "T123".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event_name"], "tengu_api_success");
        let ts_str = json["client_timestamp"].as_str().unwrap();
        assert_eq!(
            ts_str.parse::<DateTime<Utc>>().unwrap(),
            event.client_timestamp.unwrap()
        );
        assert_eq!(json["env"]["is_running_with_bun"], true);
        assert_eq!(json["auth"]["account_id"], 42);
        assert_eq!(json["slack"]["slack_team_id"], "T123");
        let back: ClaudeCodeInternalEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn json_roundtrip_github_metadata() {
        let env = EnvironmentMetadata {
            is_github_action: true,
            github_actions_metadata: Some(GitHubActionsMetadata {
                actor_id: "actor-1".into(),
                repository_id: "repo-1".into(),
                repository_owner_id: "owner-1".into(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["github_actions_metadata"]["repository_id"], "repo-1");
        let back: EnvironmentMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(back, env);
    }
}
