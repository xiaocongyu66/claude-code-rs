//! query_guard.rs — 查询生命周期的同步状态机。
//!
//! 重写自 `src/utils/QueryGuard.ts`。语义对齐：
//!
//! - 三态：`Idle`（无查询，可出队处理）、`Dispatching`（已出队，异步链
//!   还未到达 onQuery）、`Running`（onQuery 调用了 tryStart，查询执行中）。
//! - 转换：Idle→Dispatching（reserve）、Dispatching→Running（tryStart）、
//!   Idle→Running（tryStart，直接提交）、Running→Idle（end/forceEnd）、
//!   Dispatching→Idle（cancelReservation）。
//! - `isActive` 对 Dispatching 与 Running 均为 true，防止队列处理器在
//!   异步间隙重入。
//! - TS 的 `useSyncExternalStore` 接口（subscribe / getSnapshot）在 Rust
//!   侧由 `Signal<()>` 提供：`subscribe()` 返回 drop 即取消的订阅，
//!   `get_snapshot()` 返回 `is_active()`。
//! - 单线程使用：Rust 侧用 `Mutex` 保护内部状态（TS 版也是单事件循环内
//!   同步访问；这里保留锁以便跨线程共享）。

use std::sync::Mutex;

use crate::utils::signal::Signal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Dispatching,
    Running,
}

/// 查询生命周期状态机。
pub struct QueryGuard {
    inner: Mutex<Inner>,
    changed: Signal<()>,
}

struct Inner {
    status: Status,
    generation: u64,
}

impl QueryGuard {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: Status::Idle,
                generation: 0,
            }),
            changed: Signal::new(),
        }
    }

    /// 为队列处理预留。Idle→Dispatching。非 idle 时返回 false。
    pub fn reserve(&self) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.status != Status::Idle {
            return false;
        }
        inner.status = Status::Dispatching;
        drop(inner);
        self.changed.emit(&());
        true
    }

    /// processQueueIfReady 无事可做时取消预留。Dispatching→Idle。
    pub fn cancel_reservation(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.status != Status::Dispatching {
            return;
        }
        inner.status = Status::Idle;
        drop(inner);
        self.changed.emit(&());
    }

    /// 启动一次查询。成功返回 generation 编号；已有查询在运行则返回 None。
    /// 接受从 Idle（直接用户提交）与 Dispatching（队列处理器路径）转换。
    pub fn try_start(&self) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.status == Status::Running {
            return None;
        }
        inner.status = Status::Running;
        inner.generation += 1;
        let generation = inner.generation;
        drop(inner);
        self.changed.emit(&());
        Some(generation)
    }

    /// 结束一次查询。generation 仍为当前则返回 true（调用方应做清理），
    /// 已有更新的查询开始则返回 false（被取消查询的过期 finally 块）。
    pub fn end(&self, generation: u64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.generation != generation {
            return false;
        }
        if inner.status != Status::Running {
            return false;
        }
        inner.status = Status::Idle;
        drop(inner);
        self.changed.emit(&());
        true
    }

    /// 无视 generation 强制结束当前查询（onCancel 用）。
    /// generation 递增，使被取消查询的过期 finally 块看到不匹配而跳过清理。
    pub fn force_end(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.status == Status::Idle {
            return;
        }
        inner.status = Status::Idle;
        inner.generation += 1;
        drop(inner);
        self.changed.emit(&());
    }

    /// 是否活跃（Dispatching 或 Running）。始终同步。
    pub fn is_active(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status
            != Status::Idle
    }

    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    }

    /// 订阅状态变化（对齐 TS `subscribe`）。返回的 Subscription drop 时取消订阅。
    pub fn subscribe<F>(&self, listener: F) -> crate::utils::signal::Subscription<()>
    where
        F: FnMut(&()) + Send + Sync + 'static,
    {
        self.changed.subscribe(listener)
    }

    /// 快照（对齐 TS `getSnapshot`）：返回 `is_active()`。
    pub fn get_snapshot(&self) -> bool {
        self.is_active()
    }
}

impl Default for QueryGuard {
    fn default() -> Self {
        Self::new()
    }
}
