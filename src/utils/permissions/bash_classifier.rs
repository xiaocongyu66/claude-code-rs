//! bash_classifier.rs — 外部构建的 stub（classifier permissions 功能为 ANT-ONLY）。
//!
//! 重写自 `src/utils/permissions/bashClassifier.ts`（stub 版本）。语义对齐
//! 说明：
//! - `ClassifierResult` / `ClassifierBehavior` 已定义在
//!   `src/types/permissions.rs`，此处直接引用，不重复定义；
//! - TS 的 `async` 函数（`classifyBashCommand` /
//!   `generateGenericDescription`）依赖 AbortSignal；本 crate 无异步运行时
//!   依赖，Rust 侧改为同步签名并省略 signal 入参（stub 实现不使用它）。

use crate::types::permissions::{ClassifierBehavior, ClassifierConfidence, ClassifierResult};

/// 规则内容中 prompt 描述的前缀（stub 仍保留，供 `create_prompt_rule_content` 使用）。
pub const PROMPT_PREFIX: &str = "prompt:";

/// 从规则内容中提取 prompt 描述。Stub：恒返回 `None`。
pub fn extract_prompt_description(_rule_content: Option<&str>) -> Option<String> {
    None
}

/// 构造 prompt 规则内容：`prompt:` 前缀 + 去除首尾空白后的描述。
pub fn create_prompt_rule_content(description: &str) -> String {
    format!("{} {}", PROMPT_PREFIX, description.trim())
}

/// 分类器权限是否启用。Stub：恒 `false`。
pub fn is_classifier_permissions_enabled() -> bool {
    false
}

/// Stub：无 prompt deny 描述。
pub fn get_bash_prompt_deny_descriptions(_context: &serde_json::Value) -> Vec<String> {
    vec![]
}

/// Stub：无 prompt ask 描述。
pub fn get_bash_prompt_ask_descriptions(_context: &serde_json::Value) -> Vec<String> {
    vec![]
}

/// Stub：无 prompt allow 描述。
pub fn get_bash_prompt_allow_descriptions(_context: &serde_json::Value) -> Vec<String> {
    vec![]
}

/// 分类 bash 命令是否匹配规则。Stub：恒不匹配，confidence 直接给 high。
pub fn classify_bash_command(
    _command: &str,
    _cwd: &str,
    _descriptions: &[String],
    _behavior: ClassifierBehavior,
) -> ClassifierResult {
    ClassifierResult {
        matches: false,
        matched_description: None,
        confidence: ClassifierConfidence::High,
        reason: "This feature is disabled".to_string(),
    }
}

/// 生成通用描述。Stub：回传 `specific_description`（与 TS 的
/// `specificDescription || null` 一致：空字符串视为假值，返回 `None`）。
pub fn generate_generic_description(
    _command: &str,
    specific_description: Option<&str>,
) -> Option<String> {
    match specific_description {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}
