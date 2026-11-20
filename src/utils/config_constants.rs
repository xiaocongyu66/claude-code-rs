//! config_constants.rs — 为避免循环依赖而独立出的配置常量。
//!
//! 重写自 `src/utils/configConstants.ts`。
//! 本文件必须保持无依赖（Do NOT add imports）。

/// 通知渠道枚举（settings 的 notification 通道选项）。
pub const NOTIFICATION_CHANNELS: &[&str] = &[
    "auto",
    "iterm2",
    "iterm2_with_bell",
    "terminal_bell",
    "kitty",
    "ghostty",
    "notifications_disabled",
];

/// 编辑器模式（不含已废弃的 'emacs'——它会自动迁移到 'normal'）。
pub const EDITOR_MODES: &[&str] = &["normal", "vim"];

/// teammate 生成模式：
/// - `tmux`：传统基于 tmux 的 teammate
/// - `in-process`：同进程内运行的 teammate
/// - `auto`：按上下文自动选择（默认）
pub const TEAMMATE_MODES: &[&str] = &["auto", "tmux", "in-process"];
