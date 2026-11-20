//! slash_command_parsing.rs — 斜杠命令解析工具。
//!
//! 重写自 `src/utils/slashCommandParsing.ts`。集中处理 `/command args`
//! 与 `/mcp:tool (MCP) args` 两种形态的输入拆分。

/// 解析结果：命令名、参数与 MCP 标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSlashCommand {
    pub command_name: String,
    pub args: String,
    pub is_mcp: bool,
}

/// 解析斜杠命令输入。
///
/// 输入须以 `/` 开头（先 trim 空白）；第二个词为 `(MCP)` 时识别为
/// MCP 命令。无效输入（不以 `/` 开头、`/` 后无命令名）返回 `None`。
///
/// 对齐 TS 示例：
/// - `/search foo bar` → `{ command_name: "search", args: "foo bar", is_mcp: false }`
/// - `/mcp:tool (MCP) arg1 arg2` → `{ command_name: "mcp:tool (MCP)", args: "arg1 arg2", is_mcp: true }`
pub fn parse_slash_command(input: &str) -> Option<ParsedSlashCommand> {
    let trimmed = input.trim();

    if !trimmed.starts_with('/') {
        return None;
    }

    // 去掉开头的 '/' 后按单空格切分，保留空串元素以忠实还原 TS 的
    // `split(' ')`（"a  b" → ["a", "", "b"]，args join 后含连续空格）。
    let without_slash: &str = &trimmed[1..];
    let words: Vec<&str> = without_slash.split(' ').collect();

    let first = words.first().copied().unwrap_or_default();
    if first.is_empty() {
        return None;
    }

    let mut command_name = first.to_string();
    let mut is_mcp = false;
    let args_start_index = if words.len() > 1 && words[1] == "(MCP)" {
        command_name.push_str(" (MCP)");
        is_mcp = true;
        2
    } else {
        1
    };

    let args = words[args_start_index..].join(" ");

    Some(ParsedSlashCommand {
        command_name,
        args,
        is_mcp,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_slash_command;

    #[test]
    fn parses_plain_command() {
        let parsed = parse_slash_command("/search foo bar").unwrap();
        assert_eq!(parsed.command_name, "search");
        assert_eq!(parsed.args, "foo bar");
        assert!(!parsed.is_mcp);
    }

    #[test]
    fn parses_mcp_command() {
        let parsed = parse_slash_command("/mcp:tool (MCP) arg1 arg2").unwrap();
        assert_eq!(parsed.command_name, "mcp:tool (MCP)");
        assert_eq!(parsed.args, "arg1 arg2");
        assert!(parsed.is_mcp);
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(parse_slash_command("help me").is_none());
        assert!(parse_slash_command("/").is_none());
        assert!(parse_slash_command("/   ").is_none());
    }

    #[test]
    fn trims_and_preserves_inner_spacing() {
        // 对齐 TS：input.trim() 两端去白；args 为剩余词按单空格 join，
        // 内部连续空格以空串词保留（" report   title"，无尾随空格）。
        let parsed = parse_slash_command("  /bug  report   title  ").unwrap();
        assert_eq!(parsed.command_name, "bug");
        assert_eq!(parsed.args, " report   title");
    }
}
