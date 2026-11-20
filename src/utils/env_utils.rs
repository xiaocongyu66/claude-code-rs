//! env_utils.rs — 环境变量与目录解析工具。
//!
//! 重写自 `src/utils/envUtils.ts`。函数行为、边界条件与错误文案逐条对齐
//! TS 源码。
//!
//! 已知差异：
//! - TS 的 `String.prototype.normalize('NFC')` 需要 unicode-normalization
//!   crate（不在当前依赖白名单内），本端口按原样返回字符串；CLI 涉及的
//!   路径几乎总是已为 NFC。
//! - `is_env_truthy` / `is_env_defined_falsy` 的 TS 签名额外接受 `boolean`；
//!   Rust 端口接受 `Option<&str>`，boolean 分支由调用点短路表达（TS 中
//!   `isEnvTruthy(true)` 即 `true`，无需函数）。
//! - `is_in_protected_namespace` 的 ant 分支依赖未转换的
//!   `utils/protectedNamespace.js`；按外部构建的 DCE 语义返回 `false`
//!   （外部构建中 USER_TYPE !== 'ant'，整段被消除）。
//! - `get_vertex_region_for_model` 的 TS 声明返回 `string | undefined`，
//!   但实现恒返回字符串；本端口忠实于实现，返回 `String`。

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use once_cell::sync::Lazy;

/// `getClaudeConfigHomeDir` 的记忆化缓存。TS 用 lodash memoize，resolver
/// 返回 `process.env.CLAUDE_CONFIG_DIR`——即缓存键是当前的环境变量值，
/// 测试中改变环境变量即可得到新值。此处用 HashMap<原始env值, 结果> 等价表达。
static CONFIG_HOME_CACHE: Lazy<Mutex<HashMap<String, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn cache_lock() -> MutexGuard<'static, HashMap<String, String>> {
    CONFIG_HOME_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 返回 CLAUDE 配置主目录（默认 `~/.claude`，可用 `CLAUDE_CONFIG_DIR` 覆盖）。
///
/// 记忆化：150+ 调用方多数在热路径上，且以 `CLAUDE_CONFIG_DIR` 为缓存键，
/// 测试改变该环境变量时自动获得新值（对齐 TS memoize 的 resolver 语义）。
pub fn get_claude_config_home_dir() -> String {
    let key = env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    if let Some(value) = cache_lock().get(key.as_str()) {
        return value.clone();
    }
    let value = match env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => home_dir().join(".claude"),
    };
    // TS: value.normalize('NFC') —— 见模块头已知差异，按原样返回。
    let value = value.to_string_lossy().into_owned();
    cache_lock().insert(key, value.clone());
    value
}

/// 用户主目录（等价 TS `os.homedir()`）。POSIX 读 `$HOME`，Windows 读
/// `$USERPROFILE`；两者都缺失时回退到相对路径（几乎不会发生）。
fn home_dir() -> PathBuf {
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = env::var_os("USERPROFILE") {
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

/// `~/.claude/teams`。
pub fn get_teams_dir() -> String {
    format!("{}/teams", get_claude_config_home_dir())
}

/// 检查 NODE_OPTIONS 是否包含某个 flag。按空白拆分后精确匹配，避免误判。
pub fn has_node_option(flag: &str) -> bool {
    match env::var("NODE_OPTIONS") {
        Ok(options) => options.split_whitespace().any(|part| part == flag),
        Err(_) => false,
    }
}

/// 环境变量值是否为「真」（'1'/'true'/'yes'/'on'，忽略大小写与前后空白）。
/// 空字符串与缺失视为假。
pub fn is_env_truthy(env_var: Option<&str>) -> bool {
    match env_var {
        None | Some("") => false,
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        }
    }
}

/// 环境变量是否被显式定义为「假」（'0'/'false'/'no'/'off'）。
/// `None`（未设置）返回 false；空字符串也返回 false（TS `if (!envVar) return false`）。
pub fn is_env_defined_falsy(env_var: Option<&str>) -> bool {
    match env_var {
        None => false,
        Some("") => false,
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "0" | "false" | "no" | "off")
        }
    }
}

/// `--bare` / `CLAUDE_CODE_SIMPLE`：跳过 hooks、LSP、插件同步、skill 目录
/// 扫描、attribution、后台预取与所有 keychain/凭证读取。~30 个门禁使用。
///
/// 同时检查 argv：部分门禁（notably startKeychainPrefetch）在 main.tsx
/// 把 `--bare` 写回 `CLAUDE_CODE_SIMPLE=1` 之前就运行。
pub fn is_bare_mode() -> bool {
    is_env_truthy(env::var("CLAUDE_CODE_SIMPLE").ok().as_deref())
        || env::args().any(|arg| arg == "--bare")
}

/// `parse_env_vars` 的错误（文案与 TS 逐字对齐）。
#[derive(Debug, thiserror::Error)]
#[error(
    "Invalid environment variable format: {raw}, environment variables should be added as: -e KEY1=value1 -e KEY2=value2"
)]
pub struct ParseEnvVarsError {
    raw: String,
}

/// 把 `KEY=VALUE` 字符串数组解析为键值对象。
/// 缺少 `=` 的条目抛错（Rust: 返回 `Err`），文案与 TS 一致。
pub fn parse_env_vars(
    raw_env_args: Option<&[String]>,
) -> Result<HashMap<String, String>, ParseEnvVarsError> {
    let mut parsed_env = HashMap::new();
    if let Some(raw_env_args) = raw_env_args {
        for env_str in raw_env_args {
            match env_str.split_once('=') {
                Some((key, value)) if !key.is_empty() => {
                    parsed_env.insert(key.to_string(), value.to_string());
                }
                _ => {
                    return Err(ParseEnvVarsError {
                        raw: env_str.clone(),
                    });
                }
            }
        }
    }
    Ok(parsed_env)
}

/// AWS 区域，带默认回退。与 Anthropic Bedrock SDK 的区域行为一致。
pub fn get_aws_region() -> String {
    env::var("AWS_REGION")
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string())
}

/// Vertex AI 默认区域。
pub fn get_default_vertex_region() -> String {
    env::var("CLOUD_ML_REGION").unwrap_or_else(|_| "us-east5".to_string())
}

/// bash 命令是否在每次执行后重置到项目工作目录。
pub fn should_maintain_project_working_dir() -> bool {
    is_env_truthy(
        env::var("CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR")
            .ok()
            .as_deref(),
    )
}

/// 是否运行在 Homespace（ant 内部云环境）。
pub fn is_running_on_homespace() -> bool {
    env::var("USER_TYPE").as_deref() == Ok("ant")
        && is_env_truthy(env::var("COO_RUNNING_ON_HOMESPACE").ok().as_deref())
}

/// 是否运行在受保护（特权或 ASL3+）的 COO 命名空间/集群内。
///
/// 保守判定：信号含糊时倾向于报告「受保护」。未受保护环境为 homespace、
/// 开放 allowlist 命名空间与无 k8s/COO 信号的环境（笔记本/本地开发）。
pub fn is_in_protected_namespace() -> bool {
    // TS: 仅 USER_TYPE === 'ant' 时 require('./protectedNamespace.js')
    //     .checkProtectedNamespace()；外部构建中该分支被 DCE 消除。
    // 依赖未转换 → 本端口按外部构建语义返回 false（见模块头已知差异）。
    false
}

// @[MODEL LAUNCH]: 新模型的 Vertex 区域覆盖环境变量加在这里。
/// 模型前缀 → Vertex 区域覆盖环境变量。
/// 顺序敏感：更具体的前缀必须排在更宽泛的前缀之前
/// （如 'claude-opus-4-1' 必须在 'claude-opus-4' 之前）。
const VERTEX_REGION_OVERRIDES: &[(&str, &str)] = &[
    ("claude-haiku-4-5", "VERTEX_REGION_CLAUDE_HAIKU_4_5"),
    ("claude-3-5-haiku", "VERTEX_REGION_CLAUDE_3_5_HAIKU"),
    ("claude-3-5-sonnet", "VERTEX_REGION_CLAUDE_3_5_SONNET"),
    ("claude-3-7-sonnet", "VERTEX_REGION_CLAUDE_3_7_SONNET"),
    ("claude-opus-4-1", "VERTEX_REGION_CLAUDE_4_1_OPUS"),
    ("claude-opus-4", "VERTEX_REGION_CLAUDE_4_0_OPUS"),
    ("claude-sonnet-4-6", "VERTEX_REGION_CLAUDE_4_6_SONNET"),
    ("claude-sonnet-4-5", "VERTEX_REGION_CLAUDE_4_5_SONNET"),
    ("claude-sonnet-4", "VERTEX_REGION_CLAUDE_4_0_SONNET"),
];

/// 获取指定模型的 Vertex AI 区域。不同模型可能在不同区域可用；
/// 未命中前缀或环境变量未设置时回退到默认区域。
pub fn get_vertex_region_for_model(model: Option<&str>) -> String {
    if let Some(model) = model {
        if let Some((_, env_var)) = VERTEX_REGION_OVERRIDES
            .iter()
            .find(|(prefix, _)| model.starts_with(prefix))
        {
            return env::var(env_var).unwrap_or_else(|_| get_default_vertex_region());
        }
    }
    get_default_vertex_region()
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::{Mutex, MutexGuard};

    // 同模块测试共享一把锁：Rust 测试默认并行，环境变量读写需要串行化。
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn truthy_and_falsy_detection() {
        assert!(is_env_truthy(Some("1")));
        assert!(is_env_truthy(Some("TRUE")));
        assert!(is_env_truthy(Some(" yes ")));
        assert!(is_env_truthy(Some("on")));
        assert!(!is_env_truthy(Some("0")));
        assert!(!is_env_truthy(Some("")));
        assert!(!is_env_truthy(None));
        assert!(is_env_defined_falsy(Some("0")));
        assert!(is_env_defined_falsy(Some(" Off ")));
        assert!(!is_env_defined_falsy(Some("1")));
        assert!(!is_env_defined_falsy(None));
    }

    #[test]
    fn parse_env_vars_splits_on_first_equals() {
        let _lock = test_lock();
        let args = vec!["KEY1=value1".to_string(), "KEY2=a=b".to_string()];
        let parsed = parse_env_vars(Some(&args)).expect("valid");
        assert_eq!(parsed.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(parsed.get("KEY2"), Some(&"a=b".to_string()));
    }

    #[test]
    fn parse_env_vars_rejects_missing_value() {
        let _lock = test_lock();
        let args = vec!["BADKEY".to_string()];
        let err = parse_env_vars(Some(&args)).expect_err("invalid");
        assert!(
            err.to_string()
                .starts_with("Invalid environment variable format: BADKEY")
        );
    }

    #[test]
    fn vertex_region_prefix_specificity() {
        let _lock = test_lock();
        // 环境变量未设置时回退到默认区域。
        assert_eq!(
            get_vertex_region_for_model(Some("claude-opus-4-1")),
            "us-east5"
        );
        assert_eq!(get_vertex_region_for_model(None), "us-east5");
        std::env::set_var("VERTEX_REGION_CLAUDE_4_1_OPUS", "europe-west1");
        assert_eq!(
            get_vertex_region_for_model(Some("claude-opus-4-1-x")),
            "europe-west1"
        );
        std::env::remove_var("VERTEX_REGION_CLAUDE_4_1_OPUS");
    }
}
