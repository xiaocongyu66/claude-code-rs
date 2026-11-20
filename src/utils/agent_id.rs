//! agent_id.rs — 确定性 Agent ID 系统。
//!
//! 重写自 `src/utils/agentId.ts`。为 swarm/teammate 系统提供确定性
//!（可复现、可读、可预测）的 agent ID 与 request ID 的格式化/解析。
//!
//! 格式约定（与 TS 源一致）：
//! - Agent ID：`agentName@teamName`
//! - Request ID：`{requestType}-{timestamp}@{agentId}`
//!
//! 约束：agent 名不得包含 `@`（它作分隔符）。

use chrono::Utc;

/// 按 `agentName@teamName` 格式化 agent ID。
pub fn format_agent_id(agent_name: &str, team_name: &str) -> String {
    format!("{agent_name}@{team_name}")
}

/// 解析 agent ID。不含 `@` 分隔符时返回 `None`。
pub fn parse_agent_id(agent_id: &str) -> Option<(String, String)> {
    let at_index = agent_id.find('@')?;
    let agent_name = agent_id[..at_index].to_string();
    let team_name = agent_id[at_index + 1..].to_string();
    Some((agent_name, team_name))
}

/// 按 `{requestType}-{timestamp}@{agentId}` 格式生成 request ID。
/// timestamp 为毫秒级 epoch（对齐 TS 的 `Date.now()`）。
pub fn generate_request_id(request_type: &str, agent_id: &str) -> String {
    let timestamp = Utc::now().timestamp_millis();
    format!("{request_type}-{timestamp}@{agent_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_agent_id() {
        let id = format_agent_id("researcher", "my-project");
        assert_eq!(id, "researcher@my-project");
        assert_eq!(
            parse_agent_id(&id),
            Some(("researcher".to_string(), "my-project".to_string()))
        );
    }

    #[test]
    fn parse_rejects_missing_separator() {
        assert_eq!(parse_agent_id("no-at-sign"), None);
    }

    #[test]
    fn request_id_format() {
        let id = generate_request_id("shutdown", "tester@my-project");
        let (head, agent) = id.split_once('@').unwrap();
        assert!(head.starts_with("shutdown-"));
        assert_eq!(agent, "tester@my-project");
    }
}
