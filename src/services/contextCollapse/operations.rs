//! `services/contextCollapse/operations.ts` — 折叠视图投影（存根）。
//!
//! CCH 未实现折叠存储，`projectView` 为恒等投影（与 TS 存根一致）。

use crate::services::compact::Message;

/// TS `projectView(messages)` — 存根：原样返回。
pub fn project_view(messages: Vec<Message>) -> Vec<Message> {
    messages
}
