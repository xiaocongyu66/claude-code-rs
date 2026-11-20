//! internal_writes.rs — 进程内 settings 文件写入的时间戳追踪。
//!
//! 重写自 `src/utils/settings/internalWrites.ts`。
//!
//! 追踪进程内对 settings 文件的写入时间戳，使 changeDetector.ts 的
//! chokidar 监视器能忽略自身写入的回声。该模块从 changeDetector.ts
//! 提取出来以打破 settings.ts → changeDetector.ts → hooks.ts → … →
//! settings.ts 的循环依赖：settings.ts 在写入前调用 mark 标记，
//! changeDetector 在 chokidar 触发时读取标记。二者只共享这个 map，
//! changeDetector 的其余部分（chokidar、hooks、mdm 轮询）与 settings.ts
//! 无关。零导入。
//!
//! 语义对齐：TS 的 `Map<string, number>` + `Date.now()` →
//! `Mutex<HashMap<String, u128>>` + epoch 毫秒；调用方传入已解析的路径
//! （path→source 的解析在 settings.ts 中完成）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static TIMESTAMPS: Mutex<Option<HashMap<String, u128>>> = Mutex::new(None);

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn with_map<R>(f: impl FnOnce(&mut HashMap<String, u128>) -> R) -> R {
    let mut guard = TIMESTAMPS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

pub fn mark_internal_write(path: &str) {
    with_map(|map| {
        map.insert(path.to_string(), now_millis());
    });
}

/// `path` 是否在 `window_ms` 内被标记过。匹配时消费该标记——监视器每次
/// 写入只触发一次，匹配过的标记不应继续抑制下一次（真实的、外部的）
/// 对同一文件的变更。
pub fn consume_internal_write(path: &str, window_ms: u64) -> bool {
    with_map(|map| {
        let now = now_millis();
        if let Some(&ts) = map.get(path) {
            if now.saturating_sub(ts) < u128::from(window_ms) {
                map.remove(path);
                return true;
            }
        }
        false
    })
}

pub fn clear_internal_writes() {
    with_map(|map| map.clear());
}
