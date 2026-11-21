//! `services/compaction2/session/` — 压缩会话日志（compaction JSONL 行）、
//! 显示消息（边界/通知盖章）与 resume 重建。
//!
//! TS 侧没有 index.ts（按文件名直接 import）；Rust 用 mod.rs 声明四个
//! 子模块并对齐 TS 的导出面。

pub mod display;
pub mod entries;
pub mod notes;
pub mod rebuild;

pub use display::{
    create_compaction_boundary_message, create_reduced_notice_message,
    create_reduced_notice_state, take_reduced_notice, with_compaction_method,
    CompactionBoundaryInput, CompactionBoundaryTrigger, CompactionMethodMetadata,
    ReducedNoticeInput, ReducedNoticeState,
};
pub use entries::{
    bound_attachment_meta, bound_attachment_meta_list, bound_attachment_meta_pair,
    create_compaction_session_entry, is_compaction_session_entry, project_attachment_meta,
    sanitize_attachment_meta_list, sanitize_attachment_meta_pair, CompactionEntryAttachmentMeta,
    CompactionSessionEntry, CreateCompactionSessionEntryInput,
};
pub use notes::{
    build_context_notes_injection, create_context_notes_store, create_new_context_controller,
    render_context_notes_injection, ContextNoteRevision, ContextNotesInjection,
    ContextNotesStore, ContextNotesStoreOptions, ContextNotesWriteResult, NewContextController,
    NewContextRequest,
};
pub use rebuild::{rebuild_messages_from_entries, RebuildEntry};
