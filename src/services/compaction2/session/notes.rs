//! `services/compaction2/session/notes.ts` — 持久上下文笔记
//! （`context_notes`）与 rollover 请求（`new_context`）：仅存储与注入接口。
//!
//! 参考实现规则：
//! - notebook 整体替换上限 16,384 UTF-8 字节；超限写入失败，不替换笔记。
//! - notebook 修订是日志条目；只有最新修订注入模型上下文，包括
//!   resume/fork 之后。
//! - `text: ""` 清空可见 notebook。
//! - rollover（`new_context`）保留 notebook，由调用方在下一个安全的
//!   tool-loop 边界应用。
//!
//! 工具注册刻意不在本模块范围内。

use std::sync::{Arc, Mutex};

/// TS `CONTEXT_NOTES_MAX_BYTES`。
pub const CONTEXT_NOTES_MAX_BYTES: usize = 16_384;
/// TS `CONTEXT_NOTES_OPEN_TAG`。
pub const CONTEXT_NOTES_OPEN_TAG: &str = "<context_notes>";
/// TS `CONTEXT_NOTES_CLOSE_TAG`。
pub const CONTEXT_NOTES_CLOSE_TAG: &str = "</context_notes>";

/// TS `ContextNotesWriteFailure`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextNotesWriteFailure {
    TooLarge,
    Invalid,
}

/// TS `ContextNotesWriteResult`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextNotesWriteResult {
    Ok { revision: usize, bytes: usize },
    Failed {
        reason: ContextNotesWriteFailure,
        bytes: usize,
    },
}

impl ContextNotesWriteResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, ContextNotesWriteResult::Ok { .. })
    }
}

/// TS `ContextNoteRevision`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextNoteRevision {
    pub revision: usize,
    pub text: String,
    pub bytes: usize,
    pub created_at: i64,
}

/// TS `ContextNotesStore`。Rust 侧以 trait 对象表达，内部 journal 用
/// `Mutex` 提供 TS 的可变方法语义。
pub trait ContextNotesStore: Send + Sync {
    fn max_bytes(&self) -> usize;
    fn write(&self, text: &str, at: Option<i64>) -> ContextNotesWriteResult;
    fn latest(&self) -> Option<ContextNoteRevision>;
    /// 最新修订文本；notebook 为空/已清空时返回 `None`。
    fn visible_text(&self) -> Option<String>;
    fn revisions(&self) -> Vec<ContextNoteRevision>;
}

/// TS `ContextNotesStoreOptions`。
#[derive(Default)]
pub struct ContextNotesStoreOptions {
    pub max_bytes: Option<usize>,
    pub now: Option<Box<dyn Fn() -> i64 + Send + Sync>>,
    pub initial_text: Option<String>,
}

struct InMemoryNotesStore {
    max_bytes: usize,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    journal: Mutex<Vec<ContextNoteRevision>>,
    next_revision: Mutex<usize>,
}

/// TS `utf8ByteLength(text)`：Rust `str::len` 即 UTF-8 字节数。
pub fn utf8_byte_length(text: &str) -> usize {
    text.len()
}

/// TS `createContextNotesStore(options?)`。
pub fn create_context_notes_store(
    options: Option<ContextNotesStoreOptions>,
) -> Arc<dyn ContextNotesStore> {
    let options = options.unwrap_or_default();
    let store = Arc::new(InMemoryNotesStore {
        max_bytes: options.max_bytes.unwrap_or(CONTEXT_NOTES_MAX_BYTES),
        now: options.now.unwrap_or_else(|| Box::new(|| chrono::Utc::now().timestamp_millis())),
        journal: Mutex::new(Vec::new()),
        next_revision: Mutex::new(1),
    });
    if let Some(initial_text) = options.initial_text {
        let _ = store.write(&initial_text, None);
    }
    store
}

impl ContextNotesStore for InMemoryNotesStore {
    fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn write(&self, text: &str, at: Option<i64>) -> ContextNotesWriteResult {
        let bytes = utf8_byte_length(text);
        if bytes > self.max_bytes {
            return ContextNotesWriteResult::Failed {
                reason: ContextNotesWriteFailure::TooLarge,
                bytes,
            };
        }
        let created_at = at.unwrap_or_else(|| (self.now)());
        let mut next_revision = lock_mutex(&self.next_revision);
        let revision = *next_revision;
        *next_revision += 1;
        drop(next_revision);
        let mut journal = lock_mutex(&self.journal);
        journal.push(ContextNoteRevision {
            revision,
            text: text.to_string(),
            bytes,
            created_at,
        });
        ContextNotesWriteResult::Ok { revision, bytes }
    }

    fn latest(&self) -> Option<ContextNoteRevision> {
        let journal = lock_mutex(&self.journal);
        journal.last().cloned()
    }

    fn visible_text(&self) -> Option<String> {
        let journal = lock_mutex(&self.journal);
        let latest = journal.last()?;
        if latest.text.is_empty() {
            None
        } else {
            Some(latest.text.clone())
        }
    }

    fn revisions(&self) -> Vec<ContextNoteRevision> {
        lock_mutex(&self.journal).clone()
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// TS `renderContextNotesInjection(text)`：包装备注入笔记；空文本不产生块。
pub fn render_context_notes_injection(text: Option<&str>) -> String {
    let Some(text) = text else {
        return String::new();
    };
    if text.is_empty() {
        return String::new();
    }
    format!("{CONTEXT_NOTES_OPEN_TAG}\n{text}\n{CONTEXT_NOTES_CLOSE_TAG}")
}

/// TS `ContextNotesInjection`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextNotesInjection {
    pub kind: &'static str,
    pub text: String,
    pub block: String,
    pub visible_revision: usize,
}

/// TS `buildContextNotesInjection(store)`：最新可见修订的注入描述符。
pub fn build_context_notes_injection(
    store: &dyn ContextNotesStore,
) -> Option<ContextNotesInjection> {
    let text = store.visible_text()?;
    let latest = store.latest()?;
    Some(ContextNotesInjection {
        kind: "context_notes",
        block: render_context_notes_injection(Some(&text)),
        visible_revision: latest.revision,
        text,
    })
}

/// TS `NewContextRequest`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewContextRequest {
    pub requested_at: i64,
    pub reason: Option<String>,
}

/// TS `NewContextController`。
pub struct NewContextController {
    pending: Mutex<Option<NewContextRequest>>,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl NewContextController {
    /// TS `request(reason?)`。
    pub fn request(&self, reason: Option<&str>) -> NewContextRequest {
        let request = NewContextRequest {
            requested_at: (self.now)(),
            reason: reason.filter(|reason| !reason.is_empty()).map(String::from),
        };
        *lock_mutex(&self.pending) = Some(request.clone());
        request
    }

    /// TS `pending()`。
    pub fn pending(&self) -> Option<NewContextRequest> {
        lock_mutex(&self.pending).clone()
    }

    /// TS `consume()`：在安全边界取走待处理请求；notebook 保留。
    pub fn consume(&self) -> Option<NewContextRequest> {
        let mut pending = lock_mutex(&self.pending);
        let request = pending.clone();
        *pending = None;
        request
    }
}

/// TS `createNewContextController(now?)`。
pub fn create_new_context_controller(
    now: Option<Box<dyn Fn() -> i64 + Send + Sync>>,
) -> NewContextController {
    NewContextController {
        pending: Mutex::new(None),
        now: now.unwrap_or_else(|| Box::new(|| chrono::Utc::now().timestamp_millis())),
    }
}
