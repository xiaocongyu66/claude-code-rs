//! yaml.rs — YAML 解析包装（重写自 `src/utils/yaml.ts`）。
//!
//! TS 端优先用 Bun 内建 `Bun.YAML.parse`（Node 环境回退 `yaml` npm 包）；
//! Rust 端无此分叉必要，直接用 serde_yaml。
//!
//! 语义对齐说明：
//! - TS 返回 `unknown`（任意 YAML 标量/集合）；Rust 端反序列化为
//!   `serde_json::Value`（serde_json::Value 可从任意 serde 兼容格式
//!   反序列化，YAML 的 tag/锚点等扩展无对应物时 serde_yaml 自行报错）。
//! - TS 抛异常 → Rust 返回 `Result`。
//! - 空文档：Bun/YAML 包返回 `null`；serde_yaml 对空串返回
//!   `Value::Null`，一致。

use serde_json::Value;

/// 解析 YAML 文档（对齐 TS `parseYaml(input): unknown`）。
pub fn parse_yaml(input: &str) -> Result<Value, serde_yaml::Error> {
    serde_yaml::from_str::<Value>(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mapping() {
        let v = parse_yaml("a: 1\nb: hello\n").unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], "hello");
    }

    #[test]
    fn parses_nested_and_lists() {
        let v = parse_yaml("list:\n  - x: 1\n  - x: 2\n").unwrap();
        assert_eq!(v["list"][0]["x"], 1);
        assert_eq!(v["list"][1]["x"], 2);
    }

    #[test]
    fn empty_document_is_null() {
        assert_eq!(parse_yaml("").unwrap(), Value::Null);
    }

    #[test]
    fn invalid_yaml_errors() {
        assert!(parse_yaml("a: [unclosed\n").is_err());
    }
}
