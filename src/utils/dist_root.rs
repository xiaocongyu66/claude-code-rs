//! dist_root.rs — 从当前模块位置解析 dist 根目录。
//!
//! 重写自 `src/utils/distRoot.ts`。语义对齐：
//!
//! - TS 用 `import.meta.url` 取模块路径，再按 `path.sep` 切分后查找
//!   最后一个 `dist` / `src` 分段。适配三种布局：
//!   - 单文件：dist/cli.js → dist/
//!   - 代码分割：dist/chunks/chunk-xxx.js → dist/
//!   - 开发模式：src/utils/distRoot.ts → <project_root>/
//! - Rust 侧没有 `import.meta.url` 的运行时等价物；改用编译期 `file!()`
//!   宏定位源码路径（开发模式语义），并尝试 `std::env::current_exe()`
//!   定位产物（安装/部署后语义）。二者都按 TS 的 lastIndexOf('dist') /
//!   lastIndexOf('src') 规则切分。

use std::path::PathBuf;

/// 解析 dist 根目录。
pub fn dist_root() -> PathBuf {
    // 候选路径：编译期源码路径（开发模式）与当前可执行文件路径（产物模式）。
    let file_path = file!();
    let candidates: Vec<String> = {
        let mut v = vec![file_path.to_string()];
        if let Ok(exe) = std::env::current_exe() {
            v.push(exe.to_string_lossy().into_owned());
        }
        v
    };

    for candidate in candidates {
        let normalized = candidate.replace('\\', "/");
        let parts: Vec<&str> = normalized.split('/').collect();
        if let Some(dist_idx) = parts.iter().rposition(|p| *p == "dist") {
            return PathBuf::from(parts[..=dist_idx].join(&std::path::MAIN_SEPARATOR.to_string()));
        }
        // 开发模式：来自 src/ → project root
        if let Some(src_idx) = parts.iter().rposition(|p| *p == "src") {
            return PathBuf::from(parts[..src_idx].join(&std::path::MAIN_SEPARATOR.to_string()));
        }
    }

    // 都不匹配：回退到当前模块所在目录
    PathBuf::from(file_path)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
