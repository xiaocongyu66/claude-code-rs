//! system_prompt_type.rs — SystemPrompt branded 类型（兼容再导出点）。
//!
//! 重写自 `src/utils/systemPromptType.ts`。TS 版只是把
//! `@ant/model-provider` 的 `SystemPrompt`（readonly string[] 的 branded
//! 类型）与 `asSystemPrompt` 转换函数再导出。Rust 侧无该包，落为
//! 本地 newtype：
//!
//! - `SystemPrompt`：`Vec<String>` 的 branded newtype。
//! - `as_system_prompt`：从字符串切片构造（对齐 TS 的 asSystemPrompt，
//!   TS 里是同一个数组引用的类型断言，无拷贝；Rust 侧语义等价：把传入
//!   序列原样包起来）。

/// branded 的系统提示片段列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt(pub Vec<String>);

impl SystemPrompt {
    pub fn as_slices(&self) -> &[String] {
        &self.0
    }
}

impl From<Vec<String>> for SystemPrompt {
    fn from(parts: Vec<String>) -> Self {
        SystemPrompt(parts)
    }
}

/// 把字符串列表包成 SystemPrompt（对齐 TS `asSystemPrompt`）。
pub fn as_system_prompt(parts: Vec<String>) -> SystemPrompt {
    SystemPrompt(parts)
}
