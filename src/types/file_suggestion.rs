//! file_suggestion.rs — FileSuggestion 自定义命令的 stdin JSON 负载。
//!
//! 重写自 `src/types/fileSuggestion.ts`。字段与 `createBaseHookInput()` 一致，
//! 附加当前路径前缀 `query`。字段名在 TS 源码中已是 snake_case，直接保留。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSuggestionCommandInput {
    /// 当前会话 id。
    pub session_id: String,
    /// 会话 transcript 文件路径。
    pub transcript_path: String,
    /// 工作目录。
    pub cwd: String,
    /// 权限模式快照（若有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// 子代理 id（若在 agent 内触发）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 子代理类型或主线程类型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// 用户当前输入的路径前缀（待补全部分）。
    pub query: String,
}
