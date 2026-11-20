//! read_edit_context.rs — 在文件中查找 needle 并返回上下文切片。
//!
//! 重写自 `src/utils/readEditContext.ts`。语义对齐：
//!
//! - 以 8KB 块扫描文件找 `needle`，块间带 straddle overlap，
//!   匹配跨块边界的 needle；总扫描上限 MAX_SCAN_BYTES（10MB）。
//! - 返回 `EditContext { content, line_offset, truncated }`：
//!   匹配前后各 context_lines 行（行边界对齐）的切片 + 起始行号。
//! - 模型发 LF；文件可能是 CRLF。LF 扫描未命中时构造 CRLF needle 再扫
//!   （overlap 按 LF 个数 x2 预留）。
//! - ENOENT 返回 None；超过 MAX_SCAN_BYTES 未找到返回 truncated=true。
//! - `read_capped`：读整个文件（上限 MAX_SCAN_BYTES），超过返回 None。
//!
//! 偏差：TS 的 fs/promises FileHandle 换成 `tokio::fs::File`（AsyncReadExt
//! + AsyncSeekExt）；Node read 的 position 参数用 seek 实现。

use std::io::SeekFrom;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::utils::errors::is_enoent;

pub const CHUNK_SIZE: usize = 8 * 1024;
pub const MAX_SCAN_BYTES: usize = 10 * 1024 * 1024;
const NL: u8 = 0x0a;

/// 找到 needle 后的上下文切片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditContext {
    /// 文件切片：匹配前后各 context_lines 行，行边界对齐。
    pub content: String,
    /// content 第一行在原文件中的 1-based 行号。
    pub line_offset: usize,
    /// MAX_SCAN_BYTES 扫完仍未找到 needle 时为 true。
    pub truncated: bool,
}

/// 在 `path` 文件中查找 `needle`，返回带 `context_lines` 行上下文的切片。
///
/// ENOENT → Ok(None)。needle 在 MAX_SCAN_BYTES 内未找到 →
/// `Ok({ truncated: true, content: "" })`。
/// 其他打开错误原样上抛（对齐 TS 的 `throw e`）。
pub async fn read_edit_context(
    path: &str,
    needle: &str,
    context_lines: usize,
) -> std::io::Result<Option<EditContext>> {
    let mut handle = match open_for_scan(path).await {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(None),
        Err(e) => return Err(e),
    };
    // 句柄在函数结束时 drop（对齐 TS finally 里的 close）
    Ok(scan_for_context(&mut handle, needle, context_lines).await)
}

/// 以扫描用途打开文件。ENOENT → Ok(None)；其余错误上抛。
pub async fn open_for_scan(path: &str) -> std::io::Result<Option<File>> {
    match File::open(path).await {
        Ok(file) => Ok(Some(file)),
        Err(e) if is_enoent(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 接受已打开句柄的核心扫描函数（调用方负责 open/close）。
pub async fn scan_for_context(
    handle: &mut File,
    needle: &str,
    context_lines: usize,
) -> EditContext {
    if needle.is_empty() {
        return EditContext {
            content: String::new(),
            line_offset: 1,
            truncated: false,
        };
    }
    let needle_lf = needle.as_bytes().to_vec();
    // 模型发 LF；文件可能是 CRLF。数换行行数为 CRLF 形态预留 overlap，
    // CRLF buffer 延迟到 LF 扫描未命中才构造。
    let nl_count = needle_lf.iter().filter(|&&b| b == NL).count();
    let needle_crlf: Option<Vec<u8>> = if nl_count > 0 {
        let crlf: String = needle.replace('\n', "\r\n");
        Some(crlf.as_bytes().to_vec())
    } else {
        None
    };
    let overlap = needle_lf.len() + nl_count - 1;

    let mut buf = vec![0u8; CHUNK_SIZE + overlap];
    let mut pos: usize = 0;
    let mut lines_before_pos: usize = 0;
    let mut prev_tail: usize = 0;

    while pos < MAX_SCAN_BYTES {
        let bytes_read = match read_at(handle, &mut buf[prev_tail..], pos as u64).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if bytes_read == 0 {
            break;
        }
        let view_len = prev_tail + bytes_read;

        let mut match_at = index_of_within(&buf[..view_len], &needle_lf);
        let mut match_len = needle_lf.len();
        if match_at.is_none() {
            if let Some(needle_crlf) = &needle_crlf {
                if let Some(idx) = index_of_within(&buf[..view_len], needle_crlf) {
                    match_at = Some(idx);
                    match_len = needle_crlf.len();
                }
            }
        }
        if let Some(match_at) = match_at {
            let abs_match = pos - prev_tail + match_at;
            let lines_to_match = lines_before_pos + count_newlines(&buf, 0, match_at);
            return slice_context(
                handle,
                &mut buf,
                abs_match,
                match_len,
                context_lines,
                lines_to_match,
            )
            .await;
        }
        pos += bytes_read;
        // 把尾部挪到前面做 straddle；lines_before_pos 只数**将被丢弃**的字节里的换行。
        let next_tail = overlap.min(view_len);
        lines_before_pos += count_newlines(&buf, 0, view_len - next_tail);
        prev_tail = next_tail;
        buf.copy_within(view_len - prev_tail..view_len, 0);
    }

    EditContext {
        content: String::new(),
        line_offset: 1,
        truncated: pos >= MAX_SCAN_BYTES,
    }
}

/// 经 handle 读整个文件，上限 MAX_SCAN_BYTES；超过返回 None。
///
/// FileEditToolDiff 的多编辑路径（连续替换需要完整字符串）使用。
pub async fn read_capped(handle: &mut File) -> Option<String> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total = 0usize;
    loop {
        if total == buf.len() {
            let new_len = (buf.len() * 2).min(MAX_SCAN_BYTES + CHUNK_SIZE);
            buf.resize(new_len, 0);
        }
        match handle.read(&mut buf[total..]).await {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if total > MAX_SCAN_BYTES {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    Some(normalize_crlf(&buf[..total]))
}

fn index_of_within(buf: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || buf.len() < needle.len() {
        return None;
    }
    buf.windows(needle.len())
        .position(|window| window == needle)
}

fn count_newlines(buf: &[u8], start: usize, end: usize) -> usize {
    buf[start..end].iter().filter(|&&b| b == NL).count()
}

/// 解码 buf 到 utf8，含 CR 时把 CRLF 归一为 LF。
fn normalize_crlf(buf: &[u8]) -> String {
    let s = String::from_utf8_lossy(buf).into_owned();
    if s.contains('\r') {
        s.replace("\r\n", "\n")
    } else {
        s
    }
}

/// 从 position 处读到 buf（返回实际读取字节数）。seek 由 tokio 完成。
async fn read_at(handle: &mut File, buf: &mut [u8], position: u64) -> std::io::Result<usize> {
    handle.seek(SeekFrom::Start(position)).await?;
    handle.read(buf).await
}

/// 给定绝对匹配偏移，读 ±contextLines 并返回解码切片与起始行号。
/// 复用 `scratch`（调用方的扫描缓冲）做前/后/输出读取。
async fn slice_context(
    handle: &mut File,
    scratch: &mut [u8],
    match_start: usize,
    match_len: usize,
    context_lines: usize,
    lines_before_match: usize,
) -> EditContext {
    // 从 match_start 往前找 contextLines 个换行
    let back_chunk = match_start.min(CHUNK_SIZE);
    let back_read = match read_at(handle, &mut scratch[..back_chunk], (match_start - back_chunk) as u64).await {
        Ok(n) => n,
        Err(_) => return EditContext { content: String::new(), line_offset: 1, truncated: false },
    };
    let mut ctx_start = match_start;
    let mut nl_seen = 0usize;
    let mut i = back_read;
    while i > 0 && nl_seen <= context_lines {
        i -= 1;
        if scratch[i] == NL {
            nl_seen += 1;
            if nl_seen > context_lines {
                break;
            }
        }
        ctx_start -= 1;
    }
    // 在 scratch 被前向读取覆盖前算出 lineOffset
    let walked_back = match_start - ctx_start;
    let line_offset = lines_before_match
        - count_newlines(scratch, back_read - walked_back, back_read)
        + 1;

    // 从 match_end 往后找 contextLines 个换行
    let match_end = match_start + match_len;
    let fwd_read = match read_at(handle, &mut scratch[..CHUNK_SIZE], match_end as u64).await {
        Ok(n) => n,
        Err(_) => return EditContext { content: String::new(), line_offset, truncated: false },
    };
    let mut ctx_end = match_end;
    nl_seen = 0;
    for i in 0..fwd_read {
        ctx_end += 1;
        if scratch[i] == NL {
            nl_seen += 1;
            if nl_seen >= context_lines + 1 {
                break;
            }
        }
    }

    // 读出精确的上下文区间。偏差：TS 在 len <= scratch.length 时复用
    // scratch 输出缓冲；Rust 侧 borrow 检查不允许复用借用中的缓冲，
    // 这里另分配一次（上下文区间通常远小于块大小，开销可忽略）。
    let len = ctx_end - ctx_start;
    let mut out = vec![0u8; len];
    let out_read = match read_at(handle, &mut out, ctx_start as u64).await {
        Ok(n) => n,
        Err(_) => return EditContext { content: String::new(), line_offset, truncated: false },
    };
    EditContext {
        content: normalize_crlf(&out[..out_read]),
        line_offset,
        truncated: false,
    }
}
