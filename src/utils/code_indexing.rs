//! code_indexing.rs — 代码索引工具使用检测。
//!
//! 重写自 `src/utils/codeIndexing.ts`。追踪常见代码索引方案
//!（Sourcegraph、Cody 等）的 CLI 命令与 MCP server 集成。
//! Cargo.toml 暂无 regex 依赖，MCP server 名模式改为手写匹配器
//!（原模式均为 `^name$`/`^a[-_]?b$` 这类全量匹配，可精确对齐）。

/// 已知的代码索引工具标识（analytics 事件中的规范化名称）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeIndexingTool {
    Sourcegraph,
    Hound,
    Seagoat,
    Bloop,
    Gitloop,
    Cody,
    Aider,
    Continue,
    GithubCopilot,
    Cursor,
    Tabby,
    Codeium,
    Tabnine,
    Augment,
    Windsurf,
    Aide,
    Pieces,
    Qodo,
    AmazonQ,
    Gemini,
    ClaudeContext,
    CodeIndexMcp,
    LocalCodeSearch,
    AutodevCodebase,
    Openctx,
}

impl CodeIndexingTool {
    /// analytics 事件中使用的规范化名称。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sourcegraph => "sourcegraph",
            Self::Hound => "hound",
            Self::Seagoat => "seagoat",
            Self::Bloop => "bloop",
            Self::Gitloop => "gitloop",
            Self::Cody => "cody",
            Self::Aider => "aider",
            Self::Continue => "continue",
            Self::GithubCopilot => "github-copilot",
            Self::Cursor => "cursor",
            Self::Tabby => "tabby",
            Self::Codeium => "codeium",
            Self::Tabnine => "tabnine",
            Self::Augment => "augment",
            Self::Windsurf => "windsurf",
            Self::Aide => "aide",
            Self::Pieces => "pieces",
            Self::Qodo => "qodo",
            Self::AmazonQ => "amazon-q",
            Self::Gemini => "gemini",
            Self::ClaudeContext => "claude-context",
            Self::CodeIndexMcp => "code-index-mcp",
            Self::LocalCodeSearch => "local-code-search",
            Self::AutodevCodebase => "autodev-codebase",
            Self::Openctx => "openctx",
        }
    }
}

/// CLI 命令首词 → 工具。MCP server 名 → 工具。二者大小写不敏感。
fn cli_lookup(word: &str) -> Option<CodeIndexingTool> {
    match word {
        // Sourcegraph 生态
        "src" => Some(CodeIndexingTool::Sourcegraph),
        "cody" => Some(CodeIndexingTool::Cody),
        // AI 编程助手
        "aider" => Some(CodeIndexingTool::Aider),
        "tabby" => Some(CodeIndexingTool::Tabby),
        "tabnine" => Some(CodeIndexingTool::Tabnine),
        "augment" => Some(CodeIndexingTool::Augment),
        "pieces" => Some(CodeIndexingTool::Pieces),
        "qodo" => Some(CodeIndexingTool::Qodo),
        "aide" => Some(CodeIndexingTool::Aide),
        // 代码搜索工具
        "hound" => Some(CodeIndexingTool::Hound),
        "seagoat" => Some(CodeIndexingTool::Seagoat),
        "bloop" => Some(CodeIndexingTool::Bloop),
        "gitloop" => Some(CodeIndexingTool::Gitloop),
        // 云厂商 AI 助手
        "q" => Some(CodeIndexingTool::AmazonQ),
        "gemini" => Some(CodeIndexingTool::Gemini),
        _ => None,
    }
}

/// 校验 MCP server 名是否匹配某个模式（大小写不敏感；`-` 与 `_` 等价）。
fn mcp_matches(server: &str, pattern: &str) -> bool {
    let normalize = |s: &str| s.to_ascii_lowercase().replace('_', "-");
    normalize(server) == normalize(pattern)
}

fn mcp_lookup(server: &str) -> Option<CodeIndexingTool> {
    // 顺序与 TS 的 MCP_SERVER_PATTERNS 一致；前者更特异。
    const PATTERNS: &[(&str, CodeIndexingTool)] = &[
        // Sourcegraph 生态
        ("sourcegraph", CodeIndexingTool::Sourcegraph),
        ("cody", CodeIndexingTool::Cody),
        ("openctx", CodeIndexingTool::Openctx),
        // AI 编程助手
        ("aider", CodeIndexingTool::Aider),
        ("continue", CodeIndexingTool::Continue),
        ("github-copilot", CodeIndexingTool::GithubCopilot),
        ("copilot", CodeIndexingTool::GithubCopilot),
        ("cursor", CodeIndexingTool::Cursor),
        ("tabby", CodeIndexingTool::Tabby),
        ("codeium", CodeIndexingTool::Codeium),
        ("tabnine", CodeIndexingTool::Tabnine),
        ("augment-code", CodeIndexingTool::Augment),
        ("augment", CodeIndexingTool::Augment),
        ("windsurf", CodeIndexingTool::Windsurf),
        ("aide", CodeIndexingTool::Aide),
        ("codestory", CodeIndexingTool::Aide),
        ("pieces", CodeIndexingTool::Pieces),
        ("qodo", CodeIndexingTool::Qodo),
        ("amazon-q", CodeIndexingTool::AmazonQ),
        ("gemini-code-assist", CodeIndexingTool::Gemini),
        ("gemini", CodeIndexingTool::Gemini),
        // 代码搜索工具
        ("hound", CodeIndexingTool::Hound),
        ("seagoat", CodeIndexingTool::Seagoat),
        ("bloop", CodeIndexingTool::Bloop),
        ("gitloop", CodeIndexingTool::Gitloop),
        // MCP 代码索引 server
        ("claude-context", CodeIndexingTool::ClaudeContext),
        ("code-index-mcp", CodeIndexingTool::CodeIndexMcp),
        ("code-index", CodeIndexingTool::CodeIndexMcp),
        ("local-code-search", CodeIndexingTool::LocalCodeSearch),
        ("codebase", CodeIndexingTool::AutodevCodebase),
        ("autodev-codebase", CodeIndexingTool::AutodevCodebase),
        ("code-context", CodeIndexingTool::ClaudeContext),
    ];
    PATTERNS
        .iter()
        .find(|(pattern, _)| mcp_matches(server, pattern))
        .map(|(_, tool)| *tool)
}

/// 检测 bash 命令是否使用了代码索引 CLI。
///
/// - `src search "pattern"` → Sourcegraph
/// - `cody chat --message "help"` → Cody
/// - `ls -la` → None
pub fn detect_code_indexing_from_command(command: &str) -> Option<CodeIndexingTool> {
    let first_word = command
        .trim()
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();

    // npx/bunx 前缀
    if first_word == "npx" || first_word == "bunx" {
        let second = command
            .trim()
            .split_whitespace()
            .nth(1)
            .map(|w| w.to_ascii_lowercase());
        if let Some(word) = second {
            return cli_lookup(&word);
        }
    }

    cli_lookup(&first_word)
}

/// 检测 MCP server 名是否对应代码索引工具。
pub fn detect_code_indexing_from_mcp_server_name(server_name: &str) -> Option<CodeIndexingTool> {
    mcp_lookup(server_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cli_commands() {
        assert_eq!(
            detect_code_indexing_from_command("src search \"pattern\"").map(|t| t.as_str()),
            Some("sourcegraph")
        );
        assert_eq!(
            detect_code_indexing_from_command("cody chat --message \"help\"").map(|t| t.as_str()),
            Some("cody")
        );
        assert_eq!(detect_code_indexing_from_command("ls -la"), None);
        assert_eq!(
            detect_code_indexing_from_command("npx cody ask").map(|t| t.as_str()),
            Some("cody")
        );
        assert_eq!(
            detect_code_indexing_from_command("npx unrelated-tool"),
            None
        );
    }

    #[test]
    fn detects_mcp_servers() {
        assert_eq!(
            detect_code_indexing_from_mcp_server_name("sourcegraph").map(|t| t.as_str()),
            Some("sourcegraph")
        );
        assert_eq!(
            detect_code_indexing_from_mcp_server_name("GitHub_Copilot").map(|t| t.as_str()),
            Some("github-copilot")
        );
        assert_eq!(
            detect_code_indexing_from_mcp_server_name("augment-code").map(|t| t.as_str()),
            Some("augment")
        );
        assert_eq!(
            detect_code_indexing_from_mcp_server_name("code-context").map(|t| t.as_str()),
            Some("claude-context")
        );
        assert_eq!(
            detect_code_indexing_from_mcp_server_name("filesystem"),
            None
        );
    }
}
