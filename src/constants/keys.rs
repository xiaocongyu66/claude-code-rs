//! keys.rs — GrowthBook 客户端密钥选择。
//!
//! 重写自 `src/constants/keys.ts`。仅依赖环境变量与 `env_utils`，无循环导入。
//! 环境读取集中在薄封装 `get_growthbook_client_key`，密钥选择逻辑放在
//! 纯函数 `fallback_key` 中以便测试（避免测试间共享环境变量的竞态）。

use crate::utils::env_utils::is_env_truthy;

/// GrowthBook 客户端密钥的纯逻辑（不含适配器分支）。
///
/// ant 用户按 `ENABLE_GROWTHBOOK_DEV` 选 dev/prod 密钥；其他用户用外部密钥。
fn fallback_key(is_ant: bool, dev_enabled: bool) -> &'static str {
    if is_ant {
        if dev_enabled {
            "sdk-yZQvlplybuXjYh6L"
        } else {
            "sdk-xRVcrliHIlrg4og4"
        }
    } else {
        "sdk-zAZezfDKGoZuXXKe"
    }
}

/// GrowthBook 客户端密钥。
///
/// 懒读取，以便 `globalSettings.env`（模块加载后应用）中的
/// `ENABLE_GROWTHBOOK_DEV` 生效。`USER_TYPE` 是构建时 define，所以安全。
pub fn get_growthbook_client_key() -> String {
    let is_ant = std::env::var("USER_TYPE")
        .map(|user_type| user_type == "ant")
        .unwrap_or(false);
    let dev_enabled = is_env_truthy(std::env::var("ENABLE_GROWTHBOOK_DEV").ok().as_deref());
    // 适配器优先：自定义 GrowthBook 服务器（TS 真值检查，空字符串为 falsy）
    match std::env::var("CLAUDE_GB_ADAPTER_KEY") {
        Ok(adapter_key) if !adapter_key.is_empty() => adapter_key,
        _ => fallback_key(is_ant, dev_enabled).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_user_gets_external_key() {
        assert_eq!(fallback_key(false, false), "sdk-zAZezfDKGoZuXXKe");
        assert_eq!(fallback_key(false, true), "sdk-zAZezfDKGoZuXXKe");
    }

    #[test]
    fn ant_user_prod_key() {
        assert_eq!(fallback_key(true, false), "sdk-xRVcrliHIlrg4og4");
    }

    #[test]
    fn ant_user_dev_key_when_enabled() {
        assert_eq!(fallback_key(true, true), "sdk-yZQvlplybuXjYh6L");
    }
}
