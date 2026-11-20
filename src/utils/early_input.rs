//! early_input.rs — REPL 就绪之前的终端输入捕获。
//!
//! 重写自 `src/utils/earlyInput.ts`。用户常在敲下 `claude` 后立刻开始
//! 打字，本模块把 REPL 完全初始化之前的击键缓存下来，不丢失。
//!
//! 与 TS 的对应关系：
//! - `process_chunk` 为纯函数转换器，实现全部转义序列解析（CSI/DCS/OSC/
//!   SOS/PM、SS2/SS3、控制字符、回车→换行），语义逐行对齐
//! - `start/stop` 的 TTY raw-mode 操作属于 JS 运行时层，Rust 端由调用方
//!   负责把终端置为 raw 模式后逐块喂入 process_chunk（设计变更见下）
//! - Ctrl+C 仍走 process::exit(130)；Ctrl+D 停止捕获
//!
//! 设计差异（无 Node 流 API）：TS 用 stdin 的 'readable' 事件驱动；
//! Rust 端改为"全局缓冲 + 由调用方推送 chunk"的推模式。10 秒安全阀
//! 由调用方在 start 时自行设定。

use std::sync::{Mutex, OnceLock};

struct EarlyInputState {
    buffer: String,
    is_capturing: bool,
}

static STATE: OnceLock<Mutex<EarlyInputState>> = OnceLock::new();

fn state() -> &'static Mutex<EarlyInputState> {
    STATE.get_or_init(|| {
        Mutex::new(EarlyInputState {
            buffer: String::new(),
            is_capturing: false,
        })
    })
}

/// 处理一段输入数据：解析控制/转义序列并累积到缓冲区。
///
/// - Ctrl+C（0x03）：停止捕获并立即以 130 退出
/// - Ctrl+D（0x04）：停止捕获
/// - 退格（0x7F / 0x08）：删除最后一个 grapheme
/// - ESC 起头的转义序列：整体跳过（方向键、功能键、焦点事件等）
/// - 其它 < 0x20 的控制字符跳过（Tab/LF/CR 除外）；CR 转为 LF
/// - 可打印字符与允许的控制字符入缓冲区
pub fn process_chunk(chunk: &str) {
    let mut local_buffer = {
        let guard = state().lock().unwrap_or_else(|p| p.into_inner());
        guard.buffer.clone()
    };

    let chars: Vec<char> = chunk.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let char = chars[i];
        let code = char as u32;

        // Ctrl+C —— 立即退出（启动早期 gracefulShutdown 尚未就绪）
        if code == 3 {
            stop_capturing_early_input();
            std::process::exit(130);
        }

        // Ctrl+D —— EOF，停止捕获
        if code == 4 {
            stop_capturing_early_input();
            return;
        }

        // 退格 —— 删除最后一个 grapheme
        if code == 127 || code == 8 {
            if !local_buffer.is_empty() {
                let last = last_grapheme(&local_buffer);
                let drop_len = if last.is_empty() {
                    1
                } else {
                    last.chars().count()
                };
                local_buffer = local_buffer
                    .chars()
                    .take(local_buffer.chars().count() - drop_len)
                    .collect();
            }
            i += 1;
            continue;
        }

        // 跳过转义序列（方向键、功能键、焦点事件等）
        if code == 27 {
            i += 1; // 跳过 ESC
            if i >= chars.len() {
                continue;
            }
            let next = chars[i] as u32;

            // CSI 序列：ESC [ ... <final byte 0x40-0x7E>
            if next == 0x5b {
                i += 1; // 跳过 '['
                while i < chars.len() && (chars[i] as u32) >= 0x20 && (chars[i] as u32) <= 0x3f {
                    i += 1;
                }
                if i < chars.len() && (chars[i] as u32) >= 0x40 && (chars[i] as u32) <= 0x7e {
                    i += 1;
                }
                continue;
            }

            // 字符串序列：DCS (P)、OSC (])、SOS (X)、PM (^)
            // 以 BEL (0x07) 或 ST (ESC \) 结束
            if next == 0x50 || next == 0x5d || next == 0x58 || next == 0x5e {
                i += 1; // 跳过 introducer
                while i < chars.len() {
                    if (chars[i] as u32) == 0x07 {
                        i += 1;
                        break;
                    }
                    if (chars[i] as u32) == 0x1b
                        && i + 1 < chars.len()
                        && (chars[i + 1] as u32) == 0x5c
                    {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // SS2 (N)、SS3 (O) —— 两字节序列；其它简单序列 ESC <byte>
            if i < chars.len() {
                i += 1;
            }
            continue;
        }

        // 其它控制字符跳过（Tab/LF/CR 除外）
        if code < 32 && code != 9 && code != 10 && code != 13 {
            i += 1;
            continue;
        }

        // 回车转换行
        if code == 13 {
            local_buffer.push('\n');
            i += 1;
            continue;
        }

        local_buffer.push(char);
        i += 1;
    }

    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    guard.buffer = local_buffer;
}

/// 开始捕获（调用方应已确认 stdin 为 TTY 且非 -p/--print 模式）。
pub fn start_capturing_early_input() {
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_capturing {
        return;
    }
    guard.is_capturing = true;
    guard.buffer.clear();
}

/// 停止捕获（由 consume 自动调用，或手动调用）。
pub fn stop_capturing_early_input() {
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    guard.is_capturing = false;
}

/// 消费已捕获的输入（trim 后返回并清空）；自动停止捕获。
pub fn consume_early_input() -> String {
    stop_capturing_early_input();
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    let input = guard.buffer.trim().to_string();
    guard.buffer.clear();
    input
}

/// 不消费地检查是否已有输入。
pub fn has_early_input() -> bool {
    let guard = state().lock().unwrap_or_else(|p| p.into_inner());
    !guard.buffer.trim().is_empty()
}

/// 用预填文本播种缓冲区（出现在 REPL 输入框但不自动提交）。
pub fn seed_early_input(text: &str) {
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    guard.buffer = text.to_string();
}

/// 当前是否正在捕获。
pub fn is_capturing_early_input() -> bool {
    let guard = state().lock().unwrap_or_else(|p| p.into_inner());
    guard.is_capturing
}

fn last_grapheme(text: &str) -> &str {
    match text.char_indices().next_back() {
        Some((idx, _)) => &text[idx..],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_state() {
        let mut guard = state().lock().unwrap();
        guard.buffer.clear();
        guard.is_capturing = false;
    }

    #[test]
    fn accumulates_printable_input() {
        reset_state();
        process_chunk("hello");
        assert_eq!(consume_early_input(), "hello");
    }

    #[test]
    fn backspace_removes_last_grapheme() {
        reset_state();
        process_chunk("abc\u{7f}");
        assert_eq!(consume_early_input(), "ab");
    }

    #[test]
    fn escape_sequences_skipped() {
        reset_state();
        process_chunk("a\u{1b}[Db");
        assert_eq!(consume_early_input(), "ab");
        reset_state();
        // OSC 序列以 BEL 结束
        process_chunk("a\u{1b}]0;title\u{07}b");
        assert_eq!(consume_early_input(), "ab");
    }

    #[test]
    fn carriage_return_becomes_newline_and_controls_dropped() {
        reset_state();
        process_chunk("a\rb\u{1}c");
        assert_eq!(consume_early_input(), "a\nbc");
    }

    #[test]
    fn seed_and_has() {
        reset_state();
        seed_early_input("prefilled");
        assert!(has_early_input());
        assert_eq!(consume_early_input(), "prefilled");
        assert!(!has_early_input());
    }

    #[test]
    fn start_stop_capture_flag() {
        reset_state();
        start_capturing_early_input();
        assert!(is_capturing_early_input());
        stop_capturing_early_input();
        assert!(!is_capturing_early_input());
    }
}
