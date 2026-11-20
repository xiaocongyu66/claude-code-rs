//! hyperlink.rs — OSC 8 超链接转义序列。
//!
//! 重写自 `src/utils/hyperlink.ts`。语义对齐：
//!
//! - 格式：`\e]8;;URL\a` + 显示文本 + `\e]8;;\a`。
//! - 终端不支持超链接时直接返回 URL 原文。
//! - 显示文本默认为 URL；支持时套一层基础 ANSI 蓝色（对齐 TS 的
//!   `chalk.blue`；RGB/主题色在 OSC 8 下不被 wrap-ansi 保留）。
//!
//! 偏差：TS 的 `supportsHyperlinks` 来自 @anthropic/ink 的终端能力检测
//! （包含 CI/非 TTY/dumb terminal 判定）；Rust 侧无 ink，用环境变量
//! 近似：非 TTY、CI 环境或 dumb 终端视为不支持；`TERM_PROGRAM` 为
//! iTerm.app / WezTerm / vscode 等明确支持 OSC 8 的程序时视为支持。

use std::io::IsTerminal;

/// OSC 8 起始序列（以 BEL 结尾，兼容性比 ST 好）。
pub const OSC8_START: &str = "\x1b]8;;";
/// OSC 8 结束序列。
pub const OSC8_END: &str = "\x07";

/// ANSI 蓝色 SGR 前缀/重置。
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_RESET: &str = "\x1b[0m";

fn env_is(key: &str, value: &str) -> bool {
    std::env::var(key).map(|v| v == value).unwrap_or(false)
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// 终端是否支持 OSC 8 超链接。
///
/// 近似 ink 的 supportsHyperlinks：非 TTY / CI / dumb terminal 不支持；
/// TERM_PROGRAM 为已知支持 OSC 8 的程序时支持。
pub fn supports_hyperlinks() -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if env_truthy("CI") {
        return false;
    }
    if env_is("TERM", "dumb") {
        return false;
    }
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    matches!(
        program.as_str(),
        "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "tabby" | "Hyper" | "Konsole"
    )
}

/// 用 OSC 8 创建可点击超链接。终端不支持时返回 URL 原文。
///
/// - `content`：可选的链接显示文本；仅在支持超链接时生效。不支持时
///   content 被忽略，只显示 URL。
/// - `supports_hyperlinks_override`：供测试注入检测结果。
pub fn create_hyperlink(
    url: &str,
    content: Option<&str>,
    supports_hyperlinks_override: Option<bool>,
) -> String {
    let has_support = supports_hyperlinks_override.unwrap_or(supports_hyperlinks());
    if !has_support {
        return url.to_string();
    }

    let display_text = content.unwrap_or(url);
    let colored_text = format!("{ANSI_BLUE}{display_text}{ANSI_RESET}");
    format!("{OSC8_START}{url}{OSC8_END}{colored_text}{OSC8_START}{OSC8_END}")
}
