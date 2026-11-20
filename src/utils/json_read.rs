//! json_read.rs — 叶子级 stripBOM。
//!
//! 重写自 `src/utils/jsonRead.ts`。从 json.ts 中抽出以打断
//! settings → json → log → types/logs → … → settings 的循环依赖。
//!
//! UTF-8 BOM（U+FEFF）：PowerShell 5.x 默认写出带 BOM 的 UTF-8
//! （Out-File、Set-Content），不剥离会导致 JSON 解析失败。

const UTF8_BOM: char = '\u{FEFF}';

/// 剥离起始的 UTF-8 BOM；无 BOM 时原样返回。
pub fn strip_bom(content: &str) -> &str {
    content.strip_prefix(UTF8_BOM).unwrap_or(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_bom() {
        assert_eq!(strip_bom("\u{FEFF}{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn keeps_content_without_bom() {
        assert_eq!(strip_bom("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_bom(""), "");
    }
}
