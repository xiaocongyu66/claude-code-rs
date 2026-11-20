//! figures.rs — 终端符号常量。
//!
//! 重写自 `src/constants/figures.ts`。平台判定内联（与 utils/env 的
//! env.platform 同语义），避免引入未转换的 env 模块。BLACK_CIRCLE 在 TS
//! 中是模块加载时求值的常量，此处改为函数（macOS 上渲染更圆润的 ⏺）。

/// `PLATFORM === 'darwin'` 的等价判定（std::env::consts::OS）。
fn is_darwin() -> bool {
    std::env::consts::OS == "macos"
}

// The former is better vertically aligned, but isn't usually supported on Windows/Linux
pub const BULLET_OPERATOR: &str = "∙";
pub const TEARDROP_ASTERISK: &str = "✻";
pub const UP_ARROW: &str = "↑"; // ↑ - used for opus 1m merge notice
pub const DOWN_ARROW: &str = "↓"; // ↓ - used for scroll hint
pub const LIGHTNING_BOLT: &str = "↯"; // ↯ - used for fast mode indicator
pub const EFFORT_LOW: &str = "○"; // ○ - effort level: low
pub const EFFORT_MEDIUM: &str = "◐"; // ◐ - effort level: medium
pub const EFFORT_HIGH: &str = "●"; // ● - effort level: high
pub const EFFORT_XHIGH: &str = "⦿"; // ⦿ - effort level: xhigh (Opus 4.7 only)
pub const EFFORT_MAX: &str = "◉"; // ◉ - effort level: max (Opus 4.6/4.7 only)

// Media/trigger status indicators
pub const PLAY_ICON: &str = "▶"; // ▶
pub const PAUSE_ICON: &str = "⏸"; // ⏸

// MCP subscription indicators
pub const REFRESH_ARROW: &str = "↻"; // ↻ - used for resource update indicator
pub const CHANNEL_ARROW: &str = "←"; // ← - inbound channel message indicator
pub const INJECTED_ARROW: &str = "→"; // → - cross-session injected message indicator
pub const FORK_GLYPH: &str = "⑂"; // ⑂ - fork directive indicator

// Review status indicators (ultrareview diamond states)
pub const DIAMOND_OPEN: &str = "◇"; // ◇ - running
pub const DIAMOND_FILLED: &str = "◆"; // ◆ - completed/failed
pub const REFERENCE_MARK: &str = "※"; // ※ - komejirushi, away-summary recap marker

// Issue flag indicator
pub const FLAG_ICON: &str = "⚑"; // ⚑ - used for issue flag banner

// Blockquote indicator
pub const BLOCKQUOTE_BAR: &str = "▎"; // ▎ - left one-quarter block, used as blockquote line prefix
pub const HEAVY_HORIZONTAL: &str = "━"; // ━ - heavy box-drawing horizontal

/// macOS 上为 ⏺，其他平台（Windows/Linux）为 ●。
pub fn black_circle() -> &'static str {
    if is_darwin() {
        "⏺"
    } else {
        "●"
    }
}

// Bridge status indicators
pub const BRIDGE_SPINNER_FRAMES: [&str; 4] = ["·|·", "·/·", "·—·", "·\\·"];
pub const BRIDGE_READY_INDICATOR: &str = "·✔︎·";
pub const BRIDGE_FAILED_INDICATOR: &str = "×";
