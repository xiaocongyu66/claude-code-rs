//! subprocess_env.rs — 子进程环境变量安全清洗。
//!
//! 重写自 `src/utils/subprocessEnv.ts`。在 GitHub Actions 内运行时，
//! 从子进程环境中剥离敏感密钥，防止 Bash 工具命令通过 shell 展开
//!（如 ${ANTHROPIC_API_KEY}）外泄 secret。
//!
//! 父 claude 进程保留这些变量（API 调用与延迟凭据读取需要）；
//! 只有子进程（bash、shell snapshot、MCP stdio、LSP、hooks）被清洗。
//! GITHUB_TOKEN / GH_TOKEN 故意不清洗 —— 包装脚本（gh.sh）需要它们
//! 调 GitHub API；该 token 是 job 级、随 workflow 结束过期。

use crate::utils::env_utils::is_env_truthy;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex, OnceLock};

/// 需要从子进程环境剥离的环境变量。
const GHA_SUBPROCESS_SCRUB: &[&str] = &[
    // Anthropic 鉴权 —— claude 每次请求重读，子进程不需要
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    // OTLP exporter 头部 —— 文档记录携带 Authorization=Bearer token，
    // OTEL SDK 进程内读取，子进程绝不需要
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
    "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
    // 云厂商凭据 —— 同一模式（SDK 懒读取）
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "AZURE_CLIENT_CERTIFICATE_PATH",
    // GitHub Actions OIDC —— 被 action JS 在 spawn claude 前消费；
    // 泄漏可铸造 App installation token → 仓库接管
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_URL",
    // GitHub Actions artifact/cache API —— 缓存投毒 → 供应链跳板
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_RUNTIME_URL",
    // claude-code-action 专属副本 —— action JS 在 prepare 阶段
    //（spawn claude 之前）消费。ALL_INPUTS 以 JSON 形式含 anthropic_api_key
    "ALL_INPUTS",
    "OVERRIDE_GITHUB_TOKEN",
    "DEFAULT_WORKFLOW_TOKEN",
    "SSH_SIGNING_KEY",
];

/// CCR 上游代理注入函数的类型：返回要合并进子进程环境的代理变量。
pub type UpstreamProxyEnvFn = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;

static UPSTREAM_PROXY_ENV_FN: OnceLock<Mutex<Option<UpstreamProxyEnvFn>>> = OnceLock::new();

fn proxy_fn_slot() -> &'static Mutex<Option<UpstreamProxyEnvFn>> {
    UPSTREAM_PROXY_ENV_FN.get_or_init(|| Mutex::new(None))
}

/// 注册上游代理 env 注入函数（init.ts 在懒加载 upstreamproxy 模块后调用；
/// 必须在任何子进程 spawn 之前完成）。
pub fn register_upstream_proxy_env_fn<F>(f: F)
where
    F: Fn() -> HashMap<String, String> + Send + Sync + 'static,
{
    let mut slot = proxy_fn_slot().lock().unwrap_or_else(|p| p.into_inner());
    *slot = Some(Arc::new(f));
}

/// 返回清洗后的子进程环境。
///
/// CCR upstreamproxy：注入 HTTPS_PROXY + CA bundle 变量，使 agent 子进程
/// 里的 curl/gh/python 走本地 relay。未注册或代理关闭时返回空表，
/// 非 CCR 容器内为 no-op。
pub fn subprocess_env() -> HashMap<String, String> {
    let proxy_env: HashMap<String, String> = {
        let slot = proxy_fn_slot().lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() {
            Some(f) => f(),
            None => HashMap::new(),
        }
    };

    let scrub = is_env_truthy(env::var("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB").ok().as_deref());

    let mut env_map: HashMap<String, String> = env::vars().collect();
    for (k, v) in proxy_env {
        env_map.insert(k, v);
    }

    if !scrub {
        return env_map;
    }

    for k in GHA_SUBPROCESS_SCRUB {
        env_map.remove(*k);
        // GitHub Actions 为 `with:` 输入自动创建 INPUT_<NAME>，
        // 复制 INPUT_ANTHROPIC_API_KEY 这类 secret；非输入变量为 no-op
        env_map.remove(&format!("INPUT_{k}"));
    }
    env_map
}
