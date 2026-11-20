//! control_message_compat.rs — 控制消息键名兼容层。
//!
//! 重写自 `src/utils/controlMessageCompat.ts`。将旧版 iOS 客户端发出的
//! camelCase `requestId` 归一化为 snake_case `request_id`（旧构建缺少
//! Swift CodingKeys 映射）。snake_case 优先；原地修改并返回原对象。

use serde_json::{Map, Value};

/// 归一化 `requestId` → `request_id`（含嵌套的 `response` 字段）。
pub fn normalize_control_message_keys(obj: &mut Value) {
    if !obj.is_object() {
        return;
    }
    normalize_record(obj);
}

fn normalize_record(record: &mut Value) {
    let Some(map) = record.as_object_mut() else {
        return;
    };
    //snake_case wins; 只在 requestId 存在且 request_id 不存在时迁移
    if map.contains_key("requestId") && !map.contains_key("request_id") {
        if let Some(v) = map.remove("requestId") {
            map.insert("request_id".to_string(), v);
        }
    }
    if let Some(response) = map.get_mut("response") {
        if response.is_object() {
            normalize_record(response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn top_level_request_id_is_renamed() {
        let mut v = json!({"requestId": "abc", "type": "control_request"});
        normalize_control_message_keys(&mut v);
        assert_eq!(v, json!({"request_id": "abc", "type": "control_request"}));
    }

    #[test]
    fn snake_case_wins() {
        let mut v = json!({"requestId": "old", "request_id": "new"});
        normalize_control_message_keys(&mut v);
        assert_eq!(v, json!({"request_id": "new"}));
    }

    #[test]
    fn nested_response_is_normalized() {
        let mut v = json!({"response": {"requestId": "inner", "ok": true}});
        normalize_control_message_keys(&mut v);
        assert_eq!(v, json!({"response": {"request_id": "inner", "ok": true}}));
    }

    #[test]
    fn non_object_passes_through() {
        let mut v = json!("string");
        normalize_control_message_keys(&mut v);
        assert_eq!(v, json!("string"));
    }
}
