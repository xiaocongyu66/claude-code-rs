//! embedded_tools.rs — 内嵌搜索工具判定（已退役的兼容开关）。
//!
//! 重写自 `src/utils/embeddedTools.ts`。
//!
//! bfs/ugrep 嵌入态已拆除：shell find/grep shadow 属"给人用"的人性化
//! 通道，Glob/Grep 专用工具恒注册。原判定 =「嵌入 payload 非空 && 非 SDK
//! 入口」；payload 源已删除，恒为空 → 收敛为恒 false，即所有文案走
//! Glob/Grep 专用工具分支。保留导出以避免牵动大量 prompt/message 文案
//! 消费点（prompts.ts、messages.ts、memdir.ts、BashTool/AgentTool 等）。

/// 恒为 false：嵌入搜索工具已退役。
pub fn has_embedded_search_tools() -> bool {
    false
}
