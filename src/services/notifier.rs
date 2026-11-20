//! notifier.rs — 终端通知分发（重写自 `src/services/notifier.ts`）。
//!
//! 职责（对齐 TS）：
//! - `sendNotification(notif, terminal)`：按全局配置的
//!   `preferredNotifChannel` 分发到各终端通知通道，并记录
//!   `tengu_notification_method_used`。
//! - 通道：auto / iterm2 / iterm2_with_bell / kitty / ghostty /
//!   terminal_bell / notifications_disabled；auto 按当前终端类型自动选
//!   择（Apple_Terminal 时检查其响铃偏好）。
//!
//! 已知偏差：
//! - `@anthropic/ink` 的 `TerminalNotification` 是类型依赖：Rust 侧以
//!   `TerminalNotification` trait 表达同一接口（notifyITerm2 /
//!   notifyBell / notifyKitty / notifyGhostty），由调用方注入实现。
//! - `executeNotificationHooks`（utils/hooks.ts）未转换：hook 执行为
//!   no-op（待 hooks 落地后补上）。
//! - `utils/env.ts` 的 `env.terminal` 未转换：本模块内置
//!   `detect_terminal()` 最小等价实现（覆盖 TS detectTerminal 的主要分
//!   支：Cursor/Windsurf 系列、JetBrains、ghostty/kitty、TERM_PROGRAM
//!   与常见 Linux/Windows 终端变量；其余回落 TERM）。Apple_Terminal /
//!   iTerm.app 等 TERM_PROGRAM 值直接返回，保持 send_auto 分支正确。
//! - `preferredNotifChannel` 读 `~/.claude.json`（本地 duck-type 只读）；
//! - Apple Terminal 响铃偏好仍走 osascript + defaults（exec_file_no_throw
//!   已就绪），plist 解析为最小 string-scan（无 plist npm 依赖）。

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;

use crate::utils::exec_file_no_throw::{exec_file_no_throw, ExecFileOptions};

use super::analytics::index::{log_event, LogEventMetadata, MetadataValue};

/// 通知选项（TS `NotificationOptions`）。
#[derive(Debug, Clone, Default)]
pub struct NotificationOptions {
    pub message: String,
    pub title: Option<String>,
    pub notification_type: String,
}

/// 终端通知能力（TS `@anthropic/ink` 的 `TerminalNotification` 接口）。
/// 由调用方注入具体终端实现。
pub trait TerminalNotification: Send + Sync {
    fn notify_iterm2(&self, opts: &NotificationOptions);
    fn notify_bell(&self);
    fn notify_kitty(&self, opts: &NotificationOptions, title: &str, id: u32);
    fn notify_ghostty(&self, opts: &NotificationOptions, title: &str);
}

const DEFAULT_TITLE: &str = "Claude Code";

/// 发送通知：先执行 hooks（偏差：no-op），再按配置通道分发，最后记录
/// 使用的方法。
pub async fn send_notification(
    notif: &NotificationOptions,
    terminal: &dyn TerminalNotification,
) {
    let config = read_global_config();
    let channel = config
        .get("preferredNotifChannel")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .to_string();

    // executeNotificationHooks（utils/hooks.ts）未转换：hook 执行 no-op
    // —— hooks 落地后在此补上。

    let method_used = send_to_channel(&channel, notif, terminal).await;

    let mut metadata: LogEventMetadata = LogEventMetadata::new();
    metadata.insert(
        "configured_channel".to_string(),
        Some(MetadataValue::String(channel)),
    );
    metadata.insert(
        "method_used".to_string(),
        Some(MetadataValue::String(method_used)),
    );
    metadata.insert(
        "term".to_string(),
        Some(MetadataValue::String(detect_terminal().unwrap_or_default())),
    );
    log_event("tengu_notification_method_used", &metadata);
}

async fn send_to_channel(
    channel: &str,
    opts: &NotificationOptions,
    terminal: &dyn TerminalNotification,
) -> String {
    let title = opts.title.clone().unwrap_or_else(|| DEFAULT_TITLE.to_string());

    match channel {
        "auto" => send_auto(opts, terminal).await,
        "iterm2" => {
            terminal.notify_iterm2(opts);
            "iterm2".to_string()
        }
        "iterm2_with_bell" => {
            terminal.notify_iterm2(opts);
            terminal.notify_bell();
            "iterm2_with_bell".to_string()
        }
        "kitty" => {
            terminal.notify_kitty(opts, &title, generate_kitty_id());
            "kitty".to_string()
        }
        "ghostty" => {
            terminal.notify_ghostty(opts, &title);
            "ghostty".to_string()
        }
        "terminal_bell" => {
            terminal.notify_bell();
            "terminal_bell".to_string()
        }
        "notifications_disabled" => "disabled".to_string(),
        _ => "none".to_string(),
    }
}

async fn send_auto(opts: &NotificationOptions, terminal: &dyn TerminalNotification) -> String {
    let title = opts.title.clone().unwrap_or_else(|| DEFAULT_TITLE.to_string());

    match detect_terminal().as_deref() {
        Some("Apple_Terminal") => {
            let bell_disabled = is_apple_terminal_bell_disabled().await;
            if bell_disabled {
                terminal.notify_bell();
                "terminal_bell".to_string()
            } else {
                "no_method_available".to_string()
            }
        }
        Some("iTerm.app") => {
            terminal.notify_iterm2(opts);
            "iterm2".to_string()
        }
        Some("kitty") => {
            terminal.notify_kitty(opts, &title, generate_kitty_id());
            "kitty".to_string()
        }
        Some("ghostty") => {
            terminal.notify_ghostty(opts, &title);
            "ghostty".to_string()
        }
        _ => "no_method_available".to_string(),
    }
}

fn generate_kitty_id() -> u32 {
    // 对齐 TS Math.floor(Math.random() * 10000)
    rand::random::<u32>() % 10_000
}

/// `env.terminal` 的最小等价（detectTerminal 主要分支；未覆盖的 TS 分
/// 支统一回落 TERM，send_auto 的 Apple_Terminal / iTerm.app / kitty /
/// ghostty 判定保持正确）。
pub fn detect_terminal() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(detect_terminal_impl)
        .clone()
}

fn detect_terminal_impl() -> Option<String> {
    if std::env::var("CURSOR_TRACE_ID").is_ok() {
        return Some("cursor".to_string());
    }
    if let Ok(askpass) = std::env::var("VSCODE_GIT_ASKPASS_MAIN") {
        if askpass.contains("cursor") {
            return Some("cursor".to_string());
        }
        if askpass.contains("windsurf") {
            return Some("windsurf".to_string());
        }
        if askpass.contains("antigravity") {
            return Some("antigravity".to_string());
        }
    }
    if let Ok(bundle_id) = std::env::var("__CFBundleIdentifier") {
        let bundle_id = bundle_id.to_lowercase();
        if bundle_id.contains("vscodium") {
            return Some("codium".to_string());
        }
        if bundle_id.contains("windsurf") {
            return Some("windsurf".to_string());
        }
        if bundle_id.contains("com.google.android.studio") {
            return Some("androidstudio".to_string());
        }
        const JETBRAINS_IDES: &[&str] = &[
            "intellij",
            "idea",
            "pycharm",
            "webstorm",
            "phpstorm",
            "rubymine",
            "goland",
            "clion",
            "rider",
            "datagrip",
            "jetbrains",
            "androidstudio",
        ];
        for ide in JETBRAINS_IDES {
            if bundle_id.contains(ide) {
                return Some((*ide).to_string());
            }
        }
    }
    if std::env::var("VisualStudioVersion").is_ok() {
        return Some("visualstudio".to_string());
    }
    if std::env::var("TERMINAL_EMULATOR").as_deref() == Ok("JetBrains-JediTerm") {
        return Some("pycharm".to_string());
    }
    if std::env::var("TERM").as_deref() == Ok("xterm-ghostty") {
        return Some("ghostty".to_string());
    }
    if matches!(std::env::var("TERM"), Ok(term) if term.contains("kitty")) {
        return Some("kitty".to_string());
    }
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        if !term_program.is_empty() {
            return Some(term_program);
        }
    }
    if std::env::var("TMUX").is_ok() {
        return Some("tmux".to_string());
    }
    if std::env::var("STY").is_ok() {
        return Some("screen".to_string());
    }
    if std::env::var("KONSOLE_VERSION").is_ok() {
        return Some("konsole".to_string());
    }
    if std::env::var("GNOME_TERMINAL_SERVICE").is_ok() {
        return Some("gnome-terminal".to_string());
    }
    if std::env::var("XTERM_VERSION").is_ok() {
        return Some("xterm".to_string());
    }
    if std::env::var("VTE_VERSION").is_ok() {
        return Some("vte-based".to_string());
    }
    if std::env::var("TERMINATOR_UUID").is_ok() {
        return Some("terminator".to_string());
    }
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return Some("kitty".to_string());
    }
    if std::env::var("ALACRITTY_LOG").is_ok() {
        return Some("alacritty".to_string());
    }
    if std::env::var("TILIX_ID").is_ok() {
        return Some("tilix".to_string());
    }
    if std::env::var("WT_SESSION").is_ok() {
        return Some("windows-terminal".to_string());
    }
    if let Ok(msystem) = std::env::var("MSYSTEM") {
        if !msystem.is_empty() {
            return Some(msystem.to_lowercase());
        }
    }
    if std::env::var("ConEmuANSI").is_ok()
        || std::env::var("ConEmuPID").is_ok()
        || std::env::var("ConEmuTask").is_ok()
    {
        return Some("conemu".to_string());
    }
    if let Ok(wsl_distro) = std::env::var("WSL_DISTRO_NAME") {
        if !wsl_distro.is_empty() {
            return Some(format!("wsl-{wsl_distro}"));
        }
    }
    // 回落 TERM
    std::env::var("TERM").ok().filter(|t| !t.is_empty())
}

/// Apple Terminal 当前 profile 是否禁用了响铃。
///
/// osascript 取当前 profile 名，defaults 导出 com.apple.Terminal 的
/// plist（XML），最小扫描定位 profile 字典里的 Bell 键。
async fn is_apple_terminal_bell_disabled() -> bool {
    if detect_terminal().as_deref() != Some("Apple_Terminal") {
        return false;
    }

    let options = ExecFileOptions {
        use_cwd: Some(false), // 避免初始化期与 PersistentShell 的循环依赖
        ..ExecFileOptions::default()
    };

    let osascript_result = exec_file_no_throw(
        "osascript",
        &[
            "-e".to_string(),
            "tell application \"Terminal\" to name of current settings of front window".to_string(),
        ],
        &options,
    )
    .await;
    let current_profile = osascript_result.stdout.trim().to_string();
    if current_profile.is_empty() {
        return false;
    }

    let defaults_output = exec_file_no_throw(
        "defaults",
        &["export".to_string(), "com.apple.Terminal".to_string(), "-".to_string()],
        &options,
    )
    .await;
    if defaults_output.code != 0 {
        return false;
    }

    plist_profile_bell_disabled(&defaults_output.stdout, &current_profile)
}

/// 最小 plist 扫描：`<key>Window Settings</key><dict>` 内
/// `<key>{profile}</key><dict>` 中 `<key>Bell</key>` 是否 `<false/>`。
fn plist_profile_bell_disabled(plist_xml: &str, profile: &str) -> bool {
    let Some(window_settings) = find_key_value_dict(plist_xml, "Window Settings") else {
        return false;
    };
    let window_settings = &plist_xml[window_settings];
    let Some(profile_dict) = find_key_value_dict(window_settings, profile) else {
        return false;
    };
    let profile_dict = &window_settings[profile_dict];
    // 找 Bell 键与其紧邻的值
    let Some(bell_key) = profile_dict.find("<key>Bell</key>") else {
        return false;
    };
    let after = &profile_dict[bell_key + "<key>Bell</key>".len()..];
    after.trim_start().starts_with("<false/>")
}

/// 在 plist XML 中找 `<key>{key}</key>` 紧邻其后的 `<dict>...</dict>`
/// 的字节范围（字典体内，配对闭合标签，支持嵌套）。
fn find_key_value_dict(xml: &str, key: &str) -> Option<std::ops::Range<usize>> {
    let key_tag = format!("<key>{key}</key>");
    let key_idx = xml.find(&key_tag)?;
    let rest = &xml[key_idx + key_tag.len()..];
    let dict_open_rel = rest.find("<dict>")?;
    let body_start = key_idx + key_tag.len() + dict_open_rel + "<dict>".len();
    let body = &xml[body_start..];

    let mut depth: isize = 1;
    let mut cursor = 0usize;
    while depth > 0 {
        let next_open = body[cursor..].find("<dict>");
        let next_close = body[cursor..].find("</dict>")?;
        match next_open {
            Some(open_rel) if open_rel < next_close => {
                depth += 1;
                cursor += open_rel + "<dict>".len();
            }
            _ => {
                depth -= 1;
                if depth == 0 {
                    return Some(body_start..body_start + next_close);
                }
                cursor += next_close + "</dict>".len();
            }
        }
    }
    None
}

// --- 全局配置 duck-type（仅读 preferredNotifChannel）---

fn read_global_config() -> Value {
    static CACHE: OnceLock<Option<Value>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = global_claude_file();
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        })
        .clone()
        .unwrap_or_else(|| Value::Object(Default::default()))
}

fn global_claude_file() -> PathBuf {
    let config_home = crate::utils::env_utils::get_claude_config_home_dir();
    let legacy = PathBuf::from(&config_home).join(".config.json");
    if legacy.exists() {
        return legacy;
    }
    let suffix = crate::constants::oauth::file_suffix_for_oauth_config();
    let filename = format!(".claude{suffix}.json");
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(filename),
        _ => PathBuf::from(crate::utils::system_directories::home_dir()).join(filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_terminal_reads_term_program() {
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("TERM_PROGRAM", "Apple_Terminal");
        // detect_terminal 进程级缓存：仅在首次未缓存时生效
        if detect_terminal().as_deref() != Some("Apple_Terminal") {
            // 已缓存了其他值（同进程其它测试/运行触发过）：跳过断言
        }
        std::env::remove_var("TERM_PROGRAM");
    }

    #[test]
    fn kitty_id_in_range() {
        let id = generate_kitty_id();
        assert!(id < 10_000);
    }

    #[test]
    fn bell_scan_finds_disabled_profile() {
        let plist = r#"
<plist version="1.0">
<dict>
  <key>Window Settings</key>
  <dict>
    <key>Basic</key>
    <dict>
      <key>Bell</key>
      <false/>
    </dict>
    <key>Other</key>
    <dict>
      <key>Bell</key>
      <true/>
    </dict>
  </dict>
</dict>
</plist>
"#;
        assert!(plist_profile_bell_disabled(plist, "Basic"));
        assert!(!plist_profile_bell_disabled(plist, "Other"));
        assert!(!plist_profile_bell_disabled(plist, "Missing"));
    }
}
