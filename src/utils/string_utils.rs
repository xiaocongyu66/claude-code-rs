//! string_utils.rs — 通用字符串工具与安全累积器。
//!
//! 重写自 `src/utils/stringUtils.ts`。函数行为与边界条件逐条对齐 TS 源码；
//! 长度一律按字符（chars）计数——与 TS 的 UTF-16 码元计数在 BMP 字符上
//! 等价（代理对字符存在细微差异，本模块处理的是 ASCII 为主的输出文本）。

const TRUNCATION_MARKER: &str = "...[truncated]";
// 控制内存占用，避免 RSS 膨胀；超限部分由 ShellCommand 落盘。
const MAX_STRING_LENGTH: usize = 1 << 21; // 2^21

/// 转义正则特殊字符，使字符串可作为字面量用于正则模式。
pub fn escape_reg_exp(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 大写首字符，其余不变（与 lodash `capitalize` 不同，不会小写其余部分）。
///
/// - `capitalize("fooBar")` → `"FooBar"`
/// - `capitalize("hello world")` → `"Hello world"`
pub fn capitalize(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => {
            let mut out: String = first.to_uppercase().collect();
            out.extend(chars);
            out
        }
        None => String::new(),
    }
}

/// 根据数量返回单数或复数形式，替代内联的 `word${n === 1 ? '' : 's'}`。
///
/// - `plural(1, "file")` → `"file"`
/// - `plural(3, "file")` → `"files"`
/// - `plural(2, "entry", "entries")` → `"entries"`
pub fn plural(n: u64, word: &str, plural_word: Option<&str>) -> String {
    if n == 1 {
        return word.to_string();
    }
    match plural_word {
        Some(p) => p.to_string(),
        None => format!("{word}s"),
    }
}

/// 返回字符串的第一行（用于 diff 渲染中的 shebang 检测）。
pub fn first_line_of(input: &str) -> &str {
    match input.find('\n') {
        Some(idx) => &input[..idx],
        None => input,
    }
}

/// 用 indexOf 跳跃的方式计数 `needle` 出现次数（而非逐字符遍历）。
///
/// 对齐 TS：可传入字符串或 Buffer（`indexOf` 结构类型）；`start` 之后
/// 的部分才参与计数。
pub fn count_char_in_string(input: &str, needle: char, start: usize) -> usize {
    let mut count = 0;
    for (index, ch) in input.chars().enumerate() {
        if index < start {
            continue;
        }
        if ch == needle {
            count += 1;
        }
    }
    count
}

/// 全角（zenkaku）数字 → 半角数字，便于日文/CJK IME 输入。
pub fn normalize_full_width_digits(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ('\u{ff10}'..='\u{ff19}').contains(&ch) {
                char::from_u32(ch as u32 - 0xfee0).unwrap_or(ch)
            } else {
                ch
            }
        })
        .collect()
}

/// 全角空格（U+3000）→ 半角空格。
pub fn normalize_full_width_space(input: &str) -> String {
    input.replace('\u{3000}', " ")
}

/// 用分隔符安全拼接字符串数组；结果超过 `max_size` 时截断。
///
/// 默认分隔符 `,`，默认上限 `MAX_STRING_LENGTH`（2^21）。
pub fn safe_join_lines(lines: &[&str], delimiter: &str, max_size: Option<usize>) -> String {
    let max_size = max_size.unwrap_or(MAX_STRING_LENGTH);
    let mut result = String::new();

    for line in lines {
        let delimiter_to_add = if result.is_empty() { "" } else { delimiter };
        let full_addition_len = delimiter_to_add.chars().count() + line.chars().count();

        if result.chars().count() + full_addition_len <= max_size {
            // 整行放得下
            result.push_str(delimiter_to_add);
            result.push_str(line);
        } else {
            // 需要截断
            let remaining_space = max_size
                .saturating_sub(result.chars().count())
                .saturating_sub(delimiter_to_add.chars().count())
                .saturating_sub(TRUNCATION_MARKER.chars().count());

            if remaining_space > 0 {
                result.push_str(delimiter_to_add);
                result.extend(line.chars().take(remaining_space));
                result.push_str(TRUNCATION_MARKER);
            } else {
                // 这行一点都放不下，只加截断标记
                result.push_str(TRUNCATION_MARKER);
            }
            return result;
        }
    }
    result
}

/// 从末尾截断的字符串累积器：超过上限时截断尾部、保留开头。
///
/// 防止 RangeError 崩溃，同时保留输出的起始部分。
pub struct EndTruncatingAccumulator {
    content: String,
    is_truncated: bool,
    total_bytes_received: usize,
    max_size: usize,
}

impl EndTruncatingAccumulator {
    /// 创建累积器；`max_size` 为触发截断的字符数上限。
    pub fn new(max_size: Option<usize>) -> Self {
        Self {
            content: String::new(),
            is_truncated: false,
            total_bytes_received: 0,
            max_size: max_size.unwrap_or(MAX_STRING_LENGTH),
        }
    }

    /// 追加数据。总计超过 `max_size` 时截断尾部以维持上限。
    pub fn append(&mut self, data: &str) {
        self.total_bytes_received += data.chars().count();

        // 已经到达上限并截断后，不再修改内容
        if self.is_truncated && self.content.chars().count() >= self.max_size {
            return;
        }

        let current_len = self.content.chars().count();
        if current_len + data.chars().count() > self.max_size {
            // 只追加放得下的部分
            let remaining_space = self.max_size - current_len;
            if remaining_space > 0 {
                self.content.extend(data.chars().take(remaining_space));
            }
            self.is_truncated = true;
        } else {
            self.content.push_str(data);
        }
    }

    /// 返回累积的字符串；若发生过截断则附加截断标记。
    pub fn render(&self) -> String {
        if !self.is_truncated {
            return self.content.clone();
        }
        let truncated_bytes = self.total_bytes_received.saturating_sub(self.max_size);
        let truncated_kb = (truncated_bytes + 512) / 1024; // Math.round(x / 1024)
        format!("{}\n... [output truncated - {}KB removed]", self.content, truncated_kb)
    }

    /// 清空所有累积数据。
    pub fn clear(&mut self) {
        self.content.clear();
        self.is_truncated = false;
        self.total_bytes_received = 0;
    }

    /// 当前累积内容的字符数。
    pub fn len(&self) -> usize {
        self.content.chars().count()
    }

    /// 是否发生过截断。
    pub fn truncated(&self) -> bool {
        self.is_truncated
    }

    /// 截断前累计收到的字符数。
    pub fn total_bytes(&self) -> usize {
        self.total_bytes_received
    }
}

/// 将文本截断到最多 `max_lines` 行；截断时追加省略号。
///
/// 对齐 TS `split('\n')`：结尾的 '\n' 会产生一个空行元素（"a\n" →
/// ["a", ""]），此处直接按该切分计数。
pub fn truncate_to_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let kept: String = lines
        .into_iter()
        .take(max_lines)
        .collect::<Vec<&str>>()
        .join("\n");
    format!("{kept}…")
}
