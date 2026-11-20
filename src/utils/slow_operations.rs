//! slow_operations.rs — 慢操作计时守卫与 JSON/克隆/文件写包装。
//!
//! 重写自 `src/utils/slowOperations.ts`。语义对齐说明：
//!
//! - TS 的 tagged template `slowLogging\`...\`` 在 Rust 侧落地为
//!   `slow_logging(build)` 工厂：`build` 是惰性闭包，**仅在操作确认为
//!   慢时**才求值（对齐 TS 的 buildDescription 只在慢路径执行）；
//!   `slow_logging_named` 为便捷的静态标签版本。
//! - TS 的 `Symbol.dispose` 守卫落地为 `SlowOperationGuard`：`Drop` 时
//!   计时，超过阈值（对齐 `SLOW_OPERATION_THRESHOLD_MS` 语义：环境变量
//!   覆盖 > NODE_ENV=development 20ms > USER_TYPE=ant 300ms > Infinity）
//!   时调用 `log_for_debugging` 与 `bootstrap::state::add_slow_operation`。
//! - TS 的 `feature('SLOW_OPERATION_LOGGING')` 为内嵌构建开关；
//!   Rust 侧用 `FEATURE_SLOW_OPERATION_LOGGING` 环境变量对齐
//!   （与 `constants::betas` 的 feature 等价实现一致）。未开启时仍返回
//!   真守卫，但 Drop 为零开销 no-op（不计时、不求值描述）。
//! - 调用栈捕获用 `std::backtrace::Backtrace`（RUST_BACKTRACE 未设时
//!   近乎零成本，对齐 TS「构造时捕获、慢路径才格式化」）。
//! - `jsonParse` 用 `Result<Value, Error>` 代替 TS 的抛异常（Rust 惯用法）；
//!   `clone` / `clone_deep` 限定为 `serde_json::Value`（其 `clone` 即深拷贝）。
//! - `writeFileSync_DEPRECATED` 的 options 收敛为 `flush: bool`
//!   （true 时 open→write→fsync→close，对齐 Node 20.1+ 的 flush 选项）。

use std::backtrace::Backtrace;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::Value;

use crate::bootstrap::state::add_slow_operation;
use crate::utils::debug::{log_for_debugging, DebugLogLevel};

/// `feature('SLOW_OPERATION_LOGGING')` 的等价实现：环境变量为真值时启用。
fn slow_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        env::var("FEATURE_SLOW_OPERATION_LOGGING")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE"))
            .unwrap_or(false)
    })
}

/// 慢操作日志阈值（毫秒）。优先级与 TS 一致：
/// `CLAUDE_CODE_SLOW_OPERATION_THRESHOLD_MS`（非负数字）>
/// NODE_ENV=development → 20 > USER_TYPE=ant → 300 > Infinity。
pub fn get_slow_operation_threshold_ms() -> f64 {
    static THRESHOLD: OnceLock<f64> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        if let Ok(raw) = env::var("CLAUDE_CODE_SLOW_OPERATION_THRESHOLD_MS") {
            if let Ok(parsed) = raw.parse::<f64>() {
                if parsed.is_finite() && !parsed.is_nan() && parsed >= 0.0 {
                    return parsed;
                }
            }
        }
        if env::var("NODE_ENV").as_deref() == Ok("development") {
            return 20.0;
        }
        if env::var("USER_TYPE").as_deref() == Ok("ant") {
            return 300.0;
        }
        f64::INFINITY
    })
}

/// 模块级重入保护。log_for_debugging 经 appendFileSync 落盘会再次进入
/// 计时路径；无此保护会 dispose → log → dispose 死循环。
static IS_LOGGING: AtomicBool = AtomicBool::new(false);

/// 从调用栈中提取首个不在本文件内的帧，格式 ` @ file:line`
/// （对齐 TS `callerFrame`）。慢路径才调用。
pub fn caller_frame(stack: &str) -> String {
    for line in stack.lines() {
        // TS 检查 'slowOperations'；Rust 文件名为 slow_operations
        if line.contains("slowOperations") || line.contains("slow_operations") {
            continue;
        }
        if let Some((file, line_no)) = parse_frame_tail(line.trim_end()) {
            return format!(" @ {file}:{line_no}");
        }
    }
    String::new()
}

/// 匹配 TS 正则 `/([^/\\]+?):(\d+):\d+\)?$/` 的行尾帧信息。
fn parse_frame_tail(body: &str) -> Option<(&str, &str)> {
    let body = body.strip_suffix(')').unwrap_or(body);
    let last_colon = body.rfind(':')?;
    let col = &body[last_colon + 1..];
    if col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let before = &body[..last_colon];
    let mid_colon = before.rfind(':')?;
    let line_no = &before[mid_colon + 1..];
    if line_no.is_empty() || !line_no.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let file_part = &before[..mid_colon];
    let start = file_part
        .rfind(|c| c == '/' || c == '\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = &file_part[start..];
    if name.is_empty() {
        return None;
    }
    Some((name, line_no))
}

/// 把模板插值近似为人类可读描述（对齐 TS `buildDescription` 的分支）。
pub fn describe_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => describe_string(s),
        Value::Array(arr) => format!("Array[{}]", arr.len()),
        Value::Object(obj) => format!("Object{{{}}} keys", obj.len()),
    }
}

/// 字符串截断为至多 80 字符（对齐 TS `v.length > 80` 分支，超出加 `…`）。
fn describe_string(s: &str) -> String {
    if s.chars().count() > 80 {
        let truncated: String = s.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

/// 慢操作计时守卫。`Drop` 时计时；超过阈值且未重入时记录 debug 日志
/// 并登记慢操作。未启用（`FEATURE_SLOW_OPERATION_LOGGING` 关闭）时为
/// 零开销 no-op。
pub struct SlowOperationGuard {
    start: Option<Instant>,
    build_description: Option<Box<dyn FnOnce() -> String + Send>>,
    backtrace: Option<Backtrace>,
}

impl Drop for SlowOperationGuard {
    fn drop(&mut self) {
        let Some(start) = self.start else {
            return;
        };
        let Some(build) = self.build_description.take() else {
            return;
        };

        let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
        if !(duration_ms > get_slow_operation_threshold_ms()) {
            return;
        }
        // 重入保护：IS_LOGGING 已真时直接放弃（对齐 TS 的 try/finally）
        if IS_LOGGING.swap(true, Ordering::SeqCst) {
            return;
        }
        struct ResetLogging;
        impl Drop for ResetLogging {
            fn drop(&mut self) {
                IS_LOGGING.store(false, Ordering::SeqCst);
            }
        }
        let _reset = ResetLogging;

        let description = build();
        let frame = self
            .backtrace
            .take()
            .map(|bt| caller_frame(&format!("{bt}")))
            .unwrap_or_default();
        let full = format!("{description}{frame}");
        log_for_debugging(
            &format!("[SLOW OPERATION DETECTED] {full} ({:.1}ms)", duration_ms),
            DebugLogLevel::Debug,
        );
        add_slow_operation(&full, duration_ms);
    }
}

/// 慢操作守卫工厂（等价 TS `slowLogging` tagged template）。
///
/// `build` 闭包**只在操作确认超阈值时才求值**，因此可以放心放入
/// 描述性的格式化逻辑（对齐 TS 懒构造描述的设计）。
///
/// ```ignore
/// let _guard = slow_logging(|| "JSON.stringify".to_string());
/// let _json = serde_json::to_string(&serde_json::json!({"a": 1})).unwrap_or_default();
/// ```
pub fn slow_logging<F>(build: F) -> SlowOperationGuard
where
    F: FnOnce() -> String + Send + 'static,
{
    let enabled = slow_logging_enabled();
    SlowOperationGuard {
        start: enabled.then(Instant::now),
        build_description: if enabled {
            Some(Box::new(build))
        } else {
            None
        },
        backtrace: enabled.then(Backtrace::capture),
    }
}

/// 便捷守卫工厂：描述为固定标签（对齐 TS `slowLogging\`label\`` 无插值场景）。
pub fn slow_logging_named(label: &str) -> SlowOperationGuard {
    let label = label.to_string();
    slow_logging(move || label)
}

// ---------------------------------------------------------------------------
// 包装操作
// ---------------------------------------------------------------------------

/// 包装 `serde_json::to_string`，带慢操作计时。
pub fn json_stringify(value: &Value) -> String {
    let _guard = slow_logging(|| format!("JSON.stringify({})", describe_value(value)));
    serde_json::to_string(value).unwrap_or_default()
}

/// 包装 `serde_json::to_string_pretty`，带慢操作计时（对齐 TS 的
/// `jsonStringify(value, null, 2)` 用法）。
pub fn json_stringify_pretty(value: &Value, spaces: usize) -> String {
    let _guard = slow_logging(|| format!("JSON.stringify({})", describe_value(value)));
    serde_json::to_string_pretty(value)
        .map(|s| {
            if spaces == 2 {
                s
            } else {
                // serde_json 仅支持 2 空格 pretty；其余要求按原样返回
                s.replace("  ", &" ".repeat(spaces))
            }
        })
        .unwrap_or_default()
}

/// 包装 `serde_json::from_str`，带慢操作计时。
/// 与 TS 抛异常不同，Rust 侧返回 `Result`。
pub fn json_parse(text: &str) -> Result<Value, serde_json::Error> {
    let _guard = slow_logging(|| format!("JSON.parse({})", describe_string(text)));
    serde_json::from_str(text)
}

/// 深拷贝（等价 TS `structuredClone` 包装）。`Value::clone` 即整树深拷贝。
pub fn clone(value: &Value) -> Value {
    let _guard = slow_logging(|| format!("structuredClone({})", describe_value(value)));
    value.clone()
}

/// 深拷贝（等价 TS 的 lodash `cloneDeep` 包装）。`Value` 的深拷贝语义
/// 与 lodash 一致（仅 JSON 可表达的树）。
pub fn clone_deep(value: &Value) -> Value {
    let _guard = slow_logging(|| format!("cloneDeep({})", describe_value(value)));
    value.clone()
}

/// 包装 `fs.writeFileSync`（已废弃：非阻塞场景应改用 `fs::write` 的
/// 异步等价物）。`flush=true` 时 open→write→fsync→close（对齐 Node
/// 20.1+ 的 `{ flush: true }` 选项）。
pub fn write_file_sync_deprecated(path: &str, data: &[u8], flush: bool) -> io::Result<()> {
    let _guard = slow_logging(|| {
        format!(
            "fs.writeFileSync({}, {})",
            describe_string(path),
            describe_string(&String::from_utf8_lossy(data))
        )
    });

    if flush {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(data)?;
        file.sync_all()?;
    } else {
        fs::write(path, data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_frame_skips_this_file_and_formats_tail() {
        let stack = "  at foo (src/utils/slow_operations.rs:10:5)\n  at bar (src/app.rs:42:7)\n  at baz (src/app.rs:48:3)";
        assert_eq!(caller_frame(stack), " @ app.rs:42");
    }

    #[test]
    fn caller_frame_handles_missing_frame() {
        assert_eq!(caller_frame(""), "");
        assert_eq!(caller_frame("some line without frames"), "");
    }

    #[test]
    fn describe_value_shapes() {
        assert_eq!(describe_value(&Value::Null), "null");
        assert_eq!(describe_value(&Value::Bool(true)), "true");
        assert_eq!(
            describe_value(&Value::String("hello".into())),
            "hello".to_string()
        );
        assert_eq!(
            describe_value(&Value::Array(vec![Value::Null, Value::Null])),
            "Array[2]"
        );
        let obj = serde_json::json!({"a": 1, "b": 2, "c": 3});
        assert_eq!(describe_value(&obj), "Object{3} keys");
    }

    #[test]
    fn describe_string_truncates_at_80_chars() {
        let short = "abc";
        assert_eq!(describe_string(short), "abc");
        let long: String = "x".repeat(100);
        let described = describe_string(&long);
        let truncated: String = "x".repeat(80);
        assert_eq!(described, format!("{truncated}…"));
    }

    #[test]
    fn threshold_is_nonnegative() {
        assert!(get_slow_operation_threshold_ms() >= 0.0);
    }

    #[test]
    fn guard_drop_without_feature_is_noop() {
        // 未启用 FEATURE_SLOW_OPERATION_LOGGING（或阈值 Infinity）时，
        // 守卫 Drop 不得 panic、不得登记。
        let _guard = slow_logging(|| "should not be built".to_string());
        let _named = slow_logging_named("test op");
    }

    #[test]
    fn wrapped_operations_roundtrip() {
        let value = serde_json::json!({"k": [1, 2, 3]});
        let s = json_stringify(&value);
        assert_eq!(json_parse(&s).unwrap(), value);
        assert_eq!(clone(&value), value);
        assert_eq!(clone_deep(&value), value);
        assert!(json_stringify_pretty(&value, 2).contains('\n'));
    }

    #[test]
    fn write_file_sync_deprecated_writes_without_flush() {
        let dir = std::env::temp_dir();
        let path = dir.join("ccb_slow_ops_test.txt");
        write_file_sync_deprecated(path.to_str().unwrap(), b"hello", false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        write_file_sync_deprecated(path.to_str().unwrap(), b"world", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        let _ = std::fs::remove_file(&path);
    }
}
