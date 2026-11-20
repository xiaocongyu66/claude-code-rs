//! env_expansion.rs — MCP 服务器配置中的环境变量展开
//! （重写自 `envExpansion.ts`）。
//!
//! 语义对齐：展开 `${VAR}` 与 `${VAR:-default}` 语法。
//! 未定义且无默认值的变量保持原样并记录到 `missing_vars`，供上层报错。

use once_cell::sync::Lazy;
use regex::Regex;

/// 展开结果：展开后的字符串 + 缺失变量列表。
pub struct ExpandEnvResult {
    pub expanded: String,
    pub missing_vars: Vec<String>,
}

static ENV_VAR_RE: Lazy<Option<Regex>> =
    Lazy::new(|| Regex::new(r"\$\{([^}]+)\}").ok());

/// 展开字符串中的环境变量（`${VAR}` / `${VAR:-default}`）。
pub fn expand_env_vars_in_string(value: &str) -> ExpandEnvResult {
    let mut missing_vars: Vec<String> = Vec::new();

    let expanded = match ENV_VAR_RE.as_ref() {
        Some(rx) => rx
            .replace_all(value, |caps: &regex::Captures| {
                let var_content = &caps[1];
                // 按 ':-' 拆两段（保留默认值中的 ':-'）
                let mut split = var_content.splitn(2, ":-");
                let var_name = split.next().unwrap_or("");
                let default_value = split.next();

                if let Ok(env_value) = std::env::var(var_name) {
                    return env_value;
                }
                if let Some(default) = default_value {
                    return default.to_string();
                }

                // 记录缺失变量；原样返回（调试可见，上层报错）
                missing_vars.push(var_name.to_string());
                caps[0].to_string()
            })
            .into_owned(),
        None => value.to_string(),
    };

    ExpandEnvResult {
        expanded,
        missing_vars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_present_var() {
        std::env::set_var("MCP_TEST_VAR", "value1");
        let r = expand_env_vars_in_string("x${MCP_TEST_VAR}y");
        assert_eq!(r.expanded, "xvalue1y");
        assert!(r.missing_vars.is_empty());
        std::env::remove_var("MCP_TEST_VAR");
    }

    #[test]
    fn uses_default_when_missing() {
        let r = expand_env_vars_in_string("${MCP_MISSING_VAR:-fallback}");
        assert_eq!(r.expanded, "fallback");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn tracks_missing_vars() {
        let r = expand_env_vars_in_string("a${NOPE1}b");
        assert_eq!(r.expanded, "a${NOPE1}b");
        assert_eq!(r.missing_vars, vec!["NOPE1".to_string()]);
    }
}
