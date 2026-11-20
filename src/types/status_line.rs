//! status_line.rs — 自定义状态行命令的 stdin JSON 负载。
//!
//! 重写自 `src/types/statusLine.ts`，与 `buildStatusLineCommandInput` 输出形状
//! 一致。字段名在 TS 源码中已是 snake_case，直接保留。TS 的 `number | null`
//! 映射为 `Option<f64>`/`Option<i64>`。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineModel {
    /// 当前主循环模型 id。
    pub id: String,
    /// 已本地化的展示名。
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineWorkspace {
    /// 进程 cwd。
    pub current_dir: String,
    /// 项目根（原始 cwd）。
    pub project_dir: String,
    /// 附加工作区目录列表。
    pub added_dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineOutputStyle {
    /// 当前输出样式名。
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLineCost {
    /// 累计美元成本估计。
    pub total_cost_usd: f64,
    /// 会话墙钟时长。
    pub total_duration_ms: u64,
    /// API 往返累计。
    pub total_api_duration_ms: u64,
    /// 归因新增行数。
    pub total_lines_added: u64,
    /// 归因删除行数。
    pub total_lines_removed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLineCurrentUsage {
    /// 最近一条用量快照：输入。
    pub input_tokens: u64,
    /// 最近一条用量快照：输出。
    pub output_tokens: u64,
    /// 缓存写入 token。
    pub cache_creation_input_tokens: u64,
    /// 缓存命中读取 token。
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLineContextWindow {
    /// 累计输入 token（未知为 None）。
    pub total_input_tokens: Option<u64>,
    /// 累计输出 token（未知为 None）。
    pub total_output_tokens: Option<u64>,
    /// 当前模型上下文上限。
    pub context_window_size: u64,
    /// 最近一条用量快照（尚无有效用量时为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_usage: Option<StatusLineCurrentUsage>,
    /// 已用上下文占比。
    pub used_percentage: Option<f64>,
    /// 剩余占比。
    pub remaining_percentage: Option<f64>,
}

/// 单个速率限制窗口的用量与重置时间戳。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLineRateLimitWindow {
    pub used_percentage: f64,
    pub resets_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StatusLineRateLimits {
    /// 5 小时窗口用量与重置时间戳。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<StatusLineRateLimitWindow>,
    /// 7 天窗口。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<StatusLineRateLimitWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineVim {
    /// 当前 Vim 模式标签（如 INSERT）。
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineAgent {
    /// `--agent` 或子代理类型名。
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineRemote {
    /// 远程/桥接会话标识。
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineWorktree {
    /// worktree 展示名。
    pub name: String,
    /// worktree 根路径。
    pub path: String,
    /// 当前分支（可缺省）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// 进入 worktree 前 cwd。
    pub original_cwd: String,
    /// 进入前分支（可缺省）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_branch: Option<String>,
}

/// 自定义状态行命令（`settings.statusLine.command`）通过 stdin 接收的 JSON。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineCommandInput {
    /// 会话 id。
    pub session_id: String,
    /// transcript 路径。
    pub transcript_path: String,
    /// 当前工作目录。
    pub cwd: String,
    /// 工具权限模式快照。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// 子代理 id（若有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 子代理类型或主线程类型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// 用户可见会话标题（若有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub model: StatusLineModel,
    pub workspace: StatusLineWorkspace,
    /// CLI 版本号（MACRO.VERSION）。
    pub version: String,
    pub output_style: StatusLineOutputStyle,
    pub cost: StatusLineCost,
    pub context_window: StatusLineContextWindow,
    /// 是否超过 200k 输入警戒。
    pub exceeds_200k_tokens: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<StatusLineRateLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vim: Option<StatusLineVim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<StatusLineAgent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<StatusLineRemote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<StatusLineWorktree>,
}
