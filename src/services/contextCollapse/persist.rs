//! `services/contextCollapse/persist.ts` — 折叠存储层（故意未实现）。
//!
//! 与 TS 存根语义一致：`restoreFromEntries` 在 `FEATURE_CONTEXT_COLLAPSE`
//! 打开时 panic（fail loudly），防止无存储的折叠管线被静默激活。

/// feature flag 读取（对齐 TS `feature('NAME')`：读 `FEATURE_<NAME>=1`）。
fn feature(name: &str) -> bool {
    std::env::var(format!("FEATURE_{name}"))
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn assert_storage_implemented() {
    if feature("CONTEXT_COLLAPSE") {
        panic!(
            "CONTEXT_COLLAPSE storage is not implemented in this build: restoreFromEntries \
             cannot restore collapsed messages. Remove FEATURE_CONTEXT_COLLAPSE=1 \
             (see scripts/defines.ts) or implement the storage layer first."
        );
    }
}

/// TS `restoreFromEntries(...)` — 占位签名，调用即断言存储未实现。
pub fn restore_from_entries(_entries: &[serde_json::Value]) {
    assert_storage_implemented();
}
