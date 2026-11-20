//! gray_gates.rs — 实验性 gray-gate 组（W7 task #58 block 7）。
//!
//! 重写自 `src/utils/env/grayGates.ts`。
//!
//! 官方锚点：0043-chunk-zrkp23rq/module-003-env-vars.js 把两个词的
//! 代号环境变量开关并列枚举（BASALT_COVE..BREEZY_HORIZON）；
//! THRIFTY_SONIC 另见于 0082-chunk-dxsxk16b/config/env-vars.js。
//! 官方构建中这些开关默认关闭——灰度路由通过 CLAUDE_CODE_<NAME>=1
//! 在分阶段 rollout 期间翻转。
//!
//! 代码库 feature-gate/defines 面（scripts/defines.ts）已登记该组；
//! 运行时读出面在此，使门禁保持 env 驱动（无需重新构建），而不是
//! 烘进 DEFAULT_BUILD_FEATURES。

use crate::utils::env_utils::is_env_truthy;
use std::env;

/// 十个官方 gray gate，按 Chunk-0043 枚举顺序。
/// 同列表中的 `Simple`/`SimpleSystemPrompt` 被排除：它们是普通布尔开关
/// （--bare / system-prompt 替换），不是分阶段 rollout。
pub const GRAY_ENV_GATE_NAMES: &[&str] = &[
    "BASALT_COVE",
    "THRIFTY_SONIC",
    "COZY_TEAPOT",
    "GORSE_PLOVER",
    "AMBER_ASTROLABE",
    "BISON_CAIRN",
    "LARCH_CISTERN",
    "WILLOW_TERN",
    "PARCHMENT_FERN",
    "BREEZY_HORIZON",
];

/// Gray gate 枚举（数量与顺序与 GRAY_ENV_GATE_NAMES 一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GrayEnvGate {
    BasaltCove,
    ThriftySonic,
    CozyTeapot,
    GorsePlover,
    AmberAstrolabe,
    BisonCairn,
    LarchCistern,
    WillowTern,
    ParchmentFern,
    BreezyHorizon,
}

impl GrayEnvGate {
    /// 全部 gate，按枚举顺序。
    pub const ALL: &[GrayEnvGate] = &[
        GrayEnvGate::BasaltCove,
        GrayEnvGate::ThriftySonic,
        GrayEnvGate::CozyTeapot,
        GrayEnvGate::GorsePlover,
        GrayEnvGate::AmberAstrolabe,
        GrayEnvGate::BisonCairn,
        GrayEnvGate::LarchCistern,
        GrayEnvGate::WillowTern,
        GrayEnvGate::ParchmentFern,
        GrayEnvGate::BreezyHorizon,
    ];

    /// 官方环境变量名中的代号（如 `BASALT_COVE`）。
    pub fn name(self) -> &'static str {
        match self {
            GrayEnvGate::BasaltCove => "BASALT_COVE",
            GrayEnvGate::ThriftySonic => "THRIFTY_SONIC",
            GrayEnvGate::CozyTeapot => "COZY_TEAPOT",
            GrayEnvGate::GorsePlover => "GORSE_PLOVER",
            GrayEnvGate::AmberAstrolabe => "AMBER_ASTROLABE",
            GrayEnvGate::BisonCairn => "BISON_CAIRN",
            GrayEnvGate::LarchCistern => "LARCH_CISTERN",
            GrayEnvGate::WillowTern => "WILLOW_TERN",
            GrayEnvGate::ParchmentFern => "PARCHMENT_FERN",
            GrayEnvGate::BreezyHorizon => "BREEZY_HORIZON",
        }
    }

    /// 由代号字符串构造；未知代号返回 None。
    pub fn from_name(name: &str) -> Option<GrayEnvGate> {
        Self::ALL.iter().copied().find(|g| g.name() == name)
    }
}

/// gray gate 背后的环境变量：CLAUDE_CODE_BASALT_COVE 之类。
pub fn gray_gate_env_var(gate: GrayEnvGate) -> String {
    format!("CLAUDE_CODE_{}", gate.name())
}

/// gray gate 默认关闭。仅显式真值（1/true/yes/on）启用路由；
/// 未设置或显式假值保持禁用。
pub fn is_gray_gate_enabled(gate: GrayEnvGate) -> bool {
    is_env_truthy(env::var(gray_gate_env_var(gate)).ok().as_deref())
}

/// 当前已启用的全部 gray gate 快照（按枚举顺序）。
pub fn get_enabled_gray_gates() -> Vec<GrayEnvGate> {
    GrayEnvGate::ALL
        .iter()
        .copied()
        .filter(|gate| is_gray_gate_enabled(*gate))
        .collect()
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
    fn env_var_naming() {
        assert_eq!(
            gray_gate_env_var(GrayEnvGate::BreezyHorizon),
            "CLAUDE_CODE_BREEZY_HORIZON"
        );
    }

    #[test]
    fn default_off_and_selective_enablement() {
        let _lock = test_lock();
        for gate in GrayEnvGate::ALL {
            std::env::remove_var(gray_gate_env_var(*gate));
        }
        assert!(get_enabled_gray_gates().is_empty());

        std::env::set_var("CLAUDE_CODE_THRIFTY_SONIC", "1");
        std::env::set_var("CLAUDE_CODE_COZY_TEAPOT", "yes");
        std::env::set_var("CLAUDE_CODE_BASALT_COVE", "0");
        let enabled = get_enabled_gray_gates();
        assert_eq!(
            enabled,
            vec![GrayEnvGate::ThriftySonic, GrayEnvGate::CozyTeapot]
        );

        std::env::remove_var("CLAUDE_CODE_THRIFTY_SONIC");
        std::env::remove_var("CLAUDE_CODE_COZY_TEAPOT");
        std::env::remove_var("CLAUDE_CODE_BASALT_COVE");
    }

    #[test]
    fn name_roundtrip() {
        for gate in GrayEnvGate::ALL {
            assert_eq!(GrayEnvGate::from_name(gate.name()), Some(*gate));
        }
        assert_eq!(GrayEnvGate::from_name("BASALT"), None);
    }
}
