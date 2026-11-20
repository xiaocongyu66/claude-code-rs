//! files.rs — 二进制文件扩展名判定与二进制内容检测。
//!
//! 重写自 `src/constants/files.ts`。`BINARY_EXTENSIONS` 在 TS 中是 Set，
//! 此处用静态切片 + `contains` 辅助函数等价实现。

/// Binary file extensions to skip for text-based operations.
/// These files can't be meaningfully compared as text and are often large.
pub const BINARY_EXTENSIONS: &[&str] = &[
    // Images
    ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".webp", ".tiff", ".tif",
    // Videos
    ".mp4", ".mov", ".avi", ".mkv", ".webm", ".wmv", ".flv", ".m4v", ".mpeg", ".mpg",
    // Audio
    ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a", ".wma", ".aiff", ".opus",
    // Archives
    ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar", ".xz", ".z", ".tgz", ".iso",
    // Executables/binaries
    ".exe", ".dll", ".so", ".dylib", ".bin", ".o", ".a", ".obj", ".lib", ".app", ".msi", ".deb",
    ".rpm",
    // Documents (PDF is here; FileReadTool excludes it at the call site)
    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods", ".odp",
    // Fonts
    ".ttf", ".otf", ".woff", ".woff2", ".eot",
    // Bytecode / VM artifacts
    ".pyc", ".pyo", ".class", ".jar", ".war", ".ear", ".node", ".wasm", ".rlib",
    // Database files
    ".sqlite", ".sqlite3", ".db", ".mdb", ".idx",
    // Design / 3D
    ".psd", ".ai", ".eps", ".sketch", ".fig", ".xd", ".blend", ".3ds", ".max",
    // Flash
    ".swf", ".fla",
    // Lock/profiling data
    ".lockb", ".dat", ".data",
];

/// Check if a file path has a binary extension.
///
/// 与 TS 实现对齐：取 `lastIndexOf('.')` 之后到结尾的子串（含 '.'）。
/// 注意 TS 中无 '.' 时 `slice(-1)` 返回最后一个字符，此处保持相同行为。
pub fn has_binary_extension(file_path: &str) -> bool {
    let ext: String = match file_path.rfind('.') {
        Some(idx) => file_path[idx..].to_lowercase(),
        // TS: filePath.slice(-1) —— 无点时取最后一个字符（空串得空串）
        None => file_path.chars().last().map(|c| c.to_lowercase().collect()).unwrap_or_default(),
    };
    BINARY_EXTENSIONS.contains(&ext.as_str())
}

/// Number of bytes to read for binary content detection.
const BINARY_CHECK_SIZE: usize = 8192;

/// Check if a buffer contains binary content by looking for null bytes
/// or a high proportion of non-printable characters.
pub fn is_binary_content(buffer: &[u8]) -> bool {
    // Check first BINARY_CHECK_SIZE bytes (or full buffer if smaller)
    let check_size = buffer.len().min(BINARY_CHECK_SIZE);

    let mut non_printable = 0usize;
    for &byte in buffer.iter().take(check_size) {
        // Null byte is a strong indicator of binary
        if byte == 0 {
            return true;
        }
        // Count non-printable, non-whitespace bytes
        // Printable ASCII is 32-126, plus common whitespace (9, 10, 13)
        if byte < 32 && byte != 9 && byte != 10 && byte != 13 {
            // tab / newline / carriage return
            non_printable += 1;
        }
    }

    // If more than 10% non-printable, likely binary.
    // 空缓冲区时 TS 中 0/0 = NaN，NaN > 0.1 为 false；此处显式判空对齐。
    check_size > 0 && non_printable * 10 > check_size
}
