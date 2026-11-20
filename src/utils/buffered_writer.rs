//! buffered_writer.rs — 带缓冲与定时刷盘的写入器。
//!
//! 重写自 `src/utils/bufferedWriter.ts`。把高频小写入攒成批量写入，
//! 写入函数本身可能阻塞（如 errorLogSink.ts 的 appendFileSync）。
//!
//! 与 TS 的差异（无事件循环）：
//! - 定时刷盘改为后台线程（按 flush_interval 循环，等价 setTimeout）
//! - 溢出快照（TS 的 flushDeferred/setImmediate 异步写）改为同步
//!   剥离 + 同步落地：写入顺序与可见性完全一致；唯一差异是 write()
//!   在溢出时的延迟落到了当前调用而非下一个 tick。pending_overflow
//!   的合并语义（保持排序、尽量一次写完）保留。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type WriteFn = Arc<dyn Fn(&str) + Send + Sync>;

struct Inner {
    /// 缓冲状态与溢出批
    state: Mutex<State>,
    /// 串行化对 write_fn 的实际调用（避免定时线程与溢出写交叉）
    write_lock: Mutex<()>,
    write_fn: WriteFn,
    flush_interval: Duration,
    max_buffer_size: usize,
    /// 0 表示无字节上限（对齐 TS 的 Infinity）。
    max_buffer_bytes: usize,
    immediate_mode: bool,
}

struct State {
    buffer: Vec<String>,
    buffer_bytes: usize,
}

/// 缓冲写入器。
///
/// 注意：与 TS 一致，drop 不保证刷盘（TS 的 dispose 只是 flush 的
/// 别名），但本实现为短生命周期写入器在 drop 时尽力刷一次盘，
/// 减少纯 Rust 端的数据丢失面。
pub struct BufferedWriter {
    inner: Arc<Inner>,
    _flush_thread: Option<thread::JoinHandle<()>>,
}

pub struct BufferedWriterOptions {
    pub write_fn: WriteFn,
    pub flush_interval_ms: u64,
    pub max_buffer_size: usize,
    /// None 表示无字节上限（对齐 TS 的 Infinity）。
    pub max_buffer_bytes: Option<usize>,
    pub immediate_mode: bool,
}

impl Default for BufferedWriterOptions {
    fn default() -> Self {
        Self {
            write_fn: Arc::new(|_content: &str| {}),
            flush_interval_ms: 1000,
            max_buffer_size: 100,
            max_buffer_bytes: None,
            immediate_mode: false,
        }
    }
}

impl BufferedWriter {
    pub fn new(options: BufferedWriterOptions) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                buffer: Vec::new(),
                buffer_bytes: 0,
            }),
            write_lock: Mutex::new(()),
            write_fn: options.write_fn,
            flush_interval: Duration::from_millis(options.flush_interval_ms.max(1)),
            max_buffer_size: options.max_buffer_size.max(1),
            max_buffer_bytes: options.max_buffer_bytes.unwrap_or(0),
            immediate_mode: options.immediate_mode,
        });

        let _flush_thread = if options.immediate_mode {
            None
        } else {
            let inner = Arc::clone(&inner);
            Some(thread::spawn(move || {
                loop {
                    thread::sleep(inner.flush_interval);
                    flush_impl(&inner);
                }
            }))
        };

        Self {
            inner,
            _flush_thread,
        }
    }

    /// 写入一段内容。immediate 模式直接调用 write_fn。
    pub fn write(&self, content: &str) {
        if self.inner.immediate_mode {
            let _guard = self
                .inner
                .write_lock
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            (self.inner.write_fn)(content);
            return;
        }

        let detached = {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.buffer.push(content.to_string());
            state.buffer_bytes += content.len();
            let size_hit = state.buffer.len() >= self.inner.max_buffer_size;
            let bytes_hit = self.inner.max_buffer_bytes > 0
                && state.buffer_bytes >= self.inner.max_buffer_bytes;
            if size_hit || bytes_hit {
                // 剥离缓冲（flushDeferred 的同步版本）
                let detached = std::mem::take(&mut state.buffer);
                state.buffer_bytes = 0;
                Some(detached)
            } else {
                None
            }
        };

        if let Some(detached) = detached {
            let _guard = self
                .inner
                .write_lock
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            (self.inner.write_fn)(&detached.join(""));
        }
    }

    /// 刷盘：写空当前缓冲（无内容时立即返回）。
    pub fn flush(&self) {
        flush_impl(&self.inner);
    }

    /// 刷盘并停止使用（与 TS 一样只是 flush 的别名）。
    pub fn dispose(&self) {
        flush_impl(&self.inner);
    }
}

fn flush_impl(inner: &Arc<Inner>) {
    let batch = {
        let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.buffer.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut state.buffer);
        state.buffer_bytes = 0;
        batch
    };
    let _guard = inner.write_lock.lock().unwrap_or_else(|p| p.into_inner());
    (inner.write_fn)(&batch.join(""));
}

impl Drop for BufferedWriter {
    fn drop(&mut self) {
        // 进程退出策略由 cleanup registry 管理；这里仅尽力刷一次盘，
        // 避免短生命周期写入器丢数据（TS 端依赖 registerCleanup）。
        flush_impl(&self.inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter_fn() -> (WriteFn, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(Mutex::new(Vec::<String>::new()));
        let write_fn: WriteFn = {
            let calls = Arc::clone(&calls);
            let output = Arc::clone(&output);
            Arc::new(move |content: &str| {
                calls.fetch_add(1, Ordering::SeqCst);
                output.lock().unwrap().push(content.to_string());
            })
        };
        (write_fn, calls, output)
    }

    #[test]
    fn immediate_mode_bypasses_buffer() {
        let (write_fn, calls, output) = counter_fn();
        let writer = BufferedWriter::new(BufferedWriterOptions {
            write_fn,
            immediate_mode: true,
            ..Default::default()
        });
        writer.write("a");
        writer.write("b");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *output.lock().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn flush_writes_joined_buffer_once() {
        let (write_fn, calls, output) = counter_fn();
        let writer = BufferedWriter::new(BufferedWriterOptions {
            write_fn,
            immediate_mode: false,
            ..Default::default()
        });
        writer.write("a");
        writer.write("b");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        writer.flush();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*output.lock().unwrap(), vec!["ab".to_string()]);
        // 二次 flush 无内容时不再调用 write_fn
        writer.flush();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn overflow_flushes_immediately() {
        let (write_fn, calls, output) = counter_fn();
        let writer = BufferedWriter::new(BufferedWriterOptions {
            write_fn,
            max_buffer_size: 2,
            immediate_mode: false,
            ..Default::default()
        });
        writer.write("a");
        writer.write("b");
        // 2 条即触发 maxBufferSize=2 的溢出
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*output.lock().unwrap(), vec!["ab".to_string()]);
    }

    #[test]
    fn byte_budget_overflow() {
        let (write_fn, calls, _output) = counter_fn();
        let writer = BufferedWriter::new(BufferedWriterOptions {
            write_fn,
            max_buffer_bytes: Some(10),
            immediate_mode: false,
            ..Default::default()
        });
        writer.write("0123456789XX"); // 12 > 10 → 立即溢出写
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
