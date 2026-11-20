//! tagged_id.rs — 与 API 端 tagged_id.py 格式兼容的带标签 ID 编码。
//!
//! 重写自 `src/utils/taggedId.ts`。把 UUID 字符串编码为
//! `{tag}_{version}{base58(uuid_as_128bit_int)}`，
//! 例如 "user_01PaGUP2rbg1XDh7Z9W1CEpd"。
//!
//! 必须与 api/api/common/utils/tagged_id.py 保持同步。

use thiserror::Error;

const BASE_58_CHARS: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const VERSION: &str = "01";
// ceil(128 / log2(58)) = 22
const ENCODED_LENGTH: usize = 22;

#[derive(Debug, Error)]
pub enum TaggedIdError {
    /// UUID 去掉连字符后的十六进制长度不为 32。
    #[error("Invalid UUID hex length: {0}")]
    InvalidUuidHexLength(usize),
    /// 十六进制串不是合法的 128 位数值。
    #[error("Invalid UUID hex: {0}")]
    InvalidUuidHex(String),
}

/// 把 128 位无符号整数编码为定长 base58 字符串。
fn base58_encode(n: u128) -> String {
    let base: u128 = BASE_58_CHARS.len() as u128;
    let mut result = ['1'; ENCODED_LENGTH]; // BASE_58_CHARS[0] == '1'
    let mut index = ENCODED_LENGTH - 1;
    let mut value = n;
    while value > 0 {
        let rem = (value % base) as usize;
        // BASE_58_CHARS 全为 ASCII，字节索引取字符安全
        result[index] = BASE_58_CHARS.as_bytes()[rem] as char;
        value /= base;
        if index == 0 {
            // u128 至多占满 22 位 base58；数学上不会出现仍有余数的情况，
            // 此处仅防御性中止，对齐 TS 负索引写入被忽略的行为
            break;
        }
        index -= 1;
    }
    result.iter().collect()
}

/// 解析 UUID 字符串（带或不带连字符）为 128 位整数。
fn uuid_to_big_int(uuid: &str) -> Result<u128, TaggedIdError> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(TaggedIdError::InvalidUuidHexLength(hex.len()));
    }
    u128::from_str_radix(&hex, 16).map_err(|_| TaggedIdError::InvalidUuidHex(hex))
}

/// 把账户 UUID 转换为 API 端的 tagged ID 格式。
///
/// - `tag`：标签前缀（如 "user"、"org"）
/// - `uuid`：UUID 字符串（带或不带连字符）
///
/// 返回如 "user_01PaGUP2rbg1XDh7Z9W1CEpd"；UUID 非法时返回
/// `TaggedIdError`（对齐 TS 抛出的 Error）。
pub fn to_tagged_id(tag: &str, uuid: &str) -> Result<String, TaggedIdError> {
    let n = uuid_to_big_int(uuid)?;
    Ok(format!("{tag}_{VERSION}{}", base58_encode(n)))
}

#[cfg(test)]
mod tests {
    use super::to_tagged_id;

    #[test]
    fn zero_uuid_encodes_to_all_ones() {
        // base58(0) = 22 × '1'（BASE_58_CHARS[0]）
        let id = to_tagged_id("user", "00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(id, "user_011111111111111111111111");
    }

    #[test]
    fn encodes_uuid_with_and_without_dashes_identically() {
        let with_dashes = to_tagged_id("org", "01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let without_dashes = to_tagged_id("org", "0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(with_dashes, without_dashes);
        assert_eq!(with_dashes.len(), "org_01".len() + 22);
    }

    #[test]
    fn rejects_invalid_uuid() {
        assert!(to_tagged_id("user", "not-a-uuid").is_err());
        // 31 个十六进制字符
        assert!(to_tagged_id("user", "0123456789abcdef0123456789abcde").is_err());
    }
}
