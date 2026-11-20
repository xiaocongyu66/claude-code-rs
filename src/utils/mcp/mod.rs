//! src/utils/mcp — MCP 相关工具函数（elicitation 校验 / 自然语言日期解析）。
//!
//! MCP 客户端管理主体（服务器配置加载、连接生命周期等）位于
//! `services/mcp`，本目录只含客户端 UI 侧用到的两个纯工具模块。
pub mod date_time_parser;
pub mod elicitation_validation;
