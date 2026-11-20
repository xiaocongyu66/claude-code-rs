//! change_detector.rs — settings 文件变更检测与通知。
//!
//! 重写自 `src/utils/settings/changeDetector.ts`。
//!
//! 已知差异（依赖尚未转 Rust，待下一波消除）：
//! - TS 用 chokidar（inotify/FSEvents + awaitWriteFinish）；Rust 无文件
//!   监视 crate 依赖，改用 **轮询线程**：每 500ms stat 目标文件，mtime
//!   稳定超过 `FILE_STABILITY_THRESHOLD_MS`（1000ms）才确认为变更；文件
//!   消失后 `DELETION_GRACE_MS`（1700ms）内重建则取消删除通知（对齐
//!   TS 的 delete-and-recreate 容忍）。监视目标只包含「init 时目录中已
//!   存在任一 settings 文件」的候选路径（对齐 TS 只 watch 这些目录）；
//! - ConfigChange hook 执行（utils/hooks.ts 未转换）暂以「放行」占位
//!   （`execute_config_change_hooks` 返回 true）。hooks 模块转换后需
//!   接入真实的阻塞判定（exit code 2 / decision block）；
//! - MDM 轮询与 fanOut 语义一致：快照变化 → 更新 mdm 缓存 → 重置
//!   settings 缓存 → 发出 policySettings 通知（单一生产者重置缓存，
//!   避免 N 个监听者 N 次磁盘重载）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;

use crate::bootstrap::state::{get_is_remote_mode, SettingSource};
use crate::utils::cleanup_registry::register_cleanup;
use crate::utils::debug::{log_for_debugging, DebugLogLevel};
use crate::utils::settings::constants::{setting_source_as_str, SETTING_SOURCES};
use crate::utils::settings::internal_writes::{clear_internal_writes, consume_internal_write};
use crate::utils::settings::managed_path::get_managed_settings_drop_in_dir;
use crate::utils::settings::mdm::settings::{get_hkcu_settings, get_mdm_settings, refresh_mdm_settings, set_mdm_settings_cache};
use crate::utils::settings::settings::get_settings_file_path_for_source;
use crate::utils::settings::settings_cache::reset_settings_cache;
use crate::utils::signal::{Signal, Subscription};
use crate::utils::slow_operations::json_stringify;

/// 写入稳定窗口：mtime 变化后至少等待 1000ms 才确认为变更。
const FILE_STABILITY_THRESHOLD_MS: u64 = 1000;
/// 稳定性轮询间隔（必须小于 FILE_STABILITY_THRESHOLD_MS）。
const FILE_STABILITY_POLL_INTERVAL_MS: u64 = 500;
/// 内部写入抑制窗口：markInternalWrite 后 5s 内的变更视为自身写入。
const INTERNAL_WRITE_WINDOW_MS: u64 = 5000;
/// MDM（注册表/plist）轮询间隔：30 分钟（无文件系统事件可监视）。
const MDM_POLL_INTERVAL_MS: u64 = 30 * 60 * 1000;
/// 删除宽容期：删除后 1700ms 内重建则取消删除通知（auto-updater /
/// 其它会话的 delete-and-recreate 模式）。必须大于 chokidar 的
/// awaitWriteFinish 延迟（本实现为 stability+poll 之和）。
const DELETION_GRACE_MS: u64 = FILE_STABILITY_THRESHOLD_MS + FILE_STABILITY_POLL_INTERVAL_MS + 200;

/// 测试用 timing 覆盖（对齐 TS 的 testOverrides）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TimingOverrides {
    pub stability_threshold_ms: Option<u64>,
    pub poll_interval_ms: Option<u64>,
    pub mdm_poll_interval_ms: Option<u64>,
    pub deletion_grace_ms: Option<u64>,
}

struct DetectorState {
    initialized: bool,
    disposed: bool,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    timing: TimingOverrides,
}

static DETECTOR: Mutex<Option<DetectorState>> = Mutex::new(None);

static SETTINGS_CHANGED: Lazy<Signal<SettingSource>> = Lazy::new(Signal::default);

/// 订阅 settings 变更（返回的 Subscription drop 时取消订阅）。
pub fn subscribe<F>(listener: F) -> Subscription<SettingSource>
where
    F: FnMut(&SettingSource) + Send + Sync + 'static,
{
    SETTINGS_CHANGED.subscribe(listener)
}

/// ConfigChange hook 执行（hooks.ts 未转换 —— 占位实现，始终放行）。
/// hooks 转换后应返回「是否存在阻断结果（exit code 2 / decision block）」。
fn execute_config_change_hooks(source: &str, _path: &str) -> bool {
    log_for_debugging(
        &format!("ConfigChange hook execution stubbed (hooks.rs pending) for source {source}"),
        DebugLogLevel::Debug,
    );
    true
}

fn setting_source_to_config_change_source(source: SettingSource) -> &'static str {
    match source {
        SettingSource::UserSettings => "user_settings",
        SettingSource::ProjectSettings => "project_settings",
        SettingSource::LocalSettings => "local_settings",
        SettingSource::FlagSettings | SettingSource::PolicySettings => "policy_settings",
    }
}

/// 重置 settings 缓存并通知所有监听器（单一生产者重置缓存——见模块头）。
fn fan_out(source: SettingSource) {
    reset_settings_cache();
    SETTINGS_CHANGED.emit(&source);
}

/// 手动通知 settings 变更（程序性变更，如远程托管 settings 刷新）。
pub fn notify_change(source: SettingSource) {
    log_for_debugging(
        &format!(
            "Programmatic settings change notification for {}",
            setting_source_as_str(source)
        ),
        DebugLogLevel::Debug,
    );
    fan_out(source);
}

/// 收集要监视的 settings 文件路径（跳过 flagSettings——CLI 提供，会话内
/// 不变；且可能在 $TMPDIR 含 FIFO/socket 使监视器出错）。只保留「目录中
/// 至少存在一个 settings 文件」的候选路径。
fn get_watch_targets() -> Vec<(String, SettingSource)> {
    // 目录 → 该目录下所有候选 settings 文件
    let mut dir_to_settings_files: HashMap<String, Vec<(String, SettingSource)>> =
        HashMap::new();
    let mut dirs_with_existing_files: Vec<String> = Vec::new();

    for source in SETTING_SOURCES.iter().copied() {
        if source == SettingSource::FlagSettings {
            continue;
        }
        let Some(path) = get_settings_file_path_for_source(source) else {
            continue;
        };
        let dir = Path::new(&path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if dir.is_empty() {
            continue;
        }
        dir_to_settings_files
            .entry(dir.clone())
            .or_default()
            .push((path.clone(), source));
        if std::fs::metadata(&path)
            .ok()
            .map(|m| m.is_file())
            .unwrap_or(false)
            && !dirs_with_existing_files.contains(&dir)
        {
            dirs_with_existing_files.push(dir);
        }
    }

    let mut targets = Vec::new();
    for dir in dirs_with_existing_files {
        if let Some(files) = dir_to_settings_files.get(&dir) {
            targets.extend(files.iter().cloned());
        }
    }

    // managed-settings.d/ drop-in 目录：内部任何 .json 变化都属于 policySettings
    let drop_in_dir = get_managed_settings_drop_in_dir();
    if std::fs::metadata(drop_in_dir)
        .ok()
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        if let Ok(entries) = std::fs::read_dir(drop_in_dir) {
            for entry in entries.flatten() {
                let ft = entry.file_type();
                let is_file_or_link = ft
                    .as_ref()
                    .map(|t| t.is_file() || t.is_symlink())
                    .unwrap_or(false);
                let name = entry.file_name().to_string_lossy().to_string();
                if is_file_or_link && name.ends_with(".json") && !name.starts_with('.') {
                    targets.push((
                        entry.path().to_string_lossy().to_string(),
                        SettingSource::PolicySettings,
                    ));
                }
            }
        }
    }

    targets
}

/// 初始化 settings 变更监视（MDM 轮询 + 文件轮询线程）。远程模式下跳过。
pub fn initialize() {
    if get_is_remote_mode() {
        return;
    }
    let mut guard = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert(DetectorState {
        initialized: false,
        disposed: false,
        stop: Arc::new(AtomicBool::new(false)),
        handles: Vec::new(),
        timing: TimingOverrides::default(),
    });
    if state.initialized || state.disposed {
        return;
    }
    state.initialized = true;

    // MDM 轮询（注册表/plist 变更）
    start_mdm_poll(state);

    // 优雅退出时清理
    let stop = Arc::clone(&state.stop);
    register_cleanup(move || {
        let _ = &stop;
        dispose();
    });

    let targets = get_watch_targets();
    if targets.is_empty() {
        return;
    }

    log_for_debugging(
        &format!(
            "Watching for changes in setting files {}...",
            targets
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DebugLogLevel::Debug,
    );

    let stop = Arc::clone(&state.stop);
    let timing = state.timing;
    state.handles.push(thread::spawn(move || {
        watcher_loop(targets, stop, timing);
    }));
}

fn watcher_loop(targets: Vec<(String, SettingSource)>, stop: Arc<AtomicBool>, timing: TimingOverrides) {
    let poll_interval = Duration::from_millis(
        timing.poll_interval_ms.unwrap_or(FILE_STABILITY_POLL_INTERVAL_MS),
    );
    let stability = Duration::from_millis(
        timing.stability_threshold_ms.unwrap_or(FILE_STABILITY_THRESHOLD_MS),
    );
    let grace = Duration::from_millis(timing.deletion_grace_ms.unwrap_or(DELETION_GRACE_MS));

    // path → 上次记录的 mtime（epoch 毫秒）
    let mut mtimes: HashMap<String, u128> = HashMap::new();
    // path → 开始缺失的时刻（删除宽容期）
    let mut missing_since: HashMap<String, Instant> = HashMap::new();

    // 初始状态：仅记录已存在文件的 mtime（ignoreInitial 语义）
    for (path, _) in &targets {
        if let Some(mtime) = file_mtime(path) {
            mtimes.insert(path.to_string(), mtime);
        }
    }

    while !stop.load(Ordering::SeqCst) {
        thread::sleep(poll_interval);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = Instant::now();

        for (path, source) in &targets {
            if let Some(mtime) = file_mtime(path) {
                missing_since.remove(path);
                match mtimes.get(path).copied() {
                    Some(prev) if prev == mtime => continue,
                    Some(_) => {
                        // mtime 变化：等写入稳定后确认为变更（awaitWriteFinish 近似）
                        let stable = match system_time_from_millis(mtime) {
                            Some(sys) => SystemTime::now().duration_since(sys).unwrap_or_default() >= stability,
                            None => true,
                        };
                        if stable {
                            mtimes.insert(path.to_string(), mtime);
                            handle_change(path, *source);
                        }
                    }
                    None => {
                        // init 后新出现的文件（chokidar 'add' → handleChange）
                        mtimes.insert(path.to_string(), mtime);
                        handle_change(path, *source);
                    }
                }
            } else if mtimes.contains_key(path) {
                // 文件消失（之前存在）：删除宽容期，重建则取消
                let fire = match missing_since.get(path) {
                    Some(&since) => now.duration_since(since) >= grace,
                    None => {
                        missing_since.insert(path.to_string(), now);
                        false
                    }
                };
                if fire {
                    missing_since.remove(path);
                    mtimes.remove(path);
                    handle_delete(path, *source);
                }
            }
        }
    }
}

fn file_mtime(path: &str) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let sys = meta.modified().ok()?;
    Some(sys.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis())
}

fn system_time_from_millis(ms: u128) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))
}

fn handle_change(path: &str, source: SettingSource) {
    // 检查是否为进程内部写入（5 秒窗口）
    if consume_internal_write(path, INTERNAL_WRITE_WINDOW_MS) {
        return;
    }

    log_for_debugging(
        &format!("Detected change to {path}"),
        DebugLogLevel::Debug,
    );

    // ConfigChange hook（占位：始终放行）
    if !execute_config_change_hooks(setting_source_to_config_change_source(source), path) {
        log_for_debugging(
            &format!("ConfigChange hook blocked change to {path}"),
            DebugLogLevel::Debug,
        );
        return;
    }
    fan_out(source);
}

fn handle_delete(path: &str, source: SettingSource) {
    log_for_debugging(
        &format!("Detected deletion of {path}"),
        DebugLogLevel::Debug,
    );

    if !execute_config_change_hooks(setting_source_to_config_change_source(source), path) {
        log_for_debugging(
            &format!("ConfigChange hook blocked deletion of {path}"),
            DebugLogLevel::Debug,
        );
        return;
    }
    fan_out(source);
}

/// 停止全部监视（MDM 轮询 + 文件轮询），阻塞至线程退出（最多 2s）。
pub fn dispose() {
    let mut guard = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return;
    };
    state.disposed = true;
    state.stop.store(true, Ordering::SeqCst);
    let handles: Vec<JoinHandle<()>> = std::mem::take(&mut state.handles);
    drop(guard);

    for handle in handles {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    let mut guard = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = guard.as_mut() {
        state.initialized = false;
    }
    clear_internal_writes();
    SETTINGS_CHANGED.clear();
}

/// 启动 MDM 轮询线程（首次快照 + 周期比对）。
fn start_mdm_poll(state: &mut DetectorState) {
    // 初始快照（含 admin MDM 与用户可写 HKCU）
    let initial = get_mdm_settings();
    let initial_hkcu = get_hkcu_settings();
    let initial_snapshot = format!(
        "{{\"mdm\":{},\"hkcu\":{}}}",
        json_stringify(&serde_json::to_value(&initial.settings).unwrap_or_default()),
        json_stringify(&serde_json::to_value(&initial_hkcu.settings).unwrap_or_default()),
    );

    let stop = Arc::clone(&state.stop);
    let interval = state.timing.mdm_poll_interval_ms.unwrap_or(MDM_POLL_INTERVAL_MS);
    state
        .handles
        .push(thread::spawn(move || {
            let mut last_snapshot = initial_snapshot;
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(interval));
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let (current, current_hkcu) = refresh_mdm_settings();
                let snapshot = format!(
                    "{{\"mdm\":{},\"hkcu\":{}}}",
                    json_stringify(&serde_json::to_value(&current.settings).unwrap_or_default()),
                    json_stringify(&serde_json::to_value(&current_hkcu.settings).unwrap_or_default()),
                );
                if snapshot != last_snapshot {
                    last_snapshot = snapshot;
                    // 更新缓存，使同步读取者拿到新值
                    set_mdm_settings_cache(current, current_hkcu);
                    log_for_debugging(
                        "Detected MDM settings change via poll",
                        DebugLogLevel::Debug,
                    );
                    fan_out(SettingSource::PolicySettings);
                }
            }
        }));
}

/// 重置内部状态（仅测试使用；可传 timing 覆盖加速）。会停止现有线程。
pub fn reset_for_testing(overrides: Option<TimingOverrides>) {
    if let Some(state) = DETECTOR.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        state.stop.store(true, Ordering::SeqCst);
    }
    let handles: Vec<JoinHandle<()>> = DETECTOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .map(|s| std::mem::take(&mut s.handles))
        .unwrap_or_default();
    for handle in handles {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    let mut guard = DETECTOR.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(DetectorState {
        initialized: false,
        disposed: false,
        stop: Arc::new(AtomicBool::new(false)),
        handles: Vec::new(),
        timing: overrides.unwrap_or_default(),
    });
    clear_internal_writes();
}

/// 对外门面对象（对齐 TS 的 `settingsChangeDetector`）。
pub struct SettingsChangeDetector;

impl SettingsChangeDetector {
    pub fn initialize() {
        initialize();
    }

    pub fn dispose() {
        dispose();
    }

    pub fn subscribe<F>(listener: F) -> Subscription<SettingSource>
    where
        F: FnMut(&SettingSource) + Send + Sync + 'static,
    {
        subscribe(listener)
    }

    pub fn notify_change(source: SettingSource) {
        notify_change(source);
    }

    pub fn reset_for_testing(overrides: Option<TimingOverrides>) {
        reset_for_testing(overrides);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn notify_change_emits_to_subscribers() {
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        let _sub = subscribe(move |_source| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });
        notify_change(SettingSource::UserSettings);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn initialize_is_idempotent_and_remote_mode_safe() {
        // 非远程环境下初始化两次不应 panic（可能无目标文件 → 直接返回）
        initialize();
        initialize();
        dispose();
    }

    #[test]
    fn flag_settings_excluded_from_watch_targets() {
        let targets = get_watch_targets();
        // flagSettings 永不出现在目标列表中
        assert!(targets.iter().all(|(_, s)| *s != SettingSource::FlagSettings));
    }
}
