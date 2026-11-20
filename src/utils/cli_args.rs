//! cli_args.rs — Commander.js 之前的早期 CLI 参数解析。
//!
//! 重写自 `src/utils/cliArgs.ts`。支持空格分隔（`--flag value`）与
//! 等号分隔（`--flag=value`）两种语法。用于必须在 init() 之前解析的
//! flag（如 --settings）。

use std::env;

/// 在 argv 中提前解析某个 flag 的值。
///
/// - `--flag=value`：返回等号后的内容
/// - `--flag value`：返回下一个 argv 元素
/// 未找到返回 `None`。
pub fn eager_parse_cli_flag(flag_name: &str, argv: &[String]) -> Option<String> {
    for (i, arg) in argv.iter().enumerate() {
        if let Some(rest) = arg.strip_prefix(&format!("{flag_name}=")) {
            return Some(rest.to_string());
        }
        if arg == flag_name {
            if let Some(next) = argv.get(i + 1) {
                return Some(next.clone());
            }
        }
    }
    None
}

/// 用进程实际 argv 解析（对齐 TS 的默认参数 `process.argv`）。
pub fn eager_parse_cli_flag_from_env(flag_name: &str) -> Option<String> {
    eager_parse_cli_flag(flag_name, &env::args().collect::<Vec<_>>())
}

/// 处理 Unix `--` 分隔符约定。
///
/// Commander.js 的 `.passThroughOptions()` 会把 `--` 作为位置参数透传，
/// 当位置参数为 `--` 时，真正的命令在 args 数组的首个元素。
pub fn extract_args_after_double_dash(
    command_or_value: &str,
    args: &[String],
) -> (String, Vec<String>) {
    if command_or_value == "--" && !args.is_empty() {
        let mut rest = args.to_vec();
        let command = rest.remove(0);
        return (command, rest);
    }
    (command_or_value.to_string(), args.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_equals_form() {
        let argv = ["--settings=/tmp/x.json".to_string()];
        assert_eq!(
            eager_parse_cli_flag("--settings", &argv),
            Some("/tmp/x.json".to_string())
        );
    }

    #[test]
    fn parses_space_form() {
        let argv = ["--settings".to_string(), "/tmp/x.json".to_string()];
        assert_eq!(
            eager_parse_cli_flag("--settings", &argv),
            Some("/tmp/x.json".to_string())
        );
    }

    #[test]
    fn missing_flag() {
        let argv = ["--other".to_string()];
        assert_eq!(eager_parse_cli_flag("--settings", &argv), None);
    }

    #[test]
    fn double_dash_extraction() {
        let args = [
            "subcmd".to_string(),
            "--flag".to_string(),
            "arg".to_string(),
        ];
        let (cmd, rest) = extract_args_after_double_dash("--", &args);
        assert_eq!(cmd, "subcmd");
        assert_eq!(rest, vec!["--flag".to_string(), "arg".to_string()]);

        let (cmd, rest) = extract_args_after_double_dash("name", &args);
        assert_eq!(cmd, "name");
        assert_eq!(rest, args);
    }
}
