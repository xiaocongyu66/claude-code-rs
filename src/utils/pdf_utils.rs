//! pdf_utils.rs — PDF 文档相关的辅助判定。
//!
//! 重写自 `src/utils/pdfUtils.ts`。语义对齐：
//!
//! - `parse_pdf_page_range` 解析 "5" / "1-10" / "3-" 三种页范围格式，
//!   1-indexed，非法输入返回 None（`last_page` 为 usize::MAX 对齐 Infinity）。
//! - `is_pdf_supported` 判定当前主循环模型是否支持 PDF 文档块
//!   （除 claude-3-haiku 外均支持，子串匹配覆盖 Bedrock 前缀/Vertex @日期）。
//! - `is_pdf_extension` 判定扩展名（带或不带前导点）是否为 pdf。

use crate::utils::model::model::get_main_loop_model;

/// 被特殊处理的文档扩展名。
pub const DOCUMENT_EXTENSIONS: &[&str] = &["pdf"];

/// 解析页范围字符串。
///
/// 支持格式：
/// - "5" → { first_page: 5, last_page: 5 }
/// - "1-10" → { first_page: 1, last_page: 10 }
/// - "3-" → { first_page: 3, last_page: usize::MAX }（对齐 TS Infinity）
///
/// 非数字、0、倒序范围均返回 None。页码 1-indexed。
pub fn parse_pdf_page_range(pages: &str) -> Option<(usize, usize)> {
    let trimmed = pages.trim();
    if trimmed.is_empty() {
        return None;
    }

    // "N-" 开区间范围
    if let Some(first_str) = trimmed.strip_suffix('-') {
        let first: usize = first_str.trim().parse().ok()?;
        if first < 1 {
            return None;
        }
        return Some((first, usize::MAX));
    }

    match trimmed.find('-') {
        None => {
            // 单页："5"
            let page: usize = trimmed.parse().ok()?;
            if page < 1 {
                return None;
            }
            Some((page, page))
        }
        Some(dash_index) => {
            // 范围："1-10"
            let first: usize = trimmed[..dash_index].parse().ok()?;
            let last: usize = trimmed[dash_index + 1..].parse().ok()?;
            if first < 1 || last < 1 || last < first {
                return None;
            }
            Some((first, last))
        }
    }
}

/// 当前模型是否支持 PDF 文档块。Haiku 3 是唯一 predates PDF 支持的模型。
pub fn is_pdf_supported() -> bool {
    !get_main_loop_model().to_lowercase().contains("claude-3-haiku")
}

/// 扩展名是否为 PDF（带或不带前导点，大小写不敏感）。
pub fn is_pdf_extension(ext: &str) -> bool {
    let normalized = ext.strip_prefix('.').unwrap_or(ext);
    DOCUMENT_EXTENSIONS.contains(&normalized.to_lowercase().as_str())
}
