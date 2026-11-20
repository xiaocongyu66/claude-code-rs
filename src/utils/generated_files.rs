//! generated_files.rs — 生成文件判定（Linguist 风格规则）。
//!
//! 重写自 `src/utils/generatedFiles.ts`。语义对齐：
//!
//! - 应从 attribution 中排除的文件：精确文件名、扩展名（含复合扩展名
//!   如 `.min.js`）、目录片段、以及更复杂的文件名正则模式。
//! - 路径分隔符统一为 posix `/` 后做目录片段匹配；文件名/扩展名
//!   大小写不敏感。

use regex::Regex;

use once_cell::sync::Lazy;

/// 精确文件名匹配（大小写不敏感）。
const EXCLUDED_FILENAMES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "composer.lock",
    "gemfile.lock",
    "cargo.lock",
    "poetry.lock",
    "pipfile.lock",
    "shrinkwrap.json",
    "npm-shrinkwrap.json",
];

/// 扩展名模式（大小写不敏感）。
const EXCLUDED_EXTENSIONS: &[&str] = &[
    ".lock",
    ".min.js",
    ".min.css",
    ".min.html",
    ".bundle.js",
    ".bundle.css",
    ".generated.ts",
    ".generated.js",
    ".d.ts",
];

/// 指示生成/ vendored 内容的目录片段（posix 风格）。
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "/dist/",
    "/build/",
    "/out/",
    "/output/",
    "/node_modules/",
    "/vendor/",
    "/vendored/",
    "/third_party/",
    "/third-party/",
    "/external/",
    "/.next/",
    "/.nuxt/",
    "/.svelte-kit/",
    "/coverage/",
    "/__pycache__/",
    "/.tox/",
    "/venv/",
    "/.venv/",
    "/target/release/",
    "/target/debug/",
];

static EXCLUDED_FILENAME_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^.*\.min\.[a-z]+$",           // *.min.*
        r"^.*-min\.[a-z]+$",            // *-min.*
        r"^.*\.bundle\.[a-z]+$",        // *.bundle.*
        r"^.*\.generated\.[a-z]+$",     // *.generated.*
        r"^.*\.gen\.[a-z]+$",           // *.gen.*
        r"^.*\.auto\.[a-z]+$",          // *.auto.*
        r"^.*_generated\.[a-z]+$",      // *_generated.*
        r"^.*_gen\.[a-z]+$",            // *_gen.*
        r"^.*\.pb\.(go|js|ts|py|rb)$",  // protobuf 生成文件
        r"^.*_pb2?\.py$",               // Python protobuf 文件
        r"^.*\.pb\.h$",                 // C++ protobuf 头文件
        r"^.*\.grpc\.[a-z]+$",          // gRPC 生成文件
        r"^.*\.swagger\.[a-z]+$",       // Swagger 生成文件
        r"^.*\.openapi\.[a-z]+$",       // OpenAPI 生成文件
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid generated-file pattern"))
    .collect()
});

fn basename(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.rsplit('/').next() {
        Some(name) => name.to_string(),
        None => normalized,
    }
}

fn extname(path: &str) -> String {
    let base = basename(path);
    match base.rfind('.') {
        Some(idx) if idx > 0 => base[idx..].to_lowercase(),
        _ => String::new(),
    }
}

/// 判断文件是否应按 Linguist 风格规则从 attribution 中排除。
///
/// `file_path` 为相对仓库根的路径；分隔符混合（/ 与 \）也能处理。
pub fn is_generated_file(file_path: &str) -> bool {
    // 统一分隔符为 posix 风格（模式用 posix `/`）
    let normalized_path = format!("/{}", file_path.replace('\\', "/").trim_start_matches('/'));
    let file_name = basename(file_path).to_lowercase();
    let ext = extname(file_path);

    // 精确文件名匹配
    if EXCLUDED_FILENAMES.contains(&file_name.as_str()) {
        return true;
    }

    // 扩展名匹配
    if EXCLUDED_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }

    // 复合扩展名，如 .min.js
    let parts: Vec<&str> = file_name.split('.').collect();
    if parts.len() > 2 {
        let compound_ext = format!(".{}", parts[parts.len() - 2..].join("."));
        if EXCLUDED_EXTENSIONS.contains(&compound_ext.as_str()) {
            return true;
        }
    }

    // 目录片段匹配
    if EXCLUDED_DIRECTORIES.iter().any(|dir| normalized_path.contains(dir)) {
        return true;
    }

    // 文件名正则模式
    if EXCLUDED_FILENAME_PATTERNS.iter().any(|re| re.is_match(&file_name)) {
        return true;
    }

    false
}

/// 过滤掉生成文件，返回非生成文件列表。
pub fn filter_generated_files(files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|file| !is_generated_file(file))
        .cloned()
        .collect()
}
