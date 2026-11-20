//! content_array.rs — 内容块数组的插入定位工具。
//!
//! 重写自 `src/utils/contentArray.ts`。用于在 user message 的 content
//! 数组中把补充内容块（如缓存编辑指令）放到正确位置。TS 源操作 JS
//! `unknown[]`，此移植使用 `serde_json::Value`。
//!
//! 放置规则：
//! - 若存在 tool_result 块：插到最后一个 tool_result 之后
//! - 否则：插到最后一个块之前
//! - 若插入的块成为末尾元素，追加一个文本续接块（部分 API 要求
//!   prompt 不能以非文本内容结尾）

use serde_json::Value;

fn is_tool_result(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("tool_result")
}

/// 将 `block` 插入 content 数组（原地修改）。
pub fn insert_block_after_tool_results(content: &mut Vec<Value>, block: Value) {
    let mut last_tool_result_index: Option<usize> = None;
    for (i, item) in content.iter().enumerate() {
        if is_tool_result(item) {
            last_tool_result_index = Some(i);
        }
    }

    if let Some(idx) = last_tool_result_index {
        let insert_pos = idx + 1;
        content.insert(insert_pos, block);
        // 插入块成为末尾时追加文本续接块
        if insert_pos == content.len() - 1 {
            content.push(serde_json::json!({ "type": "text", "text": "." }));
        }
    } else {
        // 没有 tool_result 块 —— 插到最后一个块之前（数组为空时插到位置 0）
        let insert_index = content.len().saturating_sub(1);
        content.insert(insert_index, block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inserts_after_last_tool_result() {
        let mut content = vec![
            json!({"type": "text", "text": "q"}),
            json!({"type": "tool_result", "tool_use_id": "1"}),
            json!({"type": "text", "text": "tail"}),
        ];
        insert_block_after_tool_results(&mut content, json!({"type": "text", "text": "x"}));
        assert_eq!(content[2], json!({"type": "text", "text": "x"}));
        assert_eq!(content[3], json!({"type": "text", "text": "tail"}));
    }

    #[test]
    fn appends_continuation_when_block_becomes_last() {
        let mut content = vec![
            json!({"type": "text", "text": "q"}),
            json!({"type": "tool_result"}),
        ];
        insert_block_after_tool_results(&mut content, json!({"type": "tool_result", "content": 1}));
        assert_eq!(content.len(), 4);
        assert_eq!(content[3], json!({"type": "text", "text": "."}));
    }

    #[test]
    fn without_tool_results_inserts_before_last() {
        let mut content = vec![
            json!({"type": "text", "text": "a"}),
            json!({"type": "text", "text": "b"}),
        ];
        insert_block_after_tool_results(&mut content, json!({"type": "text", "text": "x"}));
        assert_eq!(content[1], json!({"type": "text", "text": "x"}));
        assert_eq!(content[2], json!({"type": "text", "text": "b"}));
    }

    #[test]
    fn empty_content_takes_index_zero() {
        let mut content: Vec<Value> = vec![];
        insert_block_after_tool_results(&mut content, json!({"type": "text", "text": "x"}));
        assert_eq!(content.len(), 1);
    }
}
