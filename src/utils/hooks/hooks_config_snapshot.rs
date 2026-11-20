//! hooks_config_snapshot.rs — hooks 配置快照（重写自 `hooksConfigSnapshot.ts`）。
//!
//! 启动时（或 hooks 经设置变更后）捕获当前 hooks 配置的快照，供
//! `/hooks` 及文件监视等场景读取，尊重 allowManagedHooksOnly 策略。
//!
//! 语义对齐（TS `getHooksFromAllowedSources` 的注释要点）：
//! - policySettings（托管）置 `disableAllHooks` → 返回空。
//! - policySettings 置 `allowManagedHooksOnly` → 只返回托管 hooks。
//! - `strictPluginOnlyCustomization` 含 "hooks" → 用户/项目/本地 hooks 被
//!   拦截（插件 hooks 由 hooks.ts 组装侧单独装配，不在此处一刀切）。
//! - 非托管 settings 置 `disableAllHooks` → 仅托管 hooks 仍运行（非托管
//!   设置无法关闭托管 hooks），即 `should_allow_managed_hooks_only` 视
//!   为 true。
//!
//! 已知差异：
//! - TS 的 `getSettings_DEPRECATED` 是 `getInitialSettings` 的 alias
//!   （settings.ts:830）；Rust 端 `crate::utils::settings::settings`
//!   只导出 `get_initial_settings`，直接使用。
//! - 快照类型 `HooksSettings`（HashMap<String, Vec<HookMatcher>>）随
//!   settings/types.rs 落地，与 TS 同构。

use std::sync::Mutex;

use crate::bootstrap::state::{reset_sdk_init_state, SettingSource};
use crate::utils::settings::plugin_only_policy::is_restricted_to_plugin_only;
use crate::utils::settings::settings::{get_initial_settings, get_settings_for_source};
use crate::utils::settings::settings_cache::reset_settings_cache;
use crate::utils::settings::types::HooksSettings;

static INITIAL_HOOKS_CONFIG: Mutex<Option<HooksSettings>> = Mutex::new(None);

/// 毒锁自愈：poisoned 时取回内部数据而不是 panic。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn empty_hooks() -> HooksSettings {
    HooksSettings::new()
}

/// 从允许的来源读取 hooks 配置。
fn get_hooks_from_allowed_sources() -> HooksSettings {
    let policy_settings = get_settings_for_source(SettingSource::PolicySettings);

    // 托管 settings 关闭全部 hooks → 返回空
    if policy_settings
        .as_ref()
        .and_then(|p| p.disable_all_hooks)
        .unwrap_or(false)
    {
        return empty_hooks();
    }

    // 托管 settings 置 allowManagedHooksOnly → 只用托管 hooks
    if policy_settings
        .as_ref()
        .and_then(|p| p.allow_managed_hooks_only)
        .unwrap_or(false)
    {
        return policy_settings
            .as_ref()
            .and_then(|p| p.hooks.clone())
            .unwrap_or_else(empty_hooks);
    }

    // strictPluginOnlyCustomization: 拦截用户/项目/本地 settings 的 hooks。
    // 插件 hooks（hooks.ts:1391 注册通道）不受影响——它们单独装配，
    // managedOnly 跳过键控于 should_allow_managed_hooks_only() 而非该策略。
    // agent frontmatter hooks 在注册时（runAgent.ts:~535）按 agent 来源
    // 门控：plugin/builtin/policySettings agent 正常注册，用户来源 agent
    // 在 ["hooks"] 下跳过注册。此处一刀切的执行期拦截会误杀插件 agent
    // 的 hooks。
    if is_restricted_to_plugin_only("hooks") {
        return policy_settings
            .as_ref()
            .and_then(|p| p.hooks.clone())
            .unwrap_or_else(empty_hooks);
    }

    let merged_settings = get_initial_settings();

    // 非托管 settings 置 disableAllHooks → 仅托管 hooks 仍运行
    // （非托管设置无法覆盖托管 hooks）
    if merged_settings.disable_all_hooks.unwrap_or(false) {
        return policy_settings
            .as_ref()
            .and_then(|p| p.hooks.clone())
            .unwrap_or_else(empty_hooks);
    }

    // 否则使用全部来源合并的 hooks（向后兼容）
    merged_settings.hooks.unwrap_or_else(empty_hooks)
}

/// 是否只应运行托管 hooks。成立条件：
/// - policySettings 有 allowManagedHooksOnly: true，或
/// - 非托管 settings 置 disableAllHooks（非托管设置无法关闭托管 hooks，
///   故实质退化为 managed-only）。
pub fn should_allow_managed_hooks_only() -> bool {
    let policy_settings = get_settings_for_source(SettingSource::PolicySettings);
    if policy_settings
        .as_ref()
        .and_then(|p| p.allow_managed_hooks_only)
        .unwrap_or(false)
    {
        return true;
    }
    // disableAllHooks 非来自托管 settings → 视为 managed-only
    if get_initial_settings().disable_all_hooks.unwrap_or(false)
        && policy_settings
            .as_ref()
            .and_then(|p| p.disable_all_hooks)
            .unwrap_or(false)
            != true
    {
        return true;
    }
    false
}

/// 是否应禁用全部 hooks（含托管）。仅当托管/policy settings 置
/// disableAllHooks: true 成立；非托管 settings 置位时托管 hooks 仍运行。
pub fn should_disable_all_hooks_including_managed() -> bool {
    get_settings_for_source(SettingSource::PolicySettings)
        .as_ref()
        .and_then(|p| p.disable_all_hooks)
        .unwrap_or(false)
}

/// 捕获当前 hooks 配置快照。应用启动时调用一次。尊重
/// allowManagedHooksOnly 设置。
pub fn capture_hooks_config_snapshot() {
    *lock(&INITIAL_HOOKS_CONFIG) = Some(get_hooks_from_allowed_sources());
}

/// 更新 hooks 配置快照。通过设置变更 hooks 时调用。
/// 先重置会话缓存，确保从磁盘读到新 settings——没有这一步，用户外部
/// 编辑 settings.json 后再运行 /hooks 时，文件监视的 stability 阈值可能
/// 尚未到期，快照会用到陈旧缓存。
pub fn update_hooks_config_snapshot() {
    reset_settings_cache();
    *lock(&INITIAL_HOOKS_CONFIG) = Some(get_hooks_from_allowed_sources());
}

/// 从快照读取当前 hooks 配置；快照不存在时回退即捕获。
pub fn get_hooks_config_from_snapshot() -> Option<HooksSettings> {
    let needs_capture = lock(&INITIAL_HOOKS_CONFIG).is_none();
    if needs_capture {
        capture_hooks_config_snapshot();
    }
    lock(&INITIAL_HOOKS_CONFIG).clone()
}

/// 重置 hooks 配置快照（测试用）。同时重置 SDK init state 防测试污染。
pub fn reset_hooks_config_snapshot() {
    *lock(&INITIAL_HOOKS_CONFIG) = None;
    reset_sdk_init_state();
}
