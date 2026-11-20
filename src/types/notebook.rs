//! notebook.rs — Jupyter / nbformat 类型的 TS→Rust 映射。
//!
//! 重写自 `src/types/notebook.ts`。语义对齐说明：
//! - `string | string[]` 映射为 `StringOrStringArray`（untagged 枚举）；
//! - 单元输出联合（`NotebookCellOutput` / `NotebookCellSourceOutput`）使用
//!   `#[serde(tag = "output_type")]` 内部标签枚举，`output_type` 通过
//!   `output_type()` 方法访问；
//! - `NotebookDisplayData` 的 `Record<string, unknown>` 兜底映射为 flatten 的
//!   `HashMap<String, Value>`。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// JSON 中既可能是单段字符串也可能是字符串数组的值（`string | string[]`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrStringArray {
    Single(String),
    Multiple(Vec<String>),
}

impl StringOrStringArray {
    /// 按 TS 语义把多段拼接为单段文本（无分隔符）。
    pub fn concat(&self) -> String {
        match self {
            Self::Single(s) => s.clone(),
            Self::Multiple(parts) => parts.concat(),
        }
    }
}

/// Jupyter / nbformat 单元格类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotebookCellType {
    /// 可执行代码格。
    Code,
    /// 文档格。
    Markdown,
    /// 原始文本格。
    Raw,
}

/// execute_result / display_data 的 data 载荷（节选常用键）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookDisplayData {
    /// 纯文本回退表示。
    #[serde(rename = "text/plain", skip_serializing_if = "Option::is_none")]
    pub text_plain: Option<StringOrStringArray>,
    /// MIME 桶（含图片/png 等）兜底字段。
    #[serde(flatten)]
    pub other: HashMap<String, serde_json::Value>,
}

/// 原始 notebook 中的流式输出单元。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookStreamCellOutput {
    /// 流片段，可为多段拼接。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<StringOrStringArray>,
}

/// 原始 notebook 中的执行结果或展示型输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookRichCellOutput {
    /// MIME 桶（含图片/png 等）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<NotebookDisplayData>,
}

/// 原始 notebook 中的错误输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookErrorCellOutput {
    /// 异常类型名。
    pub ename: String,
    /// 异常消息。
    pub evalue: String,
    /// 栈跟踪行数组。
    pub traceback: Vec<String>,
}

/// 单元格原始输出联合（解析自 ipynb）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "output_type")]
pub enum NotebookCellOutput {
    /// stdout/stderr 流。
    #[serde(rename = "stream")]
    Stream(NotebookStreamCellOutput),
    /// 执行结果。
    #[serde(rename = "execute_result")]
    ExecuteResult(NotebookRichCellOutput),
    /// 富展示。
    #[serde(rename = "display_data")]
    DisplayData(NotebookRichCellOutput),
    /// 内核报错。
    #[serde(rename = "error")]
    Error(NotebookErrorCellOutput),
}

impl NotebookCellOutput {
    /// 对应的 `output_type` 判别值。
    pub fn output_type(&self) -> &'static str {
        match self {
            Self::Stream(_) => "stream",
            Self::ExecuteResult(_) => "execute_result",
            Self::DisplayData(_) => "display_data",
            Self::Error(_) => "error",
        }
    }
}

/// 解析前的 notebook 单元格（nbformat 子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCell {
    /// 单元格 id（nbformat>=4.5 常见）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 单元类型。
    pub cell_type: NotebookCellType,
    /// 单元源码。
    pub source: StringOrStringArray,
    /// 代码格执行计数（TS 中可为 `number | null`，此处合并为 `Option<i64>`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_count: Option<i64>,
    /// 代码格输出列表。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<NotebookCellOutput>>,
    /// 额外元数据（编辑工具会写入）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Notebook 顶层 metadata 中与语言相关的子集。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotebookMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_info: Option<NotebookLanguageInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotebookLanguageInfo {
    /// 默认内核语言名（如 python）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 磁盘上的 `.ipynb` 根结构（用于读入与增量编辑）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookContent {
    /// 主版本号（缺省按 4 处理）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbformat: Option<i64>,
    /// 次版本号（影响 id 策略等）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nbformat_minor: Option<i64>,
    /// 单元序列。
    pub cells: Vec<NotebookCell>,
    /// 文档级元数据。
    pub metadata: NotebookMetadata,
}

/// 规范化后的内联图片载荷（送入模型 image block）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookOutputImage {
    /// base64 无空白。
    pub image_data: String,
    /// MIME 子类型。
    pub media_type: NotebookImageMediaType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotebookImageMediaType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
}

/// 经 `processOutput` 规范化后的流式输出（供工具消息使用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCellSourceStreamOutput {
    /// 已截断/拼接后的文本。
    pub text: String,
}

/// 经 `processOutput` 规范化后的富输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCellSourceRichOutput {
    /// 从 text/plain 提取的正文。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 若有 image/png 或 jpeg。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<NotebookOutputImage>,
}

/// 经 `processOutput` 规范化后的错误输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCellSourceErrorOutput {
    /// 合并 ename/evalue/traceback 后的单段文本。
    pub text: String,
}

/// 送入工具链前的单元输出联合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "output_type")]
pub enum NotebookCellSourceOutput {
    #[serde(rename = "stream")]
    Stream(NotebookCellSourceStreamOutput),
    #[serde(rename = "execute_result")]
    ExecuteResult(NotebookCellSourceRichOutput),
    #[serde(rename = "display_data")]
    DisplayData(NotebookCellSourceRichOutput),
    #[serde(rename = "error")]
    Error(NotebookCellSourceErrorOutput),
}

impl NotebookCellSourceOutput {
    /// 对应的 `output_type` 判别值。
    pub fn output_type(&self) -> &'static str {
        match self {
            Self::Stream(_) => "stream",
            Self::ExecuteResult(_) => "execute_result",
            Self::DisplayData(_) => "display_data",
            Self::Error(_) => "error",
        }
    }
}

/// 送入模型工具结果的单元摘要结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCellSource {
    /// 与源 cell 对齐的类型。
    #[serde(rename = "cellType")]
    pub cell_type: NotebookCellType,
    /// 拼接后的单元源码字符串。
    pub source: String,
    /// 代码格保留执行计数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_count: Option<i64>,
    /// 稳定单元 id（无则生成 cell-{index}）。
    pub cell_id: String,
    /// 代码格语言 id（非 python 时标注）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 规范化后的输出列表。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<NotebookCellSourceOutput>>,
}
