//! src/constants — 全局常量与零依赖辅助函数（对应 TS 的 constants/）。
//!
//! 已转换 13 个文件；以下 10 个因依赖尚未转换的模块留给后续波次：
//! `keys.ts`、`oauth.ts`（utils/envUtils）、`outputStyles.ts`（i18n/settings）、
//! `product.ts`（bridge/sessionIdCompat）、`prompts.ts`（utils/git 等大量模块）、
//! `promptEngineeringAudit.runner.ts`（Tool.ts/prompts）、`spinnerVerbs.ts`
//! （i18n/settings）、`systemPromptSections.ts`（bootstrap/state）、`system.ts`
//! （growthbook/providers/debug）、`tools.ts`（builtin-tools 包、shellToolUtils）。

pub mod api_limits;
pub mod betas;
pub mod common;
pub mod cyber_risk_instruction;
pub mod error_ids;
pub mod figures;
pub mod files;
pub mod github_app;
pub mod messages;
pub mod query_source;
pub mod tool_limits;
pub mod turn_completion_verbs;
pub mod xml;
pub mod keys;
pub mod oauth;
pub mod system_prompt_sections;
pub mod prompts;
