//! stream.rs — 一次性异步迭代流的 Rust 对应实现。
//!
//! 重写自 `src/utils/stream.ts`。生产者 enqueue、消费者 next 的单消费
//! 队列流；只能迭代一次。
//!
//! TS 基于 Promise 组合异步迭代器；Rust 端用 `Mutex + Condvar` 实现
//! 等价的阻塞迭代器（无全局异步运行时依赖）：
//! - next()：队列非空立即出队；已 done 返回 None；有 error 返回 Err；
//!   否则挂起等待 enqueue/done/error
//! - enqueue：若消费者正挂起则直接唤醒交付，否则入队
//! - done / error / return_：终止流，唤醒挂起的消费者

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

struct StreamState<T> {
    queue: VecDeque<T>,
    is_done: bool,
    error: Option<String>,
    started: bool,
}

/// 一次性单消费流。
pub struct Stream<T> {
    state: Mutex<StreamState<T>>,
    cvar: Condvar,
    /// 迭代提前结束（return_）时的回调（TS 的 returned）。
    returned: Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
}

impl<T> Stream<T> {
    pub fn new() -> Self {
        Self::new_with_returned(None)
    }

    /// 带提前结束回调的流（对齐 TS 构造器的 returned 参数）。
    pub fn new_with_returned(returned: Option<Box<dyn FnOnce() + Send + Sync>>) -> Self {
        Self {
            state: Mutex::new(StreamState {
                queue: VecDeque::new(),
                is_done: false,
                error: None,
                started: false,
            }),
            cvar: Condvar::new(),
            returned: Mutex::new(returned),
        }
    }

    /// 开始迭代（只能一次；重复调用返回错误，对齐 TS 的 throw）。
    pub fn start(&self) -> Result<(), String> {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if guard.started {
            return Err("Stream can only be iterated once".to_string());
        }
        guard.started = true;
        Ok(())
    }

    /// 取下一条；流终止返回 None，出错返回 Err。
    pub fn next(&self) -> Result<Option<T>, String> {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(value) = guard.queue.pop_front() {
                return Ok(Some(value));
            }
            if guard.is_done {
                return if let Some(error) = guard.error.take() {
                    Err(error)
                } else {
                    Ok(None)
                };
            }
            guard = self.cvar.wait(guard).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// 入队一条；若消费者正挂起则直接唤醒交付。
    pub fn enqueue(&self, value: T) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.queue.push_back(value);
        self.cvar.notify_one();
    }

    /// 正常终止流。
    pub fn done(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.is_done = true;
        self.cvar.notify_one();
    }

    /// 以错误终止流。
    pub fn error(&self, error: String) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.error = Some(error);
        guard.is_done = true;
        self.cvar.notify_one();
    }

    /// 提前结束迭代（对齐 TS 的 return()）：终止流并调用 returned 回调。
    pub fn return_(&self) {
        let returned = self
            .returned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        guard.is_done = true;
        self.cvar.notify_one();
        drop(guard);
        if let Some(returned) = returned {
            returned();
        }
    }
}

impl<T> Default for Stream<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn start_only_once() {
        let stream = Stream::<i32>::new();
        assert!(stream.start().is_ok());
        assert!(stream.start().is_err());
    }

    #[test]
    fn enqueue_then_next() {
        let stream = Stream::<i32>::new();
        stream.enqueue(1);
        stream.enqueue(2);
        assert_eq!(stream.next(), Ok(Some(1)));
        assert_eq!(stream.next(), Ok(Some(2)));
    }

    #[test]
    fn done_terminates() {
        let stream = Stream::<i32>::new();
        stream.enqueue(1);
        stream.done();
        assert_eq!(stream.next(), Ok(Some(1)));
        assert_eq!(stream.next(), Ok(None));
    }

    #[test]
    fn error_surfaces_once() {
        let stream = Stream::<i32>::new();
        stream.enqueue(1);
        stream.error("boom".to_string());
        assert_eq!(stream.next(), Ok(Some(1)));
        assert_eq!(stream.next(), Err("boom".to_string()));
        // error 只回报一次，之后视为 done
        assert_eq!(stream.next(), Ok(None));
    }

    #[test]
    fn blocked_next_wakes_on_enqueue() {
        let stream = Arc::new(Stream::<i32>::new());
        let s = Arc::clone(&stream);
        let handle = thread::spawn(move || s.next());
        thread::sleep(std::time::Duration::from_millis(50));
        stream.enqueue(42);
        assert_eq!(handle.join().unwrap(), Ok(Some(42)));
    }

    #[test]
    fn return_invokes_callback() {
        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        let stream = Stream::<i32>::new_with_returned(Some(Box::new(move || {
            *called_clone.lock().unwrap() = true;
        })));
        let s = Arc::new(stream);
        let sc = Arc::clone(&s);
        let handle = thread::spawn(move || sc.next());
        thread::sleep(std::time::Duration::from_millis(50));
        s.return_();
        assert_eq!(handle.join().unwrap(), Ok(None));
        assert!(*called.lock().unwrap());
    }
}
