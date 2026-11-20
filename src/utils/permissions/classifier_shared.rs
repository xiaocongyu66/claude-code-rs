//! classifier_shared.rs — 分类器权限系统的共享基础设施。
//!
//! 重写自 `src/utils/permissions/classifierShared.ts`。该模块为
//! `bashClassifier.ts`（语义 Bash 命令匹配）与 `yoloClassifier.ts`
//! （YOLO 模式安全分类）提供公共类型与工具函数。
//!
//! 语义对齐说明：
//! - TS 的内容块类型来自 `@anthropic-ai/sdk`（`BetaContentBlock`），
//!   messages 模块在 Rust 侧尚未落地，因此内容块以 `serde_json::Value`
//!   表达——`{ type: "tool_use", name: string, input: object }` 的判别
//!   字段直接按 JSON 键读取，与 TS 的 `b.type === 'tool_use'` 手工判断
//!   等价（SDK 的判别联合在运行时也就是这两个字段）；
//! - TS 的 `parseClassifierResponse<T extends z.ZodTypeAny>` 用 zod 的
//!   `safeParse` 做运行时校验并返回 `null` 表示失败。Rust 无 zod，
//!   泛型约束改为 `serde::de::DeserializeOwned`：`serde_json::from_value`
//!   失败即返回 `None`，与「解析失败返回 null」的语义一致；
//! - 两个函数对未落地模块零依赖，可独立编译。

use serde::de::DeserializeOwned;
use serde_json::Value;

/// 判别字段：内容块的 `type` 值。
const TOOL_USE_TYPE: &str = "tool_use";

/// 从消息内容块中按工具名提取 tool_use 块。
///
/// 对齐 TS `extractToolUseBlock(content, toolName)`：遍历内容块，
/// 返回第一个 `type === 'tool_use' && name === toolName` 的块；
/// 未找到返回 `None`。匹配发生在原始 JSON 上（不反序列化为具体类型），
/// 与 TS 的「先 find 再二次确认 type」完全对齐。
pub fn extract_tool_use_block(content: &[Value], tool_name: &str) -> Option<Value> {
    for block in content {
        let is_tool_use = block
            .get("type")
            .and_then(Value::as_str)
            .map(|t| t == TOOL_USE_TYPE)
            .unwrap_or(false);
        if !is_tool_use {
            continue;
        }
        let name_matches = block
            .get("name")
            .and_then(Value::as_str)
            .map(|n| n == tool_name)
            .unwrap_or(false);
        if name_matches {
            return Some(block.clone());
        }
    }
    None
}

/// 解析并校验分类器的 tool_use 响应，失败返回 `None`。
///
/// 对齐 TS `parseClassifierResponse<T>(toolUseBlock, schema)`：
/// 读取 `toolUseBlock.input`，用 schema（Rust 侧为 `T` 的 serde
/// 反序列化）做结构校验；校验失败返回 `None`，成功返回解析结果。
///
/// 注意 TS 侧 `schema.safeParse(toolUseBlock.input)` 中 `input` 是
/// `Record<string, unknown>`；Rust 侧要求 `T: DeserializeOwned`，
/// 调用方应使用与 wire 格式一致的 serde 类型（如同目录
/// `types::permissions` 中的结构体）。
pub fn parse_classifier_response<T: DeserializeOwned>(tool_use_block: &Value) -> Option<T> {
    let input = tool_use_block.get("input")?;
    serde_json::from_value::<T>(input.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct ClassifierOutput {
        decision: String,
    }

    fn tool_use(name: &str, input: Value) -> Value {
        serde_json::json!({ "type": "tool_use", "id": "u1", "name": name, "input": input })
    }

    #[test]
    fn extract_finds_matching_block() {
        let content = vec![
            serde_json::json!({ "type": "text", "text": "hi" }),
            tool_use("other", serde_json::json!({})),
            tool_use("classify_result", serde_json::json!({ "decision": "allow" })),
        ];
        let block = extract_tool_use_block(&content, "classify_result");
        assert!(block.is_some());
        let parsed: Option<ClassifierOutput> = parse_classifier_response(&block.unwrap_or(Value::Null));
        assert_eq!(
            parsed,
            Some(ClassifierOutput { decision: "allow".to_string() })
        );
    }

    #[test]
    fn extract_returns_none_when_absent() {
        let content = vec![serde_json::json!({ "type": "text", "text": "hi" })];
        assert!(extract_tool_use_block(&content, "classify_result").is_none());
    }

    #[test]
    fn parse_returns_none_on_schema_mismatch() {
        let block = tool_use("classify_result", serde_json::json!({ "unrelated": 1 }));
        let parsed: Option<ClassifierOutput> = parse_classifier_response(&block);
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_returns_none_without_input() {
        let block = serde_json::json!({ "type": "tool_use", "name": "x" });
        let parsed: Option<ClassifierOutput> = parse_classifier_response(&block);
        assert_eq!(parsed, None);
    }
}
