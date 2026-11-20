//! agent_swarms_enabled.rs — agent teams/teammate 功能的统一开关。
//!
//! 重写自 `src/utils/agentSwarmsEnabled.ts`。所有提到 teammate 的位置
//!（prompt、代码、工具 isEnabled、UI 等）都应检查这一个门控。
//!
//! Fork 构建：默认启用；可用 CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS_DISABLED
//!（真值）关闭。

use crate::utils::env_utils::is_env_truthy;
use std::env;

pub fn is_agent_swarms_enabled() -> bool {
    if is_env_truthy(
        env::var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS_DISABLED")
            .ok()
            .as_deref(),
    ) {
        return false;
    }
    true
}
