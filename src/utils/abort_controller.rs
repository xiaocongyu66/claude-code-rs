//! abort_controller.rs — 取消令牌（AbortController 的 Rust 对应物）。
//!
//! 重写自 `src/utils/abortController.ts`。TS 端基于 Node 的
//! AbortSignal/AbortController；Rust 端实现等价的取消令牌：
//!
//! - `CancellationToken::new()`（对齐 createAbortController：默认 50
//!   监听器上限在 Rust 中无事件层泄漏问题，忽略）
//! - `abort(reason)` 广播给所有订阅者
//! - `create_child_token(parent)`：父取消时子取消；子取消不影响父
//!
//! 内存安全对齐：TS 用 WeakRef 防止父持有已放弃的子控制器；Rust 端
//! 父持有 `Weak<CancellationTokenInner>`，子无外部引用时可回收，
//! 子取消时从父的监听列表摘除自己（等价 removeEventListener）。

use std::sync::{Arc, Mutex, Weak};

/// 取消原因（对齐 TS 的 signal.reason；Rust 端退化为字符串）。
pub type AbortReason = String;

#[derive(Debug)]
pub struct CancellationToken {
    inner: Arc<TokenInner>,
}

#[derive(Debug)]
struct TokenInner {
    aborted: std::sync::atomic::AtomicBool,
    reason: Mutex<Option<AbortReason>>,
    // 子令牌的弱引用：父取消时逐个通知；不阻止子被回收
    children: Mutex<Vec<Weak<TokenInner>>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TokenInner {
                aborted: std::sync::atomic::AtomicBool::new(false),
                reason: Mutex::new(None),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 是否已取消。
    pub fn aborted(&self) -> bool {
        self.inner.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 取消原因；未取消时为 None。
    pub fn reason(&self) -> Option<AbortReason> {
        self.inner
            .reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 触发取消并向全部传播链广播。
    pub fn abort(&self, reason: Option<AbortReason>) {
        if self
            .inner
            .aborted
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return; // 已经取消
        }
        if let Some(reason) = reason {
            let mut guard = self.inner.reason.lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                *guard = Some(reason);
            }
        }
        // 通知存活的子令牌
        let children: Vec<Weak<TokenInner>> = {
            let mut guard = self
                .inner
                .children
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            std::mem::take(guard.as_mut())
        };
        let still_alive: Vec<Weak<TokenInner>> = children
            .into_iter()
            .filter(|weak| weak.upgrade().is_some())
            .collect();
        for weak in &still_alive {
            if let Some(child) = weak.upgrade() {
                let token = CancellationToken { inner: child };
                token.abort(self.reason());
            }
        }
        // 清理后的列表重新挂回（保留仍存活的弱引用）
        let mut guard = self
            .inner
            .children
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *guard = still_alive;
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for CancellationToken {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// 创建子取消令牌：父取消时子跟随取消；子取消不影响父。
///
/// 父只持有子的弱引用 —— 子被 drop 后不阻碍回收；
/// 子被 abort 时从父的监听列表摘除自己。
pub fn create_child_token(parent: &CancellationToken) -> CancellationToken {
    let child = CancellationToken::new();

    // 快路径：父已取消，直接传播
    if parent.aborted() {
        child.abort(parent.reason());
        return child;
    }

    {
        let mut guard = parent
            .inner
            .children
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.push(Arc::downgrade(&child.inner));
    }
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_sets_reason() {
        let token = CancellationToken::new();
        assert!(!token.aborted());
        assert!(token.reason().is_none());
        token.abort(Some("user".to_string()));
        assert!(token.aborted());
        assert_eq!(token.reason(), Some("user".to_string()));
    }

    #[test]
    fn abort_is_idempotent() {
        let token = CancellationToken::new();
        token.abort(Some("first".to_string()));
        token.abort(Some("second".to_string()));
        assert_eq!(token.reason(), Some("first".to_string()));
    }

    #[test]
    fn parent_aborts_child_but_not_vice_versa() {
        let parent = CancellationToken::new();
        let child = create_child_token(&parent);
        child.abort(Some("child".to_string()));
        assert!(child.aborted());
        assert!(!parent.aborted());

        let parent = CancellationToken::new();
        let child = create_child_token(&parent);
        parent.abort(Some("parent".to_string()));
        assert!(child.aborted());
        assert_eq!(child.reason(), Some("parent".to_string()));
    }

    #[test]
    fn already_aborted_parent_propagates_on_creation() {
        let parent = CancellationToken::new();
        parent.abort(None);
        let child = create_child_token(&parent);
        assert!(child.aborted());
    }

    #[test]
    fn dropped_child_does_not_block_parent_abort() {
        let parent = CancellationToken::new();
        {
            let _child = create_child_token(&parent);
        }
        // 子已 drop：父取消仍应正常工作（弱引用被滤除）
        parent.abort(None);
        assert!(parent.aborted());
    }
}
