//! xml.rs — XML/HTML 特殊字符转义。
//!
//! 重写自 `src/utils/xml.ts`。用于把不可信字符串（进程 stdout、用户
//! 输入、外部数据）安全插入 XML 文本节点或属性值。

/// 转义元素文本内容所需的字符（`& < >`），用于 `<tag>${here}</tag>`。
///
/// 替换按 `& → &amp; → &lt; → &gt;` 的顺序进行，与 TS 一致——先转义
/// `&` 保证后续插入的实体不再被二次转义。
pub fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 转义双引号/单引号属性值所需的字符，用于 `<tag attr="${here}">`。
///
/// 在 `escape_xml` 基础上追加 `"` → `&quot;` 与 `'` → `&apos;`。
pub fn escape_xml_attr(input: &str) -> String {
    let escaped = escape_xml(input);
    let mut out = String::with_capacity(escaped.len());
    for ch in escaped.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_text_content() {
        assert_eq!(escape_xml("a<b>&c"), "a&lt;b&gt;&amp;c");
        // 已转义的 & 不再二次转义之外，普通文本不变
        assert_eq!(escape_xml("plain"), "plain");
    }

    #[test]
    fn escapes_attribute_quotes() {
        assert_eq!(escape_xml_attr("\"q\" & 's'"), "&quot;q&quot; &amp; &apos;s&apos;");
    }
}
