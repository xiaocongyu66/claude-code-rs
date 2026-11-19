//! cli.rs — CLI bootstrap：加载完整 CLI 之前的快速路径分发。
//!
//! 重写自 `src/entrypoints/cli.tsx`。行为对齐：`--version` 零模块加载；
//! 其余快速路径随目录重写推进逐个接入。

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --version/-v/-V：零模块加载
    if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-v" | "-V") {
        println!("{VERSION} (Claude Code Hardened)");
        return ExitCode::SUCCESS;
    }

    // 其余快速路径尚未重写完成；加载完整 CLI 的 Rust 实现待后续波次接入。
    eprintln!(
        "cch: full CLI not yet ported to Rust; args: [{}]",
        args.join(", ")
    );
    ExitCode::from(2)
}
