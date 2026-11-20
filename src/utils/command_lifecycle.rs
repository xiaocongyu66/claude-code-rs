//! command_lifecycle.rs — 命令生命周期监听器登记。
//!
//! 重写自 `src/utils/commandLifecycle.ts`。全局单例监听器，供
//! BashTool 等命令在 started/completed 时通知 UI。

use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLifecycleState {
    Started,
    Completed,
}

type Listener = Box<dyn Fn(&str, CommandLifecycleState) + Send + Sync>;

struct Registry {
    listener: Option<Listener>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry { listener: None }))
}

/// 安装（或传 None 卸载）生命周期监听器。
pub fn set_command_lifecycle_listener<F>(callback: Option<F>)
where
    F: Fn(&str, CommandLifecycleState) + Send + Sync + 'static,
{
    let mut guard = registry().lock().unwrap_or_else(|p| p.into_inner());
    guard.listener = callback.map(|f| Box::new(f) as Listener);
}

/// 通知监听器某命令的状态变化；未安装监听器时为 no-op。
pub fn notify_command_lifecycle(uuid: &str, state: CommandLifecycleState) {
    let guard = registry().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(listener) = guard.listener.as_ref() {
        listener(uuid, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn listener_receives_events() {
        let events: Arc<Mutex<Vec<(String, CommandLifecycleState)>>> =
            Arc::new(Mutex::new(Vec::new()));
        {
            let events = Arc::clone(&events);
            set_command_lifecycle_listener(Some(move |uuid: &str, state: CommandLifecycleState| {
                events.lock().unwrap().push((uuid.to_string(), state));
            }));
        }
        notify_command_lifecycle("id-1", CommandLifecycleState::Started);
        notify_command_lifecycle("id-1", CommandLifecycleState::Completed);
        let recorded = events.lock().unwrap();
        assert_eq!(
            *recorded,
            vec![
                ("id-1".to_string(), CommandLifecycleState::Started),
                ("id-1".to_string(), CommandLifecycleState::Completed)
            ]
        );
        // 卸载，避免污染其它用例
        set_command_lifecycle_listener(
            None as Option<Box<dyn Fn(&str, CommandLifecycleState) + Send + Sync>>,
        );
    }
}
