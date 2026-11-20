//! alias_expansion.rs — session command alias expansion: runtime hook point.
//!
//! 重写自 `src/hooks/aliasExpansion.ts`。W3 task #54：
//! `apply_aliases_to_input_text` 接入真实输入管线——
//! `processUserInputBase`（src/utils/processUserInput/processUserInput.ts）
//! 在 `processSlashCommand` 之前立即对 slash 输入应用别名，使
//! `/<alias> ...` 在 slash 分发之前转发到目标命令。
//!
//! 存储：`<get_claude_config_home_dir()>/aliases.json`——扁平 map，
//! alias name -> { target, created }。target 是不含前导斜杠的命令行，
//! 例如 alias "ci" -> "prompt:review the CI config"。
//!
//! 语义对齐：
//! - `load_alias_map`：文件缺失 -> 空 map；解析成功但形状不是 alias map
//!   -> 调用 `reject_corrupt_json_file` 隔离并返回 StoreCorruptError
//!   （绝不把腐蚀文件静默当空文件，随后被 read-modify-write 覆盖）。
//! - `expand_message`：纯函数，改写 `/x rest` -> `/<target> rest`。
//!
//! 已知差异：
//! - TS `readJsonFileStrict`/`rejectCorruptJsonFile` 抛异常；Rust 端
//!   `read_json_file_strict` 返回 `Result`、`reject_corrupt_json_file`
//!   返回错误值，本模块统一汇入 `AliasStoreError` 向上传播。
//! - TS 的 `AliasEntry.created` 类型声明为 string，但运行时只校验
//!   `target`；手工编辑的文件里 `created` 可能是任意 JSON。为保持
//!   load-modify-save 往返不丢数据，Rust 端用 `serde_json::Value`
//!   原样保存 `created`（写回时逐位保持）。
//! - TS `trimmed.search(/\s/)` 匹配 ASCII 空白；Rust 端用
//!   `char::is_whitespace`，覆盖 Unicode 空白（超集，CLI 输入场景无差）。

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{SecondsFormat, Utc};

use crate::utils::env_utils::get_claude_config_home_dir;
use crate::utils::safe_json_file::{
    read_json_file_strict, reject_corrupt_json_file, write_json_file_atomic, StoreCorruptError,
};

/// alias name -> { target, created }
pub type AliasMap = HashMap<String, AliasEntry>;

/// 一条别名：`target` 为目标命令行（无前导斜杠），`created` 为创建时间。
///
/// `created` 保留为原始 JSON 值（见模块头已知差异）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasEntry {
    pub target: String,
    pub created: serde_json::Value,
}

/// alias 存储的读/写错误：腐蚀（已隔离备份）与 IO 故障的并集。
#[derive(Debug, thiserror::Error)]
pub enum AliasStoreError {
    #[error(transparent)]
    Corrupt(#[from] StoreCorruptError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// `<getClaudeConfigHomeDir()>/aliases.json`
pub fn get_alias_store_path() -> PathBuf {
    PathBuf::from(get_claude_config_home_dir()).join("aliases.json")
}

/// 形状校验（对齐 TS `isAliasMap`）：必须是对象（非数组、非 null），
/// 且每个值都是带字符串 `target` 的对象。`created` 不参与校验。
fn is_alias_map(value: &serde_json::Value) -> bool {
    let serde_json::Value::Object(map) = value else {
        return false;
    };
    for entry in map.values() {
        let serde_json::Value::Object(fields) = entry else {
            return false;
        };
        match fields.get("target") {
            Some(serde_json::Value::String(_)) => {}
            _ => return false,
        }
    }
    true
}

/// 加载 alias map。文件缺失时返回空 map；腐蚀文件被隔离到
/// `.corrupt-<ts>` 备份并返回 StoreCorruptError——绝不静默当空文件。
pub fn load_alias_map() -> Result<AliasMap, AliasStoreError> {
    let path = get_alias_store_path();
    let parsed = match read_json_file_strict(&path)? {
        None => return Ok(HashMap::new()),
        Some(value) => value,
    };
    if !is_alias_map(&parsed) {
        return Err(reject_corrupt_json_file(&path, "not an alias map").into());
    }
    let serde_json::Value::Object(map) = parsed else {
        return Err(reject_corrupt_json_file(&path, "not an alias map").into());
    };
    let mut result: AliasMap = HashMap::with_capacity(map.len());
    for (name, entry) in map {
        let serde_json::Value::Object(fields) = entry else {
            return Err(reject_corrupt_json_file(&path, "not an alias map").into());
        };
        // is_alias_map 已保证 target 为字符串；这里直接拆包。
        let target = match fields.get("target") {
            Some(serde_json::Value::String(target)) => target.clone(),
            _ => {
                return Err(reject_corrupt_json_file(&path, "not an alias map").into());
            }
        };
        let created = fields.get("created").cloned().unwrap_or(serde_json::Value::Null);
        result.insert(name, AliasEntry { target, created });
    }
    Ok(result)
}

/// 原子写整个 alias map（tmp + rename，崩溃不留半写文件）。
pub fn save_alias_map(map: &AliasMap) -> Result<(), AliasStoreError> {
    let mut object = serde_json::Map::with_capacity(map.len());
    for (name, entry) in map {
        let mut fields = serde_json::Map::new();
        fields.insert("target".to_string(), serde_json::Value::String(entry.target.clone()));
        fields.insert("created".to_string(), entry.created.clone());
        object.insert(name.clone(), serde_json::Value::Object(fields));
    }
    let value = serde_json::Value::Object(object);
    write_json_file_atomic(&get_alias_store_path(), &value)?;
    Ok(())
}

/// 定义（或覆盖）一条别名。`created` 为当前 UTC 时间的 ISO 字符串
/// （对齐 JS `new Date().toISOString()`：`YYYY-MM-DDTHH:mm:ss.sssZ`）。
pub fn define_alias(name: &str, target: &str) -> Result<(), AliasStoreError> {
    let mut map = load_alias_map()?;
    let created = serde_json::Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
    map.insert(name.to_string(), AliasEntry {
        target: target.to_string(),
        created,
    });
    save_alias_map(&map)
}

/// 删除一条别名。不存在时返回 false 且不写文件。
pub fn remove_alias(name: &str) -> Result<bool, AliasStoreError> {
    let mut map = load_alias_map()?;
    if map.remove(name).is_none() {
        return Ok(false);
    }
    save_alias_map(&map)?;
    Ok(true)
}

/// 展开用户输入行开头的 alias token（默认从磁盘加载 map）。
///
/// Hook 点：在 slash 分发之前应用别名——包括内建命令名
/// （显式用户别名优先于内建）。
pub fn apply_aliases_to_input_text(input: &str, map: Option<&AliasMap>) -> Result<String, AliasStoreError> {
    match map {
        Some(map) => Ok(expand_message(input, map)),
        None => {
            let map = load_alias_map()?;
            Ok(expand_message(input, &map))
        }
    }
}

/// 纯展开：当 `x` 是 alias 时把 `/x rest` 重写为 `/<target> rest`。
/// 非 `/` 开头、或 head 不是已知别名时原样返回输入。
pub fn expand_message(input: &str, map: &AliasMap) -> String {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return input.to_string();
    }
    // TS `trimmed.search(/\s/)`：第一个空白字符的位置；无空白时取整个 head。
    let space_idx = trimmed.find(char::is_whitespace);
    let head = match space_idx {
        Some(idx) => &trimmed[1..idx],
        None => &trimmed[1..],
    };
    let tail = match space_idx {
        Some(idx) => &trimmed[idx..],
        None => "",
    };
    let Some(entry) = map.get(head) else {
        return input.to_string();
    };
    format!("/{}{}", entry.target, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(pairs: &[(&str, &str)]) -> AliasMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    AliasEntry {
                        target: v.to_string(),
                        created: serde_json::Value::Null,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn expand_rewrites_alias_head() {
        let map = map_of(&[("ci", "prompt:review the CI config")]);
        assert_eq!(
            expand_message("/ci please", &map),
            "/prompt:review the CI config please"
        );
    }

    #[test]
    fn expand_keeps_target_without_args() {
        let map = map_of(&[("ci", "prompt:review")]);
        assert_eq!(expand_message("/ci", &map), "/prompt:review");
    }

    #[test]
    fn expand_preserves_leading_whitespace_of_tail() {
        let map = map_of(&[("x", "y")]);
        assert_eq!(expand_message("/x\t tabbed", &map), "/y\t tabbed");
    }

    #[test]
    fn expand_non_slash_input_passthrough() {
        let map = map_of(&[("ci", "prompt:review")]);
        assert_eq!(expand_message("ci please", &map), "ci please");
    }

    #[test]
    fn expand_unknown_alias_passthrough() {
        let map = map_of(&[("ci", "prompt:review")]);
        assert_eq!(expand_message("/nope", &map), "/nope");
    }

    #[test]
    fn is_alias_map_rejects_non_object_and_bad_entries() {
        assert!(!is_alias_map(&serde_json::Value::Null));
        assert!(!is_alias_map(&serde_json::Value::Array(Vec::new())));
        assert!(!is_alias_map(&serde_json::json!({"ci": "not-an-object"})));
        assert!(!is_alias_map(&serde_json::json!({"ci": {"created": "x"}})));
        assert!(is_alias_map(&serde_json::json!({"ci": {"target": "prompt:review", "created": "x"}})));
    }
}
