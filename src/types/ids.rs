//! ids.rs — 会话与代理 ID 的 branded 类型。
//!
//! 重写自 `src/types/ids.ts`。语义对齐：TS 的 branded string（`string & {
//! __brand }`）对应 Rust newtype；`toAgentId` 的正则校验改为手写校验器
//! （Cargo.toml 暂无 regex 依赖）。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 唯一标识一个 Claude Code 会话（`getSessionId()` 产生）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// 唯一标识会话内的一个子代理（`createAgentId()` 产生）。
/// 出现该字段时表示上下文属于子代理而非主会话。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl SessionId {
    /// 将原始字符串包装为 `SessionId`。
    /// 尽量少用 —— 优先使用 `getSessionId()`。
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

impl AgentId {
    /// 将原始字符串包装为 `AgentId`。
    /// 尽量少用 —— 优先使用 `createAgentId()`。
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

impl From<String> for SessionId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<String> for AgentId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// 校验并包装 `AgentId`。
/// 匹配 `createAgentId()` 产生的格式：`a` + 可选 `<label>-` + 16 个十六进制字符。
/// 不匹配（如 teammate 名称、team-addressing）时返回 `None`。
///
/// 等价正则：`/^a(?:.+-)?[0-9a-f]{16}$/`（`.` 不匹配换行符）。
pub fn to_agent_id(s: &str) -> Option<AgentId> {
    let rest = s.strip_prefix('a')?;

    // 可选 label 段：`.+` 至少一个字符（不含换行符）并以 `-` 结尾。
    let hex_part = match rest.rfind('-') {
        Some(idx) => {
            let label = &rest[..idx];
            if label.is_empty() || label.bytes().any(|b| b == b'\n' || b == b'\r') {
                return None;
            }
            &rest[idx + 1..]
        }
        None => rest,
    };

    if hex_part.len() == 16
        && hex_part
            .bytes()
            .all(|b| (b'0'..=b'9').contains(&b) || (b'a'..=b'f').contains(&b))
    {
        Some(AgentId(s.to_string()))
    } else {
        None
    }
}
