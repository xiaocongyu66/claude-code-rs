//! aws_auth_status_manager.rs — 云 provider 鉴权状态单例管理器。
//!
//! 重写自 `src/utils/awsAuthStatusManager.ts`。原名只针对 AWS，
//! 现服务于所有云鉴权刷新流程（AWS Bedrock、GCP Vertex）；
//! SDK 的 'auth_status' 消息形态与 provider 无关，故用单一管理器。

use crate::utils::signal::Signal;
use std::sync::{Arc, Mutex, OnceLock};

/// provider 鉴权状态快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthStatus {
    pub is_authenticating: bool,
    pub output: Vec<String>,
    pub error: Option<String>,
}

struct Manager {
    status: Mutex<AwsAuthStatus>,
    changed: Signal<AwsAuthStatus>,
}

static INSTANCE: OnceLock<Mutex<Option<Arc<Manager>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Manager>>> {
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// 管理器句柄：持有单例引用，提供状态读写与订阅。
pub struct AwsAuthStatusManager {
    inner: Arc<Manager>,
}

impl AwsAuthStatusManager {
    /// 获取（必要时创建）单例。
    pub fn get_instance() -> Self {
        let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard.get_or_insert_with(|| {
            Arc::new(Manager {
                status: Mutex::new(AwsAuthStatus {
                    is_authenticating: false,
                    output: Vec::new(),
                    error: None,
                }),
                changed: Signal::new(),
            })
        });
        Self {
            inner: Arc::clone(entry),
        }
    }

    /// 当前状态快照（输出列表为防御性拷贝）。
    pub fn get_status(&self) -> AwsAuthStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn mutate<F: FnOnce(&mut AwsAuthStatus)>(&self, f: F) {
        let snapshot = {
            let mut status = self.inner.status.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut status);
            status.clone()
        };
        self.inner.changed.emit(&snapshot);
    }

    /// 开始一次鉴权：进入 authenticating、清空输出与错误。
    pub fn start_authentication(&self) {
        self.mutate(|status| {
            status.is_authenticating = true;
            status.output.clear();
            status.error = None;
        });
    }

    /// 追加一行鉴权过程输出。
    pub fn add_output(&self, line: &str) {
        self.mutate(|status| {
            status.output.push(line.to_string());
        });
    }

    /// 设置鉴权错误信息（保留已有输出）。
    pub fn set_error(&self, error: &str) {
        self.mutate(|status| {
            status.error = Some(error.to_string());
        });
    }

    /// 结束一次鉴权：成功时完全清空状态；失败时保留输出便于排错。
    pub fn end_authentication(&self, success: bool) {
        self.mutate(|status| {
            if success {
                status.is_authenticating = false;
                status.output.clear();
                status.error = None;
            } else {
                status.is_authenticating = false;
            }
        });
    }

    /// 订阅状态变化（对齐 TS 的 `manager.subscribe`）。
    pub fn subscribe<F>(&self, listener: F) -> crate::utils::signal::Subscription<AwsAuthStatus>
    where
        F: FnMut(&AwsAuthStatus) + Send + Sync + 'static,
    {
        self.inner.changed.subscribe(listener)
    }

    /// 测试用：销毁单例（对齐 TS 的 static reset()）。
    pub fn reset() {
        let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(instance) = guard.take() {
            instance.changed.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> AwsAuthStatusManager {
        AwsAuthStatusManager::reset();
        AwsAuthStatusManager::get_instance()
    }

    #[test]
    fn lifecycle() {
        let manager = fresh();
        assert!(!manager.get_status().is_authenticating);

        manager.start_authentication();
        assert!(manager.get_status().is_authenticating);

        manager.add_output("line 1");
        manager.add_output("line 2");
        manager.set_error("boom");
        let status = manager.get_status();
        assert_eq!(
            status.output,
            vec!["line 1".to_string(), "line 2".to_string()]
        );
        assert_eq!(status.error.as_deref(), Some("boom"));

        // 失败：保留输出
        manager.end_authentication(false);
        let status = manager.get_status();
        assert!(!status.is_authenticating);
        assert_eq!(status.output.len(), 2);

        // 成功：完全清空
        manager.start_authentication();
        manager.end_authentication(true);
        let status = manager.get_status();
        assert!(!status.is_authenticating);
        assert!(status.output.is_empty());
        assert!(status.error.is_none());
    }

    #[test]
    fn emits_status_on_change() {
        let manager = fresh();
        let events: Arc<Mutex<Vec<AwsAuthStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let _sub = {
            let events = Arc::clone(&events);
            manager.subscribe(move |status| {
                events.lock().unwrap().push(status.clone());
            })
        };
        manager.start_authentication();
        manager.add_output("x");
        manager.end_authentication(true);
        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert!(recorded[0].is_authenticating);
        assert!(!recorded[2].is_authenticating);
    }
}
