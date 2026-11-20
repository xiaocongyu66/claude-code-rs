//! truncate.rs — 宽度感知的截断/换行。
//!
//! 重写自 `src/utils/truncate.ts`。语义对齐：
//!
//! - 所有函数按 **终端显示宽度** 截断（不是字符数），在 grapheme 边界
//!   切割，避免切断 emoji / CJK / 代用对。
//! - `truncate_path_middle`：保留目录上下文与文件名，中间插 '…'。
//! - `truncate_to_width`：尾部追加 '…'；`truncate_start_to_width`：
//!   头部前置 '…'；`truncate_to_width_no_ellipsis`：不加省略号。
//! - `truncate(str, max, single_line)`：可选先按首个换行截断（追加 '…'）。
//! - `wrap_text`：按宽度换行。
//!
//! 偏差：
//! - TS 用 `@anthropic/ink` 的 stringWidth（基于 get-east-asian-width 等
//!   unicode 数据表）与 Intl.Segmenter。Cargo.toml 无 unicode-width /
//!   unicode-segmentation，这里实现**近似版本**：
//!   - 宽度：常见全宽区间（CJK 统一表意文字、谚文、全角符号、emoji
//!     区块等）记 2，组合记号（combining mark）记 0，其余记 1。
//!   - grapheme：按 Unicode scalar value 切分，把组合记号吸附到前一个
//!     基字符（近似 Intl.Segmenter 的 grapheme cluster 切分；复杂的
//!     emoji ZWJ 序列在近似下可能被拆分，换行/截断仍保证不切断代用对）。
//! - TS 注释说此文件“needs ink/stringWidth (not leaf-safe)”；Rust 侧用
//!   近似实现落了地，等引入 unicode-width crate 后可替换测宽函数。

// ---------------------------------------------------------------------------
// 宽度与 grapheme 近似
// ---------------------------------------------------------------------------

fn char_width(c: char) -> usize {
    let cp = c as u32;
    // 组合记号：宽度 0
    if is_combining(cp) {
        return 0;
    }
    // 全宽 / 宽区间：宽度 2
    if is_wide(cp) {
        return 2;
    }
    1
}

fn is_combining(cp: u32) -> bool {
    matches!(cp,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x05BF..=0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7..=0x05C7
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670..=0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x0711..=0x0711
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0
        | 0x07EB..=0x07F3
        | 0x07FD..=0x07FD
        | 0x0816..=0x0819
        | 0x081B..=0x0823
        | 0x0825..=0x0827
        | 0x0829..=0x082D
        | 0x0859..=0x085B
        | 0x08D3..=0x08E1
        | 0x08E3..=0x0903
        | 0x093A..=0x093C
        | 0x093E..=0x094F
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0981..=0x0983
        | 0x09BC..=0x09BC
        | 0x09BE..=0x09C4
        | 0x09C7..=0x09C8
        | 0x09CB..=0x09CD
        | 0x09D7..=0x09D7
        | 0x09E2..=0x09E3
        | 0x09FE..=0x09FE
        | 0x0A01..=0x0A03
        | 0x0A3C..=0x0A3C
        | 0x0A3E..=0x0A42
        | 0x0A47..=0x0A48
        | 0x0A4B..=0x0A4D
        | 0x0A51..=0x0A51
        | 0x0A70..=0x0A71
        | 0x0A75..=0x0A75
        | 0x0A81..=0x0A83
        | 0x0ABC..=0x0ABC
        | 0x0ABE..=0x0AC5
        | 0x0AC7..=0x0AC9
        | 0x0ACB..=0x0ACD
        | 0x0AE2..=0x0AE3
        | 0x0AFA..=0x0AFF
        | 0x0B01..=0x0B03
        | 0x0B3C..=0x0B3C
        | 0x0B3E..=0x0B44
        | 0x0B47..=0x0B48
        | 0x0B4B..=0x0B4D
        | 0x0B56..=0x0B57
        | 0x0B62..=0x0B63
        | 0x0B82..=0x0B82
        | 0x0BBE..=0x0BC2
        | 0x0BC6..=0x0BC8
        | 0x0BCA..=0x0BCD
        | 0x0BD7..=0x0BD7
        | 0x0C00..=0x0C04
        | 0x0C3E..=0x0C44
        | 0x0C46..=0x0C48
        | 0x0C4A..=0x0C4D
        | 0x0C55..=0x0C56
        | 0x0C62..=0x0C63
        | 0x0C81..=0x0C83
        | 0x0CBC..=0x0CBC
        | 0x0CBE..=0x0CC4
        | 0x0CC6..=0x0CC8
        | 0x0CCA..=0x0CCD
        | 0x0CD5..=0x0CD6
        | 0x0CE2..=0x0CE3
        | 0x0D00..=0x0D03
        | 0x0D3B..=0x0D3C
        | 0x0D3E..=0x0D44
        | 0x0D46..=0x0D48
        | 0x0D4A..=0x0D4D
        | 0x0D57..=0x0D57
        | 0x0D62..=0x0D63
        | 0x0D81..=0x0D83
        | 0x0DCA..=0x0DCA
        | 0x0DCF..=0x0DD4
        | 0x0DD6..=0x0DD6
        | 0x0DD8..=0x0DDF
        | 0x0DF2..=0x0DF3
        | 0x0E31..=0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x0EB1..=0x0EB1
        | 0x0EB4..=0x0EBC
        | 0x0EC8..=0x0ECD
        | 0x0F18..=0x0F19
        | 0x0F35..=0x0F35
        | 0x0F37..=0x0F37
        | 0x0F39..=0x0F39
        | 0x0F71..=0x0F7E
        | 0x0F80..=0x0F84
        | 0x0F86..=0x0F87
        | 0x0F8D..=0x0F97
        | 0x0F99..=0x0FBC
        | 0x0FC6..=0x0FC6
        | 0x102B..=0x103E
        | 0x1056..=0x1059
        | 0x105E..=0x1060
        | 0x1062..=0x1064
        | 0x1067..=0x106D
        | 0x1071..=0x1074
        | 0x1082..=0x108D
        | 0x108F..=0x108F
        | 0x109A..=0x109D
        | 0x135D..=0x135F
        | 0x1712..=0x1714
        | 0x1732..=0x1734
        | 0x1752..=0x1753
        | 0x1772..=0x1773
        | 0x17B4..=0x17D3
        | 0x17DD..=0x17DD
        | 0x180B..=0x180D
        | 0x1885..=0x1886
        | 0x18A9..=0x18A9
        | 0x1920..=0x192B
        | 0x1930..=0x193B
        | 0x1A17..=0x1A1B
        | 0x1A55..=0x1A5E
        | 0x1A60..=0x1A7C
        | 0x1A7F..=0x1A7F
        | 0x1AB0..=0x1ACE
        | 0x1B00..=0x1B04
        | 0x1B34..=0x1B44
        | 0x1B6B..=0x1B73
        | 0x1B80..=0x1B82
        | 0x1BA1..=0x1BAD
        | 0x1BE6..=0x1BF3
        | 0x1C2C..=0x1C33
        | 0x1C36..=0x1C37
        | 0x1CD0..=0x1CD2
        | 0x1CD4..=0x1CE8
        | 0x1CED..=0x1CED
        | 0x1CF4..=0x1CF4
        | 0x1CF8..=0x1CF9
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20F0
        | 0x2CEF..=0x2CF1
        | 0x2D7F..=0x2D7F
        | 0x2DE0..=0x2DFF
        | 0x302A..=0x302F
        | 0x3099..=0x309A
        | 0xA66F..=0xA672
        | 0xA674..=0xA67D
        | 0xA69E..=0xA69F
        | 0xA6F0..=0xA6F1
        | 0xA802..=0xA802
        | 0xA806..=0xA806
        | 0xA80B..=0xA80B
        | 0xA823..=0xA827
        | 0xA880..=0xA881
        | 0xA8B4..=0xA8C5
        | 0xA8E0..=0xA8F1
        | 0xA8FF..=0xA8FF
        | 0xA926..=0xA92D
        | 0xA947..=0xA953
        | 0xA980..=0xA983
        | 0xA9B3..=0xA9C0
        | 0xA9E5..=0xA9E5
        | 0xAA29..=0xAA36
        | 0xAA43..=0xAA43
        | 0xAA4C..=0xAA4D
        | 0xAA7B..=0xAA7D
        | 0xAAB0..=0xAAB4
        | 0xAAB7..=0xAAB8
        | 0xAABE..=0xAABF
        | 0xAAC1..=0xAAC1
        | 0xAAEB..=0xAAEF
        | 0xAAF5..=0xAAF6
        | 0xABE3..=0xABEA
        | 0xABEC..=0xABED
        | 0xFB1E..=0xFB1E
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
    )
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F     // 谚文 jamo
        | 0x231A..=0x231B   // ⌚⌛
        | 0x2329..=0x232A
        | 0x23E9..=0x23EC
        | 0x23F0..=0x23F0
        | 0x23F3..=0x23F3
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267F..=0x267F
        | 0x2693..=0x2693
        | 0x26A1..=0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE..=0x26CE
        | 0x26D4..=0x26D4
        | 0x26EA..=0x26EA
        | 0x26F2..=0x26F3
        | 0x26F5..=0x26F5
        | 0x26FA..=0x26FA
        | 0x26FD..=0x26FD
        | 0x2705..=0x2705
        | 0x270A..=0x270B
        | 0x2728..=0x2728
        | 0x274C..=0x274C
        | 0x274E..=0x274E
        | 0x2753..=0x2755
        | 0x2757..=0x2757
        | 0x2795..=0x2797
        | 0x27B0..=0x27B0
        | 0x27BF..=0x27BF
        | 0x2B1B..=0x2B1C
        | 0x2B50..=0x2B50
        | 0x2B55..=0x2B55
        | 0x2E80..=0x303E   // CJK 标点/部首
        | 0x3041..=0x33FF   // 平假名/片假名/CJK 符号
        | 0x3400..=0x4DBF   // CJK 扩展 A
        | 0x4E00..=0x9FFF   // CJK 统一表意文字
        | 0xA000..=0xA4CF   // 彝文
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3   // 谚文音节
        | 0xF900..=0xFAFF   // CJK 兼容表意
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60   // 全角 ASCII
        | 0xFFE0..=0xFFE6   // 全角符号
        | 0x16FE0..=0x16FE4
        | 0x16FF0..=0x16FF1
        | 0x17000..=0x187F7 // 西夏文
        | 0x18800..=0x18CD5
        | 0x18D00..=0x18D08
        | 0x1AFF0..=0x1AFFD
        | 0x1B000..=0x1B122
        | 0x1B150..=0x1B167
        | 0x1B170..=0x1B2FB
        | 0x1F000..=0x1F0FF // 麻将/扑克
        | 0x1F100..=0x1F1AD
        | 0x1F1E6..=0x1F1FF // 旗帜
        | 0x1F201..=0x1F2FF
        | 0x1F300..=0x1FAD6 // emoji 杂项
        | 0x1FB00..=0x1FB92
        | 0x1FB94..=0x1FBCA
        | 0x1FBF0..=0x1FBF9
        | 0x20000..=0x3FFFD // CJK 扩展 B-F
    )
}

/// 计算字符串的近似显示宽度（终端列数）。
pub fn string_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 把文本切成 grapheme 近似段（组合记号吸附到前一个基字符）。
///
/// 返回的段按原顺序拼接可还原原文本。
pub fn grapheme_segments(text: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    for c in text.chars() {
        if is_combining(c as u32) {
            if let Some(last) = segments.last_mut() {
                last.push(c);
                continue;
            }
        }
        segments.push(c.to_string());
    }
    segments
}

fn segment_width(segment: &str) -> usize {
    segment.chars().map(char_width).sum()
}

// ---------------------------------------------------------------------------
// 截断函数
// ---------------------------------------------------------------------------

/// 中间截断文件路径，同时保留目录上下文与文件名。
/// 宽度感知（CJK/emoji 正确计量）。
///
/// 例：maxLength=30 时 "src/components/deeply/nested/folder/MyComponent.tsx"
/// → "src/components/…/MyComponent.tsx"。
pub fn truncate_path_middle(path: &str, max_length: usize) -> String {
    // 无需截断
    if string_width(path) <= max_length {
        return path.to_string();
    }

    // 零边界（max_length 为 usize，无负数）
    if max_length == 0 {
        return "…".to_string();
    }

    // 至少容得下 "…" + 有意义的内容
    if max_length < 5 {
        return truncate_to_width(path, max_length);
    }

    // 找文件名（最后一段路径）；显示时保留前导斜杠
    let last_slash = path.rfind('/');
    let (filename, directory) = match last_slash {
        Some(idx) => (&path[idx..], &path[..idx]),
        None => (path, ""),
    };
    let filename_width = string_width(filename);

    // 文件名本身就太长：从头截断（保留尾部）
    if filename_width >= max_length - 1 {
        return truncate_start_to_width(path, max_length);
    }

    // directory + "…" + filename 的空间分配
    let available_for_dir = max_length - 1 - filename_width; // -1 给省略号

    if available_for_dir == 0 {
        // 目录没空间：只显示文件名（必要时截断）
        return truncate_start_to_width(filename, max_length);
    }

    let truncated_dir = truncate_to_width_no_ellipsis(directory, available_for_dir);
    format!("{truncated_dir}…{filename}")
}

/// 按最大显示宽度截断，grapheme 边界切割，截断时追加 '…'。
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if string_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut width = 0usize;
    let mut result = String::new();
    for segment in grapheme_segments(text) {
        let seg_width = segment_width(&segment);
        if width + seg_width > max_width - 1 {
            break;
        }
        result.push_str(&segment);
        width += seg_width;
    }
    result.push('…');
    result
}

/// 从字符串头部截断、保留尾部，截断时前置 '…'。
/// 宽度感知、grapheme 安全。
pub fn truncate_start_to_width(text: &str, max_width: usize) -> String {
    if string_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let segments = grapheme_segments(text);
    let mut width = 0usize;
    let mut start_idx = segments.len();
    for i in (0..segments.len()).rev() {
        let seg_width = segment_width(&segments[i]);
        if width + seg_width > max_width - 1 {
            break; // -1 给 '…'
        }
        width += seg_width;
        start_idx = i;
    }
    let tail: String = segments[start_idx..].concat();
    format!("…{tail}")
}

/// 按最大显示宽度截断，**不追加**省略号。
/// 调用方自己加分隔符的场景（如中间截断的 '…'）使用。
pub fn truncate_to_width_no_ellipsis(text: &str, max_width: usize) -> String {
    if string_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut width = 0usize;
    let mut result = String::new();
    for segment in grapheme_segments(text) {
        let seg_width = segment_width(&segment);
        if width + seg_width > max_width {
            break;
        }
        result.push_str(&segment);
        width += seg_width;
    }
    result
}

/// 按终端列数截断；可选 `single_line` 先按首个换行截断。
/// 截断发生时追加 '…'。
pub fn truncate(text: Option<&str>, max_width: usize, single_line: bool) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let result;

    // single_line：先按首个换行截断
    if single_line {
        if let Some(first_newline) = text.find('\n') {
            result = &text[..first_newline];
            // 总宽度（含省略号）不超 max_width 时直接截断+省略号
            if string_width(result) + 1 > max_width {
                return truncate_to_width(result, max_width);
            }
            return format!("{result}…");
        }
    }

    if string_width(text) <= max_width {
        return text.to_string();
    }
    truncate_to_width(text, max_width)
}

/// 按宽度换行文本，返回行列表。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0usize;

    for segment in grapheme_segments(text) {
        let seg_width = segment_width(&segment);
        if current_width + seg_width <= width {
            current_line.push_str(&segment);
            current_width += seg_width;
        } else {
            if !current_line.is_empty() {
                lines.push(current_line.clone());
            }
            current_line = segment;
            current_width = seg_width;
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }
    lines
}
