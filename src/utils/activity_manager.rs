//! activity_manager.rs — 用户与 CLI 活动时间的通用追踪器。
//!
//! 重写自 `src/utils/activityManager.ts`。语义对齐：
//!
//! - TS 的单例（`ActivityManager.instance` + module-level `activityManager`）
//!   在 Rust 侧用 `once_cell::sync::Lazy` + `Mutex` 的全局实例提供，同时
//!   保留可独立 new 出来做测试的结构体 `ActivityManager`。
//! - `getNow` / `getActiveTimeCounter` 可注入（对齐 TS 的
//!   ActivityManagerOptions）；时间戳单位为毫秒（`Date.now()`）。
//! - 活动时长写入 `bootstrap::state` 的 `AttributedCounter`，属性
//!   `{ type: 'user' | 'cli' }`（对齐 TS `activeTimeCounter.add(sec, {type})`）。
//! - TS 用 Promise 写 `trackOperation`；Rust 用 `async fn` + 终结保证
//!   （等价 try/finally：即便 `f` 失败也调用 end_cli_activity）。

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;

use crate::bootstrap::state::{get_active_time_counter, AttributeValue, AttributedCounter};

/// 活动类型属性（写入计数器的 attributes）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityType {
    User,
    Cli,
}

impl ActivityType {
    fn as_str(self) -> &'static str {
        match self {
            ActivityType::User => "user",
            ActivityType::Cli => "cli",
        }
    }
}

/// 活动状态快照（对齐 TS `getActivityStates()`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityStates {
    pub is_user_active: bool,
    pub is_cli_active: bool,
    pub active_operation_count: usize,
}

/// 注入式时钟（对齐 TS 的 `getNow?: () => number`）。
pub type NowFn = fn() -> u64;
/// 注入式计数器获取器（对齐 TS 的 `getActiveTimeCounter?`）。
pub type ActiveTimeCounterFn = fn() -> Option<std::sync::Arc<dyn AttributedCounter>>;

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_active_time_counter() -> Option<std::sync::Arc<dyn AttributedCounter>> {
    get_active_time_counter()
}

/// ActivityManager 处理用户与 CLI 操作的通用活动追踪。
/// 自动去重重叠活动，并分别统计用户与 CLI 活跃时间。
pub struct ActivityManager {
    active_operations: Mutex<std::collections::HashSet<String>>,
    last_user_activity_time: Mutex<u64>, // 0 表示尚无活动
    last_cli_recorded_time: Mutex<u64>,
    is_cli_active: Mutex<bool>,
    user_activity_timeout_ms: u64, // 固定 5000，对齐 TS 常量
    get_now: NowFn,
    get_active_time_counter: ActiveTimeCounterFn,
}

impl ActivityManager {
    pub fn new() -> Self {
        Self::with_options(None, None)
    }

    /// 带可注入依赖的构造（对齐 TS `ActivityManagerOptions`）。
    pub fn with_options(get_now: Option<NowFn>, get_counter: Option<ActiveTimeCounterFn>) -> Self {
        let now_fn = get_now.unwrap_or(default_now);
        Self {
            active_operations: Mutex::new(std::collections::HashSet::new()),
            last_user_activity_time: Mutex::new(0),
            last_cli_recorded_time: Mutex::new(now_fn()),
            is_cli_active: Mutex::new(false),
            user_activity_timeout_ms: 5000,
            get_now: now_fn,
            get_active_time_counter: get_counter.unwrap_or(default_active_time_counter),
        }
    }

    fn record_active_time(&self, seconds: f64, activity_type: ActivityType) {
        if seconds <= 0.0 {
            return;
        }
        if let Some(counter) = (self.get_active_time_counter)() {
            counter.add(
                seconds,
                Some(&vec![("type".to_string(), AttributeValue::Str(activity_type.as_str().to_string()))]),
            );
        }
    }

    /// 用户与 CLI 交互（打字、命令等）时调用。
    pub fn record_user_activity(&self) {
        let now = (self.get_now)();
        let mut last_user = self.last_user_activity_time.lock().unwrap_or_else(|p| p.into_inner());
        let is_cli_active = *self.is_cli_active.lock().unwrap_or_else(|p| p.into_inner());
        // CLI 活跃时不计用户时间（CLI 优先）
        if !is_cli_active && *last_user != 0 {
            let time_since_last_activity = (now - *last_user) as f64 / 1000.0;
            if time_since_last_activity > 0.0 {
                // 仅在超时窗口内记录
                if time_since_last_activity < self.user_activity_timeout_ms as f64 / 1000.0 {
                    self.record_active_time(time_since_last_activity, ActivityType::User);
                }
            }
        }
        *last_user = now;
    }

    /// 开始追踪 CLI 活动（工具执行、AI 响应等）。
    pub fn start_cli_activity(&self, operation_id: &str) {
        let mut ops = self.active_operations.lock().unwrap_or_else(|p| p.into_inner());
        // 操作已存在 ⇒ 上一个没清理干净，强制清理（宁可低估不超高）。
        if ops.contains(operation_id) {
            drop(ops);
            self.end_cli_activity(operation_id);
            ops = self.active_operations.lock().unwrap_or_else(|p| p.into_inner());
        }
        let was_empty = ops.is_empty();
        ops.insert(operation_id.to_string());
        drop(ops);
        if was_empty {
            *self.is_cli_active.lock().unwrap_or_else(|p| p.into_inner()) = true;
            *self.last_cli_recorded_time.lock().unwrap_or_else(|p| p.into_inner()) = (self.get_now)();
        }
    }

    /// 停止追踪 CLI 活动。
    pub fn end_cli_activity(&self, operation_id: &str) {
        let mut ops = self.active_operations.lock().unwrap_or_else(|p| p.into_inner());
        ops.remove(operation_id);
        if !ops.is_empty() {
            return;
        }
        // 最后一个操作结束 —— CLI 转为非活跃，先记录 CLI 时间。
        let now = (self.get_now)();
        let mut last_recorded = self.last_cli_recorded_time.lock().unwrap_or_else(|p| p.into_inner());
        let time_since_last_record = (now - *last_recorded) as f64 / 1000.0;
        *last_recorded = now;
        drop(last_recorded);
        drop(ops);
        self.record_active_time(time_since_last_record, ActivityType::Cli);
        *self.is_cli_active.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }

    /// 自动追踪一个异步操作（mainly for testing/debugging）。
    pub async fn track_operation<F, T, E>(&self, operation_id: &str, f: F) -> Result<T, E>
    where
        F: std::future::Future<Output = Result<T, E>>,
    {
        self.start_cli_activity(operation_id);
        let result = f.await;
        self.end_cli_activity(operation_id);
        result
    }

    /// 获取当前活动状态（mainly for testing/debugging）。
    pub fn get_activity_states(&self) -> ActivityStates {
        let now = (self.get_now)();
        let last_user = *self.last_user_activity_time.lock().unwrap_or_else(|p| p.into_inner());
        let time_since_user_activity = (now - last_user) as f64 / 1000.0;
        let is_user_active = time_since_user_activity < self.user_activity_timeout_ms as f64 / 1000.0;
        ActivityStates {
            is_user_active,
            is_cli_active: *self.is_cli_active.lock().unwrap_or_else(|p| p.into_inner()),
            active_operation_count: self
                .active_operations
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
        }
    }
}

impl Default for ActivityManager {
    fn default() -> Self {
        Self::new()
    }
}

static ACTIVITY_MANAGER: Lazy<ActivityManager> = Lazy::new(ActivityManager::new);

/// 全局单例（对齐 TS module-level `activityManager`）。
pub fn activity_manager() -> &'static ActivityManager {
    &*ACTIVITY_MANAGER
}
