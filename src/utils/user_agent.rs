//! user_agent.rs — User-Agent 字符串工具。
//!
//! 重写自 `src/utils/userAgent.ts`。刻意保持零依赖，SDK 打包代码
//! （bridge、cli/transports）可以无负担导入。
//!
//! TS 通过构建期 `MACRO.VERSION`（由 package.json 注入）取版本号；
//! Rust 侧用编译期 `CARGO_PKG_VERSION`（由 Cargo.toml 注入）等价替代。

pub fn get_claude_code_user_agent() -> String {
    format!("claude-code/{}", env!("CARGO_PKG_VERSION"))
}
