//! ndjson_framer.rs — NDJSON（换行分隔 JSON）帧解析状态机。
//!
//! 重写自 `src/utils/ndjsonFramer.ts`。语义对齐：
//!
//! - 累积到来的字节块、按 `\n` 切分、对完整行调用解析器产出消息；
//!   格式错误的行默认跳过，调用方可选配置错误回调 / 销毁回调。
//! - 帧大小上限 `max_frame_bytes`：缓冲字节数 + 当前段字节数超过上限
//!   时判定为 oversized frame（对齐 TS 中 bufferBytes + segmentBytes >
//!   maxFrameBytes 的窗口内判定）。
//!
//! 偏差：TS 版直接挂在 Node `net.Socket` 的 'data' 事件上；Rust 侧
//! 落成纯状态机 `NdjsonFramer`，由调用方驱动 `feed(chunk)`（底层读取
//! 是 UdsMessaging / pipeTransport 等上层模块的事，避免在 utils 层
//! 绑定具体 IO 抽象）。

use serde_json::Value;

/// 帧错误（对齐 TS 的 Error 分流：oversized 与 invalid）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// 帧超过 max_frame_bytes 上限（`max_frame_bytes` 为 usize::MAX 时
    /// 永远不会产生——对齐 TS 的 Number.POSITIVE_INFINITY 默认值）。
    Oversized { max_frame_bytes: usize, bytes: usize },
    /// 单行 JSON 解析失败（或自定义解析器抛错）。
    Invalid(String),
}

/// NDJSON 分帧器配置（对齐 TS NdjsonFramerOptions 的判定部分）。
#[derive(Debug, Clone, Copy)]
pub struct NdjsonFramerOptions {
    /// 帧字节数上限；None 表示无上限。
    pub max_frame_bytes: Option<usize>,
}

impl Default for NdjsonFramerOptions {
    fn default() -> Self {
        Self {
            max_frame_bytes: None,
        }
    }
}

/// 解析结果：每 `feed` 一次返回本批次解析出的消息与首个错误（若有）。
#[derive(Debug, Default)]
pub struct FeedResult {
    pub messages: Vec<Value>,
    /// 本批次首个帧错误（ oversized / invalid）。无论配置如何，
    /// 解析都继续处理已就绪的完整行（对齐 TS 中 oversized 后直接 return
    /// 的行为：返回错误且本批次不再产出后续消息）。
    pub error: Option<FrameError>,
}

pub struct NdjsonFramer {
    buffer: String,
    buffer_bytes: usize,
    max_frame_bytes: usize,
}

impl NdjsonFramer {
    pub fn new(options: NdjsonFramerOptions) -> Self {
        Self {
            buffer: String::new(),
            buffer_bytes: 0,
            max_frame_bytes: options.max_frame_bytes.unwrap_or(usize::MAX),
        }
    }

    /// 喂入一个字节块，产出本批次解析出的消息与可能的首个帧错误。
    ///
    /// 对齐 TS 行为：
    /// - 当 buffer_bytes + 段字节数 > max_frame_bytes 时，返回 Oversized
    ///   错误并不再处理本块后续内容。
    /// - 每个完整行尝试解析；失败则产生 Invalid 错误，但继续处理后续行。
    /// - 空行（trim 后为空）跳过。
    pub fn feed(&mut self, chunk: &[u8]) -> FeedResult {
        let mut result = FeedResult::default();
        let max = self.max_frame_bytes;

        let mut start = 0usize;
        let mut idx = 0usize;
        while idx < chunk.len() {
            if chunk[idx] != 0x0a {
                idx += 1;
                continue;
            }

            let segment_bytes = idx - start;
            if max != usize::MAX && self.buffer_bytes + segment_bytes > max {
                result.error = Some(FrameError::Oversized {
                    max_frame_bytes: max,
                    bytes: self.buffer_bytes + segment_bytes,
                });
                return result;
            }

            self.buffer
                .push_str(&String::from_utf8_lossy(&chunk[start..idx]));
            self.emit_line(&mut result);
            self.buffer.clear();
            self.buffer_bytes = 0;
            start = idx + 1;
            idx += 1;
        }

        let tail_bytes = chunk.len() - start;
        if max != usize::MAX && self.buffer_bytes + tail_bytes > max {
            result.error = Some(FrameError::Oversized {
                max_frame_bytes: max,
                bytes: self.buffer_bytes + tail_bytes,
            });
            return result;
        }
        if tail_bytes > 0 {
            self.buffer
                .push_str(&String::from_utf8_lossy(&chunk[start..]));
            self.buffer_bytes += tail_bytes;
        }

        result
    }

    fn emit_line(&self, result: &mut FeedResult) {
        let line = self.buffer.trim();
        if line.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => result.messages.push(value),
            Err(e) => {
                if result.error.is_none() {
                    result.error = Some(FrameError::Invalid(e.to_string()));
                }
            }
        }
    }

    /// 重置内部缓冲（供连接重用）。
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.buffer_bytes = 0;
    }
}
