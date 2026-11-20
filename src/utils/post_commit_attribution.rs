//! post_commit_attribution.rs — prepare-commit-msg 钩子安装（有意保持 no-op）。
//!
//! 重写自 `src/utils/postCommitAttribution.ts`。
//!
//! ⚠ 官方语义待考，防止误造提交信息行为：该函数会写 git hook 文件，
//! 直接影响用户仓库中的提交流程。反编译语料中唯一的 prepare-commit-msg
//! 实体属于 runner 沙箱的 `.runner/git-hooks` 透传脚本，并非 Claude Code
//! 的 attribution hook 安装逻辑；官方 attribution 模块仅见
//! `armedTrailerFiles` 状态字段，hook 脚本内容与安装条件不可 1:1 对齐。
//! 按"宁可少做不可做错"规约保持 noop；恢复实现前不得向用户仓库写入
//! 任何 hook 文件。

/// no-op：不安装任何 prepare-commit-msg 钩子。
pub fn install_prepare_commit_msg_hook(_worktree_path: &str, _worktree_hooks_dir: Option<&str>) {}
