//! display_tags.rs — 展示用标题中的 XML 标签块剥离。
//!
//! 重写自 `src/utils/displayTags.ts`。匹配任意 `<tag>…</tag>` 形态的
//! 小写标签块（含可选属性、跨行内容），用于把系统注入的包装标签从
//! 展示标题中剥掉（IDE 上下文、斜杠命令标记、hook 输出、任务通知、
//! 频道消息等）。
//!
//! Cargo.toml 暂无 regex 依赖，改为手写扫描器，精确对齐 TS 的
//! `/<([a-z][\w-]*)(?:\s[^>]*)?>[\s\S]*?<\/\1>\n?/g`：
//! - 仅小写标签名 `[a-z][\w-]*`（用户文本提到 `<Button>`、`<!DOCTYPE>`
//!   时不会误剥）
//! - 非贪婪正文 + 回溯引用闭合标签（相邻块分开；不成对的 `<` 不匹配）
//! - 匹配块后最多吃掉一个换行

use std::borrow::Cow;

/// 剥离 XML 类标签块；结果 trim 后若为空则返回原文（"有总比没有强"）。
pub fn strip_display_tags(text: &str) -> Cow<'_, str> {
    match strip_tags(text, None) {
        Cow::Borrowed(stripped) => {
            let trimmed = stripped.trim();
            if trimmed.is_empty() {
                Cow::Borrowed(text)
            } else {
                Cow::Borrowed(trimmed)
            }
        }
        Cow::Owned(stripped) => {
            let trimmed = stripped.trim();
            if trimmed.is_empty() {
                Cow::Borrowed(text)
            } else {
                Cow::Owned(trimmed.to_string())
            }
        }
    }
}

/// 同 strip_display_tags，但全部内容都是标签时返回空串。
/// 用于 getLogDisplayTitle 检测纯命令 prompt（如 /clear）等的回退逻辑。
pub fn strip_display_tags_allow_empty(text: &str) -> Cow<'_, str> {
    match strip_tags(text, None) {
        Cow::Borrowed(stripped) => Cow::Borrowed(stripped.trim()),
        Cow::Owned(stripped) => Cow::Owned(stripped.trim().to_string()),
    }
}

/// 只剥离 IDE 注入的上下文标签（ide_opened_file / ide_selection）。
/// 用于 UP 箭头重发时保留用户输入（含 `<code>foo</code>` 这类小写 HTML）。
pub fn strip_ide_context_tags(text: &str) -> Cow<'_, str> {
    match strip_tags(text, Some(&["ide_opened_file", "ide_selection"])) {
        Cow::Borrowed(stripped) => Cow::Borrowed(stripped.trim()),
        Cow::Owned(stripped) => Cow::Owned(stripped.trim().to_string()),
    }
}

/// 扫描器实现 TS 的全局 replace 语义（Cow：无匹配时零拷贝）。
fn strip_tags<'a>(text: &'a str, allowed: Option<&[&str]>) -> Cow<'a, str> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some((tag_end, name)) = parse_open_tag(bytes, i) {
                if matches_tag_name(&name, allowed) {
                    // 非贪婪查找 `</name>`
                    if let Some(block_end) = find_closing_tag(bytes, tag_end, &name) {
                        i = consume_newline(bytes, block_end);
                        continue;
                    }
                }
            }
        }
        // 解析失败 / 不匹配：复制到下一个 '<'。
        // 从 i+1 开始查找保证循环始终前进；'<' 是 ASCII，切片永远落在
        // UTF-8 边界上。
        let next = bytes[i + 1..]
            .iter()
            .position(|&b| b == b'<')
            .map(|p| i + 1 + p)
            .unwrap_or(bytes.len());
        out.push_str(&text[i..next]);
        i = next;
    }

    if out.is_empty() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// 解析 `<name ...>` 开标签；返回（`>` 之后位置, 标签名）。
/// 名字必须为 `[a-z][\w-]*`，后跟 `>` 或空白 + 属性（不含 `>`）+ `>`
///（对应正则的 `(?:\s[^>]*)?`）。
fn parse_open_tag(bytes: &[u8], start: usize) -> Option<(usize, String)> {
    let mut i = start + 1; // 跳过 '<'
    let name_start = i;
    // 首字符必须 a-z
    if i >= bytes.len() || !(b'a'..=b'z').contains(&bytes[i]) {
        return None;
    }
    i += 1;
    // 后续 [\w-]*：字母、数字、下划线、连字符
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
    {
        i += 1;
    }
    let name = String::from_utf8_lossy(&bytes[name_start..i]).into_owned();

    if i < bytes.len() && bytes[i] == b'>' {
        return Some((i + 1, name));
    }
    if i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        let attr = bytes[i..].iter().position(|&b| b == b'>')?;
        return Some((i + attr + 1, name));
    }
    None
}

/// 在 `from` 之后查找第一个 `</name>`；返回其结束位置（对齐非贪婪语义）。
fn find_closing_tag(bytes: &[u8], from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{}>", name);
    let n = needle.as_bytes();
    let mut search = from;
    while search + n.len() <= bytes.len() {
        if &bytes[search..search + n.len()] == n {
            return Some(search + n.len());
        }
        search += 1;
    }
    None
}

/// `</tag>` 后最多吃掉一个 `\n`（对齐正则的 `\n?`）。
fn consume_newline(bytes: &[u8], pos: usize) -> usize {
    if pos < bytes.len() && bytes[pos] == b'\n' {
        pos + 1
    } else {
        pos
    }
}

fn matches_tag_name(name: &str, allowed: Option<&[&str]>) -> bool {
    match allowed {
        Some(list) => list.contains(&name),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_simple_tags() {
        assert_eq!(
            strip_display_tags("<ide_opened_file>x</ide_opened_file>Hello"),
            "Hello"
        );
        assert_eq!(strip_display_tags("a<task>x</task>b"), "ab");
    }

    #[test]
    fn strips_tag_with_attributes_and_newline() {
        assert_eq!(
            strip_display_tags("Hello\n<channel id=\"1\">msg</channel>\nWorld"),
            "Hello\nWorld"
        );
    }

    #[test]
    fn handles_multiline_and_adjacent_blocks() {
        assert_eq!(
            strip_display_tags("<one>a\nb</one><two>c</two>tail"),
            "tail"
        );
    }

    #[test]
    fn ignores_uppercase_and_doctype() {
        assert_eq!(
            strip_display_tags("fix the <Button> layout"),
            "fix the <Button> layout"
        );
        assert_eq!(
            strip_display_tags("see <!DOCTYPE html> doc"),
            "see <!DOCTYPE html> doc"
        );
    }

    #[test]
    fn unpaired_angle_brackets_pass_through() {
        assert_eq!(strip_display_tags("when x < y"), "when x < y");
    }

    #[test]
    fn all_tags_returns_original_when_empty_result() {
        assert_eq!(strip_display_tags("<task>x</task>"), "<task>x</task>");
        assert_eq!(strip_display_tags_allow_empty("<task>x</task>"), "");
    }

    #[test]
    fn ide_only_strips_ide_tags() {
        assert_eq!(
            strip_ide_context_tags("<ide_selection>sel</ide_selection><code>foo</code>"),
            "<code>foo</code>"
        );
    }
}
