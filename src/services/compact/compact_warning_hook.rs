//! `services/compact/compactWarningHook.ts` — React 订阅钩子。
//!
//! 语义对齐说明：TS 用 `useSyncExternalStore` 订阅 store，是 React 层。
//! React/Ink UI 未转换，因此 Rust 侧提供与 store 同语义的**命令式订阅
//! 句柄**：调用方（未来的渲染层）自行持有返回值并在析构时解绑。

use crate::services::compact::compact_warning_state::COMPACT_WARNING_STORE;

/// TS `useCompactWarningSuppression()` 的非 React 等价物：返回当前抑制
/// 状态与一个订阅句柄。返回 `subscribe` 回调便于调用方解绑。
pub fn use_compact_warning_suppression() -> (
    bool,
    impl Fn() + Send + Sync + 'static,
) {
    let unsubscribe = COMPACT_WARNING_STORE.subscribe(std::sync::Arc::new(|| {}));
    (COMPACT_WARNING_STORE.get_state(), move || (unsubscribe)())
}
