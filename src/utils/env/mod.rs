//! env.rs — 环境变量读出面目录的统一 re-export（barrel）。
//!
//! 重写自 `src/utils/env/index.ts`。散落/实验性环境变量读取的白名单之家，
//! 使用方从这里或具体子模块导入，而不是各处散落原始 `std::env` 读取。

pub mod auto_mode_env;
pub mod env_reads;
pub mod forwarding_gates;
pub mod gray_gates;

pub use auto_mode_env::{AUTO_MODE_ENV_VAR, is_auto_mode_enabled};
pub use env_reads::{
    get_stage_file_root, is_agent_view_relaunch, is_artifact_auto_open, is_eval_confined,
    is_spawned_by_server, should_propagate_nested_memory,
};
pub use forwarding_gates::{
    is_hook_forwarding_disabled, is_hook_forwarding_enabled, is_plugin_forwarding_disabled,
    is_plugin_forwarding_enabled,
};
pub use gray_gates::{
    GRAY_ENV_GATE_NAMES, GrayEnvGate, get_enabled_gray_gates, gray_gate_env_var,
    is_gray_gate_enabled,
};
