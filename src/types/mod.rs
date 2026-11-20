//! src/types — 跨模块共享的类型定义（对应 TS 的 types/）。
//!
//! 已转换 13 个文件（含 generated/ 下 4 个 proto 生成文件、9 个零依赖/自包含
//! 文件）；`*.d.ts` 环境声明（global、ink-elements、ink-jsx、
//! internal-modules、react-compiler-runtime、sdk-stubs）为 TS 编译期专属，
//! 无 Rust 运行时对应，直接随转换删除。
//!
//! 以下 6 个文件因依赖尚未转换的模块留给后续波次：
//! - `command.ts` → hooks/useCanUseTool、services/compact、services/mcp/types、
//!   Tool、utils/effort、utils/ide、utils/settings、utils/theme、types/logs、
//!   types/message、types/plugin
//! - `hooks.ts` → utils/lazySchema、entrypoints/agentSdkTypes、types/message、
//!   utils/permissions、state/AppState、utils/commitAttribution
//! - `logs.ts` → utils/fileHistory、utils/toolResultStorage、
//!   services/compaction2、types/message
//! - `message.ts` → @ant/model-provider 包、builtin-tools 包
//! - `plugin.ts` → services/lsp/types、services/mcp/types、
//!   skills/bundledSkills、utils/plugins/schemas、utils/settings/types
//! - `textInputTypes.ts` → entrypoints/agentSdkTypes、utils/config、
//!   utils/imageResizer、utils/textHighlighting、types/message

pub mod connector_text;
pub mod file_suggestion;
pub mod generated;
pub mod ids;
pub mod message_queue_types;
pub mod notebook;
pub mod permissions;
pub mod status_line;
pub mod tools;
pub mod utils;
