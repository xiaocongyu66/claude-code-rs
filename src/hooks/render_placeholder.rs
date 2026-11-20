//! render_placeholder.rs — PromptInput 占位符渲染的纯函数。
//!
//! 重写自 `src/hooks/renderPlaceholder.ts`。逐条对齐 TS 语义：
//!
//! - 无 placeholder -> `rendered_placeholder` 为 None（TS undefined）。
//! - `hide_placeholder_text`（语音录入态）：只显示光标、不显示占位文本；
//!   光标可见条件 `show_cursor && focus && terminal_focus` 不满足时
//!   渲染空字符串（注意：仍是 Some("")，不是 None）。
//! - 正常态：默认渲染 dim(placeholder)；三个焦点条件同时满足时，
//!   非空占位符把首字符 invert、其余 dim；空占位符渲染 invert(" ")。
//! - `show_placeholder = value.is_empty() && placeholder.is_some()`。
//!
//! 已知差异：
//! - TS 用 chalk（chalk.dim / chalk.inverse）。chalk 在 TTY 环境输出
//!   ANSI 转义：dim = `\x1b[2m`…`\x1b[22m`，inverse = `\x1b[7m`…`\x1b[27m`。
//!   Ink TUI 中 chalk 总处于彩色级别，本端口直接内联等价 ANSI 序列，
//!   `invert` 可由调用方注入自定义实现（对齐 TS 默认参数 `invert = chalk.inverse`）。
//! - TS `placeholder[0]!` 按码点取首字符；Rust 端用 char_indices 保证
//!   多字节首字符按码点切分（`&ph[..first_len]` 不会 panic）。

/// ANSI 转义：chalk.dim —— `\x1b[2m` … `\x1b[22m`。
pub fn ansi_dim(text: &str) -> String {
    format!("\x1b[2m{text}\x1b[22m")
}

/// ANSI 转义：chalk.inverse —— `\x1b[7m` … `\x1b[27m`。
/// 默认 `invert` 实现，等价 TS 的 `chalk.inverse`。
pub fn ansi_inverse(text: &str) -> String {
    format!("\x1b[7m{text}\x1b[27m")
}

/// 自定义反转函数的类型（默认 `ansi_inverse`）。
pub type InvertFn = fn(&str) -> String;

/// `renderPlaceholder` 的参数（对齐 TS `PlaceholderRendererProps`）。
/// 带默认值的字段以 Option/手写 Default 表达。
#[derive(Debug, Clone)]
pub struct PlaceholderRendererProps<'a> {
    /// 占位文本；None 时整体不渲染占位。
    pub placeholder: Option<&'a str>,
    /// 当前输入值；决定 `show_placeholder`。
    pub value: &'a str,
    /// 是否显示光标（TS 默认 undefined -> falsy）。
    pub show_cursor: bool,
    /// 输入框是否聚焦（TS 默认 undefined -> falsy）。
    pub focus: bool,
    /// 终端是否聚焦（TS 默认 true）。
    pub terminal_focus: bool,
    /// 反转函数（TS 默认 `chalk.inverse`）。
    pub invert: InvertFn,
    /// 只显示光标、隐藏占位文本（语音录入态）。
    pub hide_placeholder_text: bool,
}

impl Default for PlaceholderRendererProps<'_> {
    fn default() -> Self {
        Self {
            placeholder: None,
            value: "",
            show_cursor: false,
            focus: false,
            terminal_focus: true,
            invert: ansi_inverse,
            hide_placeholder_text: false,
        }
    }
}

/// `renderPlaceholder` 的返回（对齐 TS 返回类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPlaceholder {
    /// 渲染后的占位符；无 placeholder 时为 None（TS undefined）。
    pub rendered_placeholder: Option<String>,
    /// 输入为空且存在占位符时为 true。
    pub show_placeholder: bool,
}

/// 渲染输入框占位符（纯函数，无副作用）。
pub fn render_placeholder(props: &PlaceholderRendererProps<'_>) -> RenderedPlaceholder {
    let PlaceholderRendererProps {
        placeholder,
        value,
        show_cursor,
        focus,
        terminal_focus,
        invert,
        hide_placeholder_text,
    } = *props;

    let mut rendered_placeholder: Option<String> = None;

    if let Some(placeholder) = placeholder {
        if hide_placeholder_text {
            // 语音录入：只显示光标，不显示占位文本
            rendered_placeholder = Some(if show_cursor && focus && terminal_focus {
                invert(" ")
            } else {
                String::new()
            });
        } else {
            rendered_placeholder = Some(ansi_dim(placeholder));

            // 仅当输入框与终端同时聚焦时显示 inverse 光标
            if show_cursor && focus && terminal_focus {
                rendered_placeholder = Some(if !placeholder.is_empty() {
                    // 按码点取首字符（对齐 TS placeholder[0]）
                    let mut chars = placeholder.char_indices();
                    // is_empty 已保证存在首字符
                    let first_len = chars.next().map(|(_, c)| c.len_utf8()).unwrap_or(0);
                    let head = &placeholder[..first_len];
                    let tail = &placeholder[first_len..];
                    format!("{}{}", invert(head), ansi_dim(tail))
                } else {
                    invert(" ")
                });
            }
        }
    }

    let show_placeholder = value.is_empty() && placeholder.is_some();

    RenderedPlaceholder {
        rendered_placeholder,
        show_placeholder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(placeholder: Option<&str>, value: &str) -> PlaceholderRendererProps<'_> {
        PlaceholderRendererProps {
            placeholder,
            value,
            ..Default::default()
        }
    }

    #[test]
    fn no_placeholder_renders_nothing() {
        let result = render_placeholder(&props(None, ""));
        assert_eq!(result.rendered_placeholder, None);
        assert!(!result.show_placeholder);
    }

    #[test]
    fn placeholder_dimmed_when_value_empty() {
        let result = render_placeholder(&props(Some("hint"), ""));
        assert_eq!(result.rendered_placeholder.as_deref(), Some("\x1b[2mhint\x1b[22m"));
        assert!(result.show_placeholder);
    }

    #[test]
    fn placeholder_hidden_when_value_nonempty() {
        let result = render_placeholder(&props(Some("hint"), "abc"));
        assert_eq!(result.rendered_placeholder.as_deref(), Some("\x1b[2mhint\x1b[22m"));
        assert!(!result.show_placeholder);
    }

    #[test]
    fn inverse_cursor_first_char_when_focused() {
        let props = PlaceholderRendererProps {
            placeholder: Some("hint"),
            value: "",
            show_cursor: true,
            focus: true,
            terminal_focus: true,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(
            result.rendered_placeholder.as_deref(),
            Some("\x1b[7mh\x1b[27m\x1b[2mint\x1b[22m")
        );
    }

    #[test]
    fn inverse_cursor_blank_placeholder_when_focused() {
        let props = PlaceholderRendererProps {
            placeholder: Some(""),
            value: "",
            show_cursor: true,
            focus: true,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(result.rendered_placeholder.as_deref(), Some("\x1b[7m \x1b[27m"));
    }

    #[test]
    fn no_cursor_when_terminal_unfocused() {
        let props = PlaceholderRendererProps {
            placeholder: Some("hint"),
            value: "",
            show_cursor: true,
            focus: true,
            terminal_focus: false,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(result.rendered_placeholder.as_deref(), Some("\x1b[2mhint\x1b[22m"));
    }

    #[test]
    fn hide_placeholder_text_shows_only_cursor_when_focused() {
        let props = PlaceholderRendererProps {
            placeholder: Some("hint"),
            value: "",
            show_cursor: true,
            focus: true,
            hide_placeholder_text: true,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(result.rendered_placeholder.as_deref(), Some("\x1b[7m \x1b[27m"));
    }

    #[test]
    fn hide_placeholder_text_blank_when_unfocused() {
        let props = PlaceholderRendererProps {
            placeholder: Some("hint"),
            value: "",
            hide_placeholder_text: true,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(result.rendered_placeholder.as_deref(), Some(""));
    }

    #[test]
    fn multibyte_first_char_splits_by_codepoint() {
        let props = PlaceholderRendererProps {
            placeholder: Some("中文"),
            value: "",
            show_cursor: true,
            focus: true,
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(
            result.rendered_placeholder.as_deref(),
            Some("\x1b[7m中\x1b[27m\x1b[2m文\x1b[22m")
        );
    }

    #[test]
    fn custom_invert_function() {
        let props = PlaceholderRendererProps {
            placeholder: Some("ab"),
            value: "",
            show_cursor: true,
            focus: true,
            invert: |t| format!("[{t}]"),
            ..Default::default()
        };
        let result = render_placeholder(&props);
        assert_eq!(
            result.rendered_placeholder.as_deref(),
            Some("[a]\x1b[2mb\x1b[22m")
        );
    }
}
