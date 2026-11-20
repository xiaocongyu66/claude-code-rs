//! mailbox.rs — 简单消息信箱（队列 + 等待者）。
//!
//! 重写自 `src/utils/mailbox.ts`。发送时先匹配等待者；未匹配的消息
//! 入队，供 poll/receive 取出。变化通过 `signal` 通知订阅者。
//!
//! 单一 Mutex 同时保护队列与等待者：TS 依赖单线程事件循环保证
//! receive 的"查队列 → 登记等待者"原子进行，Rust 多线程下需要锁。

use crate::utils::signal::Signal;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

/// 消息来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageSource {
    #[default]
    User,
    Teammate,
    System,
    Tick,
    Task,
}

/// 信箱中的消息。
#[derive(Debug, Clone, Default)]
pub struct Message {
    pub id: String,
    pub source: MessageSource,
    pub content: String,
    pub from: Option<String>,
    pub color: Option<String>,
    pub timestamp: String,
}

struct Waiter {
    predicate: Box<dyn FnMut(&Message) -> bool + Send + Sync>,
    fulfill: Sender<Message>,
}

struct Inner {
    queue: Vec<Message>,
    waiters: Vec<Waiter>,
    revision: u64,
}

/// 单信箱：队列 + 等待者，带变更信号。
pub struct Mailbox {
    inner: Mutex<Inner>,
    changed: Signal<()>,
}

impl Mailbox {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: Vec::new(),
                waiters: Vec::new(),
                revision: 0,
            }),
            changed: Signal::new(),
        }
    }

    /// 当前队列长度。
    pub fn length(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue
            .len()
    }

    /// 信箱变更版本号（每次 send/receive 等写操作递增）。
    pub fn revision(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .revision
    }

    /// 投递消息：先尝试匹配等待者，命中则直接交付；否则入队。
    pub fn send(&self, msg: Message) {
        let matched = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.revision += 1;
            // 在等待者中找到第一个匹配者
            let mut found = None;
            for (idx, waiter) in guard.waiters.iter_mut().enumerate() {
                if (waiter.predicate)(&msg) {
                    found = Some(idx);
                    break;
                }
            }
            found.map(|idx| guard.waiters.remove(idx))
        };

        if let Some(waiter) = matched {
            let _ = waiter.fulfill.send(msg);
            self.changed.emit(&());
            return;
        }

        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue
            .push(msg);
        self.changed.emit(&());
    }

    /// 取出第一条匹配谓词的消息；队列空或不匹配返回 None。
    pub fn poll(&self, mut predicate: impl FnMut(&Message) -> bool) -> Option<Message> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let idx = guard.queue.iter().position(|msg| predicate(msg))?;
        Some(guard.queue.remove(idx))
    }

    /// 订阅变更信号（对齐 TS 的 `mailbox.subscribe`）。
    pub fn subscribe<F>(&self, listener: F) -> crate::utils::signal::Subscription<()>
    where
        F: FnMut(&()) + Send + Sync + 'static,
    {
        self.changed.subscribe(listener)
    }
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 阻塞等待直至收到匹配谓词的消息。
///
/// 与 TS 的 `receive()` 语义一致：队列中已有匹配项则立即返回；
/// 否则登记等待者，`send` 匹配后通过 channel 唤醒。
/// 查队列与登记等待者在同一锁内完成，避免多线程下的错过。
pub fn receive(
    mailbox: &Mailbox,
    mut predicate: impl FnMut(&Message) -> bool + Send + Sync + 'static,
) -> Message {
    let (tx, rx) = std::sync::mpsc::channel::<Message>();
    {
        let mut guard = mailbox.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(idx) = guard.queue.iter().position(|msg| predicate(msg)) {
            let msg = guard.queue.remove(idx);
            guard.revision += 1;
            mailbox.changed.emit(&());
            return msg;
        }
        // 队列中无匹配项：在同一锁内登记等待者
        guard.waiters.push(Waiter {
            predicate: Box::new(predicate),
            fulfill: tx,
        });
    }
    // 等待者被 fulfill 前不可能被 drop（sender 存活在信箱内部）；
    // channel 关闭属不可达路径，返回空消息兜底而非 panic。
    rx.recv().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str) -> Message {
        Message {
            id: id.to_string(),
            source: MessageSource::Teammate,
            content: "hello".to_string(),
            from: Some("tester".to_string()),
            color: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn send_then_poll() {
        let mailbox = Mailbox::new();
        mailbox.send(msg("1"));
        assert_eq!(mailbox.length(), 1);
        let got = mailbox.poll(|m| m.id == "1");
        assert!(got.is_some());
        assert_eq!(mailbox.length(), 0);
    }

    #[test]
    fn revision_increments_on_send() {
        let mailbox = Mailbox::new();
        let r0 = mailbox.revision();
        mailbox.send(msg("1"));
        assert!(mailbox.revision() > r0);
    }

    #[test]
    fn changed_signal_fires_on_send() {
        let mailbox = Mailbox::new();
        let hits = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let _sub = {
            let hits = std::sync::Arc::clone(&hits);
            mailbox.subscribe(move |()| {
                *hits.lock().unwrap() += 1;
            })
        };
        mailbox.send(msg("1"));
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    #[test]
    fn receive_returns_queued_or_blocks_until_send() {
        let mailbox = std::sync::Arc::new(Mailbox::new());
        let mbox = std::sync::Arc::clone(&mailbox);
        let handle = std::thread::spawn(move || receive(&mbox, |_| true));
        // 等待者登记后发送
        std::thread::sleep(std::time::Duration::from_millis(50));
        mailbox.send(msg("for-waiter"));
        let received = handle.join().expect("thread completes");
        assert_eq!(received.id, "for-waiter");
    }

    #[test]
    fn send_goes_to_matched_waiter_not_queue() {
        let mailbox = std::sync::Arc::new(Mailbox::new());
        let mbox = std::sync::Arc::clone(&mailbox);
        let handle = std::thread::spawn(move || receive(&mbox, |m| m.id == "target"));
        std::thread::sleep(std::time::Duration::from_millis(50));
        mailbox.send(msg("nope")); // 不匹配 → 入队
        mailbox.send(msg("target")); // 匹配 → 直达等待者
        let received = handle.join().expect("thread completes");
        assert_eq!(received.id, "target");
        assert_eq!(mailbox.length(), 1); // 只剩 nope
        // 剩余消息仍可 poll
        let leftover = mailbox.poll(|m| m.id == "nope");
        assert!(leftover.is_some());
    }
}
