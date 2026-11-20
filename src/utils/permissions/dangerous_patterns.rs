//! dangerous_patterns.rs — 危险 shell allow 规则前缀清单。
//!
//! 重写自 `src/utils/permissions/dangerousPatterns.ts`。
//!
//! `Bash(python:*)` / `PowerShell(node:*)` 这类 allow 规则让模型可以经
//! 该解释器执行任意代码，绕过 auto 模式分类器。本清单供
//! permissionSetup.ts 中的 `isDangerous{Bash,PowerShell}Permission`
//! 断言使用，在进入 auto 模式时剥离此类规则。
//!
//! 语义对齐：TS 在模块加载时按 `process.env.USER_TYPE === 'ant'` 展开
//! ant-only 条件展开项；Rust 侧用 `once_cell::sync::Lazy` 在首次访问时
//! 求值（等价于 TS 的模块加载时求值）。

use once_cell::sync::Lazy;

/// 跨平台代码执行入口：Unix 与 Windows 上都存在的项。
/// 两个平台单独维护的清单共享此常量，防止新增解释器时两边漂移。
pub const CROSS_PLATFORM_CODE_EXEC: &[&str] = &[
    // Interpreters
    "python",
    "python3",
    "python2",
    "node",
    "deno",
    "tsx",
    "ruby",
    "perl",
    "php",
    "lua",
    // Package runners
    "npx",
    "bunx",
    "npm run",
    "yarn run",
    "pnpm run",
    "bun run",
    // Shells reachable from both (Git Bash / WSL on Windows, native on Unix)
    "bash",
    "sh",
    // Remote arbitrary-command wrapper (native OpenSSH on Win10+)
    "ssh",
];

static DANGEROUS_BASH_PATTERNS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut patterns: Vec<&'static str> = CROSS_PLATFORM_CODE_EXEC.to_vec();
    patterns.extend_from_slice(&[
        "zsh",
        "fish",
        "eval",
        "exec",
        "env",
        "xargs",
        "sudo",
    ]);

    // Anthropic internal: ant-only tools plus general tools that ant sandbox
    // dotfile data shows are commonly over-allowlisted as broad prefixes.
    // These stay ant-only — external users don't have coo, and the rest are
    // an empirical-risk call grounded in ant sandbox data, not a universal
    // "this tool is unsafe" judgment. PS may want these once it has usage data.
    if std::env::var("USER_TYPE").as_deref() == Ok("ant") {
        patterns.extend_from_slice(&[
            "fa run",
            // Cluster code launcher — arbitrary code on the cluster
            "coo",
            // Network/exfil: gh gist create --public, gh api arbitrary HTTP,
            // curl/wget POST. gh api needs its own entry — the matcher is
            // exact-shape, not prefix, so pattern 'gh' alone does not catch
            // rule 'gh api:*' (same reason 'npm run' is separate from 'npm').
            "gh",
            "gh api",
            "curl",
            "wget",
            // git config core.sshCommand / hooks install = arbitrary code
            "git",
            // Cloud resource writes (s3 public buckets, k8s mutations)
            "kubectl",
            "aws",
            "gcloud",
            "gsutil",
        ]);
    }

    patterns
});

/// `DANGEROUS_BASH_PATTERNS`：危险 Bash allow 前缀清单（只读视图）。
pub fn dangerous_bash_patterns() -> &'static [&'static str] {
    &DANGEROUS_BASH_PATTERNS
}
