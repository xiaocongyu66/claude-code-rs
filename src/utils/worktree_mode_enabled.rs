//! worktree_mode_enabled.rs — worktree 模式开关。
//!
//! 重写自 `src/utils/worktreeModeEnabled.ts`。原先受 GrowthBook flag
//! 'tengu_worktree_mode' 控制；但 CACHED_MAY_BE_STALE 模式在缓存未
//! 填充时返回默认值 false，会在首次启动时静默吞掉 --worktree
//! （见 https://github.com/anthropics/claude-code/issues/27044），
//! 因此现在对所有用户无条件开启。

pub fn is_worktree_mode_enabled() -> bool {
    true
}
