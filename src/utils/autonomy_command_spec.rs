//! autonomy_command_spec.rs — `/autonomy` 斜杠命令的规格常量与参数解析。
//!
//! 重写自 `src/utils/autonomyCommandSpec.ts`。

pub const AUTONOMY_COMMAND_NAME: &str = "autonomy";

pub const AUTONOMY_ARGUMENT_HINT: &str =
    "[status [--deep]|runs [limit]|flows [limit]|flow <id>|flow cancel <id>|flow resume <id>]";

pub const AUTONOMY_USAGE: &str = "Usage: /autonomy [status [--deep]|runs [limit]|flows [limit]|flow <id>|flow cancel <id>|flow resume <id>]";

/// 解析 `/autonomy` 参数得到的命令形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedAutonomyCommand {
    Status { deep: bool },
    Runs { limit: Option<String> },
    Flows { limit: Option<String> },
    FlowDetail { flow_id: String },
    FlowCancel { flow_id: String },
    FlowResume { flow_id: String },
    Usage,
}

/// 解析 `/autonomy` 的参数串（裁剪空白后按空白分词，至多取前三段）。
pub fn parse_autonomy_args(args: &str) -> ParsedAutonomyCommand {
    let mut parts: Vec<&str> = Vec::new();
    for part in args.trim().split_whitespace() {
        if part.is_empty() {
            continue;
        }
        parts.push(part);
        if parts.len() == 3 {
            break;
        }
    }

    let subcommand = parts.first().copied().unwrap_or("status");
    let arg1 = parts.get(1).copied();
    let arg2 = parts.get(2).copied();

    if subcommand.is_empty() || subcommand == "status" {
        return ParsedAutonomyCommand::Status {
            deep: arg1 == Some("--deep"),
        };
    }

    if subcommand == "runs" {
        return ParsedAutonomyCommand::Runs {
            limit: arg1.map(str::to_string),
        };
    }

    if subcommand == "flows" {
        return ParsedAutonomyCommand::Flows {
            limit: arg1.map(str::to_string),
        };
    }

    if subcommand == "flow" {
        if arg1 == Some("cancel") {
            return match arg2 {
                Some(flow_id) => ParsedAutonomyCommand::FlowCancel {
                    flow_id: flow_id.to_string(),
                },
                None => ParsedAutonomyCommand::Usage,
            };
        }
        if arg1 == Some("resume") {
            return match arg2 {
                Some(flow_id) => ParsedAutonomyCommand::FlowResume {
                    flow_id: flow_id.to_string(),
                },
                None => ParsedAutonomyCommand::Usage,
            };
        }
        return match arg1 {
            Some(flow_id) => ParsedAutonomyCommand::FlowDetail {
                flow_id: flow_id.to_string(),
            },
            None => ParsedAutonomyCommand::Usage,
        };
    }

    ParsedAutonomyCommand::Usage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_status() {
        assert_eq!(
            parse_autonomy_args(""),
            ParsedAutonomyCommand::Status { deep: false }
        );
        assert_eq!(
            parse_autonomy_args("   status   "),
            ParsedAutonomyCommand::Status { deep: false }
        );
        assert_eq!(
            parse_autonomy_args("status --deep"),
            ParsedAutonomyCommand::Status { deep: true }
        );
    }

    #[test]
    fn runs_and_flows_take_optional_limit() {
        assert_eq!(
            parse_autonomy_args("runs"),
            ParsedAutonomyCommand::Runs { limit: None }
        );
        assert_eq!(
            parse_autonomy_args("flows 10"),
            ParsedAutonomyCommand::Flows {
                limit: Some("10".into())
            }
        );
    }

    #[test]
    fn flow_subcommands() {
        assert_eq!(
            parse_autonomy_args("flow abc-123"),
            ParsedAutonomyCommand::FlowDetail {
                flow_id: "abc-123".into()
            }
        );
        assert_eq!(
            parse_autonomy_args("flow cancel abc-123"),
            ParsedAutonomyCommand::FlowCancel {
                flow_id: "abc-123".into()
            }
        );
        assert_eq!(
            parse_autonomy_args("flow resume abc-123"),
            ParsedAutonomyCommand::FlowResume {
                flow_id: "abc-123".into()
            }
        );
        assert_eq!(parse_autonomy_args("flow"), ParsedAutonomyCommand::Usage);
        assert_eq!(
            parse_autonomy_args("flow cancel"),
            ParsedAutonomyCommand::Usage
        );
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert_eq!(parse_autonomy_args("bogus"), ParsedAutonomyCommand::Usage);
    }
}
