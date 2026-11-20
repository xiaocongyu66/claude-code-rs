//! heredoc.rs — heredoc 提取与还原工具。
//!
//! 重写自 `src/utils/bash/heredoc.ts`。shell-quote 库把 `<<` 解析成两个 `<`
//! 重定向算符，会破坏 heredoc 语法的命令拆分；本模块在解析前先把 heredoc
//! 抽出替换为占位符，解析后再还原。
//!
//! 支持形式：<<WORD、<<'WORD'、<<"WORD"、<<-WORD、<<-'WORD'。
//!
//! 已知差异：
//! - TS 端 `HEREDOC_START_PATTERN` 使用 lookbehind 正则；Rust 的 `regex`
//!   crate 不支持 lookbehind，改为等价的手工扫描器（`next_heredoc_start`）
//!   逐位置匹配，语义与原正则一致。
//! - 占位符盐用 `rand::thread_rng`（ChaCha）生成 8 字节随机数；TS 端用
//!   `crypto.randomBytes`。两者都足够唯一，安全等级相当。
//! - TS 的 start/end 索引是 JS 字符串下标（UTF-16 码元）；Rust 下为 UTF-8
//!   字节下标。对纯 ASCII 输入完全一致；多字节字符在 heredoc 体前的命令
//!   段中会导致下标差异，但提取/替换始终基于同一坐标体系，结果仍自洽。

use std::collections::HashMap;

use rand::Rng;

const HEREDOC_PLACEHOLDER_PREFIX: &str = "__HEREDOC_";
const HEREDOC_PLACEHOLDER_SUFFIX: &str = "__";

/// 生成 8 随机字节的十六进制串（16 字符），防止占位符与命令中的字面
/// `__HEREDOC_N__` 文本碰撞。
fn generate_placeholder_salt() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 8] = rng.gen();
    let mut s = String::with_capacity(16);
    for b in &bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// heredoc 起始的手工扫描结果。等价于 TS 的正则
/// `/(?<!<)<<(?!<)(-)?[ \t]*(?:(['"])(\\?\w+)\2|\\?(\w+))/` 的一次匹配。
struct HeredocStart {
    /// `<<` 的起始字节偏移
    index: usize,
    /// 整个匹配的结束字节偏移（等价于 lastIndex 推进位置）
    end: usize,
    is_dash: bool,
    /// 引号字符（quoted 定界符时存在）
    quote: Option<char>,
    /// 定界符（不含引号；quoted 形式可能含前导反斜杠）
    delim: String,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 从 `from` 开始查找下一个 heredoc 起始。语义与 TS 的
/// `/(?<!<)<<(?!<)(-)?[ \t]*(?:(['"])(\\?\w+)\2|\\?(\w+))/g` + exec 一致。
fn next_heredoc_start(cmd: &str, from: usize) -> Option<HeredocStart> {
    let b = cmd.as_bytes();
    let mut i = from;
    while i + 1 < b.len() {
        if b[i] == b'<' && b[i + 1] == b'<' {
            let prev_ok = i == 0 || b[i - 1] != b'<';
            let next_ok = i + 2 >= b.len() || b[i + 2] != b'<';
            if prev_ok && next_ok {
                let mut p = i + 2;
                let is_dash = p < b.len() && b[p] == b'-';
                if is_dash {
                    p += 1;
                }
                while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                    p += 1;
                }
                // 引号替代项：( ['"] (\\?\w+) \2 )
                if p < b.len() && (b[p] == b'"' || b[p] == b'\'') {
                    let q = b[p];
                    let mut q2 = p + 1;
                    if q2 < b.len() && b[q2] == b'\\' {
                        q2 += 1;
                    }
                    let wstart = q2;
                    while q2 < b.len() && is_word_byte(b[q2]) {
                        q2 += 1;
                    }
                    if q2 > wstart && q2 < b.len() && b[q2] == q {
                        let delim = cmd[wstart..q2].to_string();
                        return Some(HeredocStart {
                            index: i,
                            end: q2 + 1,
                            is_dash,
                            quote: Some(q as char),
                            delim,
                        });
                    }
                }
                // 非引号替代项：\\?(\w+) —— 反斜杠在捕获组外
                let mut u = p;
                if u < b.len() && b[u] == b'\\' {
                    u += 1;
                }
                let wstart = u;
                while u < b.len() && is_word_byte(b[u]) {
                    u += 1;
                }
                if u > wstart {
                    let delim = cmd[wstart..u].to_string();
                    return Some(HeredocStart {
                        index: i,
                        end: u,
                        is_dash,
                        quote: None,
                        delim,
                    });
                }
            }
        }
        i += 1;
    }
    None
}

/// heredoc 提取结果条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeredocInfo {
    /// 完整 heredoc 文本（<< 算符 + 定界符 + 体 + 闭合定界符）
    pub full_text: String,
    /// 定界符（不含引号）
    pub delimiter: String,
    /// `<<` 算符在原命令中的起始字节偏移
    pub operator_start_index: usize,
    /// `<<` 算符结束位置（exclusive）——同行此后的内容保留
    pub operator_end_index: usize,
    /// heredoc 体起始位置（体前那个换行）
    pub content_start_index: usize,
    /// heredoc 体结束位置（含闭合定界符，exclusive）
    pub content_end_index: usize,
}

/// heredoc 提取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeredocExtractionResult {
    /// heredoc 被占位符替换后的命令
    pub processed_command: String,
    /// 占位符 → 原 heredoc 信息
    pub heredocs: HashMap<String, HeredocInfo>,
}

/// 提取选项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeredocOptions {
    /// 只提取带引号/转义定界符的 heredoc（其体是字面量）。
    /// 非引号 heredoc（`<<EOF`）会展开 $()/${}，体内容必须对安全校验可见，
    /// 因此跳过但记录其内容范围用于嵌套过滤。
    pub quoted_only: bool,
}

/// 提取命令中的 heredoc 并替换为占位符，使 shell-quote 解析不被破坏。
/// 解析后用 [`restore_heredocs`] 还原。
pub fn extract_heredocs(command: &str, options: &HeredocOptions) -> HeredocExtractionResult {
    extract_heredocs_impl(command, options.quoted_only)
}

fn passthrough(command: &str) -> HeredocExtractionResult {
    HeredocExtractionResult {
        processed_command: command.to_string(),
        heredocs: HashMap::new(),
    }
}

fn extract_heredocs_impl(command: &str, quoted_only: bool) -> HeredocExtractionResult {
    let mut heredocs = HashMap::new();

    // 快速通道：没有 << 就跳过
    if !command.contains("<<") {
        return passthrough(command);
    }

    // 安全前置校验：$'...' / $"..."（ANSI-C / locale 引号）会扰乱引号跟踪
    if command.contains("$'") || command.contains("$\"") {
        return passthrough(command);
    }
    let first_heredoc_pos = match command.find("<<") {
        Some(p) => p,
        None => return passthrough(command),
    };
    // 首个 << 之前出现反引号：反引号嵌套规则复杂，且充当 shell_eof_token
    // 触发 heredoc 提前闭合，放弃提取
    if first_heredoc_pos > 0 && command[..first_heredoc_pos].contains('`') {
        return passthrough(command);
    }
    // 算术上下文检查：`(( x = 1 << 2 ))` 中的 << 是位移而非 heredoc。
    // << 前 (( 比 )) 多时无法可靠区分，整体放弃。
    if first_heredoc_pos > 0 {
        let before = &command[..first_heredoc_pos];
        let open_arith = before.matches("((").count();
        let close_arith = before.matches("))").count();
        if open_arith > close_arith {
            return passthrough(command);
        }
    }

    let bytes = command.as_bytes();
    let mut heredoc_matches: Vec<HeredocInfo> = Vec::new();
    // quotedOnly 跳过非引号 heredoc 时，仍记录其内容范围，使嵌套过滤器能
    // 拒绝落在被跳过 heredoc 体内部的引号 heredoc
    let mut skipped_heredoc_ranges: Vec<(usize, usize)> = Vec::new();

    // 增量引号/注释扫描器（原 isInsideQuotedString / isInsideComment 的
    // 合并等价实现）。引号状态对注释盲，注释状态观察引号但反之不然。
    let mut advance_scan = |scan: &mut ScanState, target: usize| {
        for i in scan.pos..target {
            let ch = bytes[i];
            // 任意物理换行清除注释状态（原实现用 lastIndexOf 定行首，等价）
            if ch == b'\n' {
                scan.in_comment = false;
            }
            if scan.in_single {
                if ch == b'\'' {
                    scan.in_single = false;
                }
                continue;
            }
            if scan.in_double {
                if scan.dq_escape_next {
                    scan.dq_escape_next = false;
                    continue;
                }
                if ch == b'\\' {
                    scan.dq_escape_next = true;
                    continue;
                }
                if ch == b'"' {
                    scan.in_double = false;
                }
                continue;
            }
            // 非引号上下文。引号跟踪对注释盲
            if ch == b'\\' {
                scan.pending_backslashes += 1;
                continue;
            }
            let escaped = scan.pending_backslashes % 2 == 1;
            scan.pending_backslashes = 0;
            if escaped {
                continue;
            }
            if ch == b'\'' {
                scan.in_single = true;
            } else if ch == b'"' {
                scan.in_double = true;
            } else if !scan.in_comment && ch == b'#' {
                scan.in_comment = true;
            }
        }
        scan.pos = target;
    };

    let mut scan = ScanState {
        pos: 0,
        in_single: false,
        in_double: false,
        in_comment: false,
        dq_escape_next: false,
        pending_backslashes: 0,
    };

    let mut from = 0usize;
    while let Some(m) = next_heredoc_start(command, from) {
        from = m.end;
        let start_index = m.index;

        // 推进增量扫描器到本匹配位置，使各状态反映 startIndex 之前
        advance_scan(&mut scan, start_index);

        // 在引号串内的 << 不是真正的 heredoc 算符
        if scan.in_single || scan.in_double {
            continue;
        }
        // 注释内的 <<（无引号 # 之后）——bash 中是注释，跳过
        if scan.in_comment {
            continue;
        }
        // 奇数个前导反斜杠：bash 中 `\<<EOF` 不是 heredoc
        if scan.pending_backslashes % 2 == 1 {
            continue;
        }
        // 落在先前被跳过 heredoc 体内部的 << 必须跳过
        let inside_skipped = skipped_heredoc_ranges
            .iter()
            .any(|&(cs, ce)| start_index > cs && start_index < ce);
        if inside_skipped {
            continue;
        }

        let operator_end_index = m.end;
        let delimiter = m.delim.clone();
        let is_dash = m.is_dash;

        // 校验 1：若正则捕获了引号，确认闭合引号确实被反向引用匹配
        if let Some(q) = m.quote {
            if bytes[operator_end_index - 1] as char != q {
                continue;
            }
        }

        let full_match = &command[m.index..m.end];
        let is_escaped_delimiter = full_match.contains('\\');
        let is_quoted_or_escaped = m.quote.is_some() || is_escaped_delimiter;

        // 校验 2：匹配后一字符必须是 bash 词终止符（元字符或 EOF）
        if operator_end_index < command.len() {
            let next_char = bytes[operator_end_index];
            if !matches!(
                next_char,
                b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>'
            ) {
                continue;
            }
        }

        // heredoc 体从下一行开始；逻辑行内的引号串会延长该行，需要带
        // 引号状态地扫描第一个不在引号内的换行
        let mut first_newline_offset = None;
        {
            let mut in_single = false;
            let mut in_double = false;
            let mut k = operator_end_index;
            while k < command.len() {
                let ch = bytes[k];
                if in_single {
                    if ch == b'\'' {
                        in_single = false;
                    }
                    continue;
                }
                if in_double {
                    if ch == b'\\' {
                        k += 2; // 跳过双引号内转义字符
                        continue;
                    }
                    if ch == b'"' {
                        in_double = false;
                    }
                    continue;
                }
                if ch == b'\n' {
                    first_newline_offset = Some(k - operator_end_index);
                    break;
                }
                // 统计未引号上下文的反斜杠数
                let mut backslash_count = 0usize;
                let mut j = k;
                while j > operator_end_index && bytes[j - 1] == b'\\' {
                    backslash_count += 1;
                    j -= 1;
                }
                if backslash_count % 2 == 1 {
                    continue; // 被转义的字符
                }
                if ch == b'\'' {
                    in_single = true;
                } else if ch == b'"' {
                    in_double = true;
                }
                k += 1;
            }
            // 若扫描结束仍在引号内——逻辑行永不结束，没有 heredoc 体
        }

        let first_newline_offset = match first_newline_offset {
            Some(o) => o,
            None => continue, // 没有未引号换行——无 heredoc 体，跳过
        };

        // 同行内容以奇数个反斜杠结尾是行连接——bash 会先连接行再读 heredoc，
        // 本提取器先于连接运行，会误放，直接放弃
        let same_line_content = &command[operator_end_index..operator_end_index + first_newline_offset];
        let mut trailing_backslashes = 0usize;
        for &ch in same_line_content.as_bytes().iter().rev() {
            if ch == b'\\' {
                trailing_backslashes += 1;
            } else {
                break;
            }
        }
        if trailing_backslashes % 2 == 1 {
            continue;
        }

        let content_start_index = operator_end_index + first_newline_offset;
        let after_newline = &command[content_start_index + 1..];
        let content_lines: Vec<&str> = after_newline.split('\n').collect();

        // 查找闭合定界符——必须独占一行
        let mut closing_line_index = None;
        for (i, line) in content_lines.iter().enumerate() {
            if is_dash {
                // <<- 只剥离行首 tab（POSIX/bash），剥离后须与定界符完全相同
                let stripped = line.trim_start_matches('\t');
                if stripped == delimiter {
                    closing_line_index = Some(i);
                    break;
                }
            } else {
                // << 要求闭合定界符前后均无空白
                if *line == delimiter {
                    closing_line_index = Some(i);
                    break;
                }
            }
            // PST_EOFTOKEN 式提前闭合检查：以定界符开头且其后跟替换闭合符
            // (`)` `}` 反引号) 或 shell 元字符时，bash 可能提前闭合 heredoc——
            // 立即放弃整个提取
            let eof_check_line = if is_dash {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if eof_check_line.len() > delimiter.len()
                && eof_check_line.starts_with(&delimiter)
            {
                let char_after = eof_check_line.as_bytes()[delimiter.len()];
                if matches!(
                    char_after,
                    b')' | b'`' | b'}' | b'|' | b'&' | b';' | b'(' | b'<' | b'>'
                ) {
                    closing_line_index = None;
                    break;
                }
            }
        }

        // quotedOnly 且本 heredoc 非引号：记录内容范围用于嵌套过滤，但不加入
        // 提取结果。无闭合定界符时 bash 把余下全部输入当 heredoc 体（并展开
        // 其中的 $()），因此整个剩余范围都要跟踪
        if quoted_only && !is_quoted_or_escaped {
            let skip_content_end_index = match closing_line_index {
                None => command.len(),
                Some(ci) => {
                    let skip_lines_up_to_closing = &content_lines[..=ci];
                    let skip_content_length: usize =
                        skip_lines_up_to_closing.iter().map(|l| l.len()).sum::<usize>()
                            + ci; // join('\n') 的换行数 = 行数 - 1 = ci
                    content_start_index + 1 + skip_content_length
                }
            };
            skipped_heredoc_ranges.push((content_start_index, skip_content_end_index));
            continue;
        }

        let closing_line_index = match closing_line_index {
            Some(i) => i,
            None => continue, // 无闭合定界符——格式错误，跳过
        };

        // 计算结束位置
        let lines_up_to_closing = &content_lines[..=closing_line_index];
        let content_length: usize =
            lines_up_to_closing.iter().map(|l| l.len()).sum::<usize>() + closing_line_index;
        let content_end_index = content_start_index + 1 + content_length;

        // 与任何被跳过 heredoc 的内容范围重叠时放弃（同一行上多个 heredoc
        // 共用同一个换行，后续 heredoc 的体搜索会穿过前者的体）
        let overlaps_skipped = skipped_heredoc_ranges
            .iter()
            .any(|&(cs, ce)| content_start_index < ce && cs < content_end_index);
        if overlaps_skipped {
            continue;
        }

        let operator_text = &command[start_index..operator_end_index];
        let content_text = &command[content_start_index..content_end_index];
        let full_text = format!("{}{}", operator_text, content_text);

        heredoc_matches.push(HeredocInfo {
            full_text,
            delimiter,
            operator_start_index: start_index,
            operator_end_index,
            content_start_index,
            content_end_index,
        });
    }

    if heredoc_matches.is_empty() {
        return passthrough(command);
    }

    // 过滤嵌套 heredoc——算符落在另一个 heredoc 体内的排除
    let mut top_level: Vec<usize> = Vec::new();
    for (i, candidate) in heredoc_matches.iter().enumerate() {
        let nested = heredoc_matches.iter().enumerate().any(|(j, other)| {
            i != j
                && candidate.operator_start_index > other.content_start_index
                && candidate.operator_start_index < other.content_end_index
        });
        if !nested {
            top_level.push(i);
        }
    }
    if top_level.is_empty() {
        return passthrough(command);
    }

    // 多个 heredoc 共用同一内容起始位置会造成替换时索引错乱，直接放弃
    let mut seen_starts = std::collections::HashSet::new();
    let dup = top_level
        .iter()
        .any(|&i| !seen_starts.insert(heredoc_matches[i].content_start_index));
    if dup {
        return passthrough(command);
    }

    // 按 content_end 降序排列，从后往前替换以保护索引
    let mut order = top_level.clone();
    order.sort_by(|&a, &b| heredoc_matches[b].content_end_index.cmp(&heredoc_matches[a].content_end_index));

    let salt = generate_placeholder_salt();
    let mut processed_command = command.to_string();
    for (idx, &info_idx) in order.iter().enumerate() {
        let info = heredoc_matches[info_idx].clone();
        // 反向编号（按降序排列）
        let placeholder_index = top_level.len() - 1 - idx;
        let placeholder = format!(
            "{}{}_{}{}",
            HEREDOC_PLACEHOLDER_PREFIX, placeholder_index, salt, HEREDOC_PLACEHOLDER_SUFFIX
        );
        heredocs.insert(placeholder.clone(), info.clone());
        // 替换 heredoc 为占位符，保留同行内容：
        // - 算符之前保留
        // - 算符替换为占位符
        // - 算符到 heredoc 体之间的内容保留（如 " && echo done"）
        // - heredoc 体（换行到闭合定界符）移除
        // - 闭合定界符之后保留
        let mut spliced = String::with_capacity(processed_command.len() + placeholder.len());
        spliced.push_str(&processed_command[..info.operator_start_index]);
        spliced.push_str(&placeholder);
        spliced.push_str(&processed_command[info.operator_end_index..info.content_start_index]);
        spliced.push_str(&processed_command[info.content_end_index..]);
        processed_command = spliced;
    }

    HeredocExtractionResult {
        processed_command,
        heredocs,
    }
}

/// 增量引号/注释扫描器的状态
struct ScanState {
    pos: usize,
    in_single: bool,
    in_double: bool,
    in_comment: bool,
    dq_escape_next: bool,
    pending_backslashes: usize,
}

/// 把占位符还原为原 heredoc 内容（单字符串）
fn restore_heredocs_in_string(text: &str, heredocs: &HashMap<String, HeredocInfo>) -> String {
    let mut result = text.to_string();
    for (placeholder, info) in heredocs {
        if result.contains(placeholder) {
            result = result.replace(placeholder, &info.full_text);
        }
    }
    result
}

/// 还原字符串数组中的 heredoc 占位符。
pub fn restore_heredocs(
    parts: &[String],
    heredocs: &HashMap<String, HeredocInfo>,
) -> Vec<String> {
    if heredocs.is_empty() {
        return parts.to_vec();
    }
    parts
        .iter()
        .map(|part| restore_heredocs_in_string(part, heredocs))
        .collect()
}

/// 命令是否含 heredoc 语法（快速检查，不做合法性校验）。
pub fn contains_heredoc(command: &str) -> bool {
    next_heredoc_start(command, 0).is_some()
}
