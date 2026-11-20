//! tempfile.rs — 临时文件路径生成。
//!
//! 重写自 `src/utils/tempfile.ts`。语义对齐：
//!
//! - 默认放在系统临时目录（`os.tmpdir()` → `std::env::temp_dir()`）。
//! - 默认前缀 `claude-prompt`，默认扩展名 `.md`。
//! - `content_hash` 提供时用其 SHA-256 的前 16 个十六进制字符作为标识，
//!   跨进程稳定；否则用随机 UUID v4。

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// 生成临时文件路径。
///
/// - `prefix`：文件名前缀，默认 `claude-prompt`
/// - `extension`：扩展名，默认 `.md`
/// - `content_hash`：提供时标识由其 SHA-256 前 16 位 hex 派生（稳定、
///   可复现，避免污染发给 API 的 prompt cache 前缀）；否则随机 UUID。
pub fn generate_tempFilePath(
    prefix: Option<&str>,
    extension: Option<&str>,
    content_hash: Option<&str>,
) -> PathBuf {
    let id = match content_hash {
        Some(content) => {
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            let digest = hasher.finalize();
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            hex[..16].to_string()
        }
        None => Uuid::new_v4().to_string(),
    };
    let prefix = prefix.unwrap_or("claude-prompt");
    let extension = extension.unwrap_or(".md");
    let mut path = std::env::temp_dir();
    path.push(format!("{prefix}-{id}{extension}"));
    path
}
