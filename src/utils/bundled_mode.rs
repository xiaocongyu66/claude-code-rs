//! bundled_mode.rs — Bun 运行时检测。
//!
//! 重写自 `src/utils/bundledMode.ts`。Rust 端没有 Bun 运行时概念：
//! - `is_running_with_bun()` 恒为 false（构建产物由 Rust 原生执行）。
//! - `is_in_bundled_mode()` 保留 TS 的 argv[1] 兜底判定（`/$bunfs/` 前缀），
//!   使移植后的启动路径分流逻辑保持同一形态；在纯 Rust 进程中恒为 false。

use std::env;

/// 当前运行时是否为 Bun。Rust 端恒为 false。
pub fn is_running_with_bun() -> bool {
    false
}

/// 是否运行在 Bun compile 产出的独立二进制中。
///
/// TS 源的主判定（`Bun.embeddedFiles` 非空）在 Rust 端不存在；保留
/// argv[1] 兜底逻辑：bunfs 虚拟路径前缀是 compile 自执行的铁证。
pub fn is_in_bundled_mode() -> bool {
    match env::args_os().nth(1) {
        Some(a1) => {
            let a1 = a1.to_string_lossy();
            a1.starts_with("/$bunfs/") || a1.starts_with("/$BUNFS/")
        }
        None => false,
    }
}
