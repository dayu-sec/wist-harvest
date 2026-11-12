//! 腾讯云响应体的取值助手。
//!
//! 云 API 的字段类型不总是文档写的那样（`ProjectId` 有时是数字、`TotalCount`
//! 有时是字符串），所以取值一律宽松，但**能区分"没有这个字段"和"字段为 null"**。

use harvest_core::model::{Presence, Tag};
use serde_json::{Map, Value};

/// 取字符串。缺失/`null` → `""`；数字/布尔 → 其字面量。
pub fn as_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

pub fn as_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(i64::from(*b)),
        _ => None,
    }
}

pub fn as_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) if s == "true" => Some(true),
        Some(Value::String(s)) if s == "false" => Some(false),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

/// 字符串数组。非数组一律当空数组（沿用既有兼容行为），但调用方必须确保
/// "请求失败"不会走到这里——失败永远不会被转成空数组。
pub fn as_str_vec(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| !item.is_null())
            .map(|item| as_str(Some(item)))
            .collect(),
        _ => Vec::new(),
    }
}

/// 三态字符串。
pub fn presence_str(map: &Map<String, Value>, key: &str) -> Presence<String> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(value) => Presence::Present(as_str(Some(value))),
    }
}

pub fn presence_bool(map: &Map<String, Value>, key: &str) -> Presence<bool> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(value) => Presence::from_option(as_bool(Some(value))),
    }
}

pub fn presence_i64(map: &Map<String, Value>, key: &str) -> Presence<i64> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(value) => Presence::from_option(as_i64(Some(value))),
    }
}

pub fn presence_str_vec(map: &Map<String, Value>, key: &str) -> Presence<Vec<String>> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(value) => Presence::Present(as_str_vec(Some(value))),
    }
}

/// 原样保留数组元素结构（`AssistantCidrSet` / `Ipv6CidrBlockSet`）。
pub fn presence_array(map: &Map<String, Value>, key: &str) -> Presence<Vec<Value>> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(Value::Array(items)) => Presence::Present(items.clone()),
        Some(_) => Presence::Present(Vec::new()),
    }
}

/// 标签三态。每项只保留 `Key`/`Value`，去掉 `tagKey`/`tagValue` 别名。
pub fn presence_tags(map: &Map<String, Value>, key: &str) -> Presence<Vec<Tag>> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(Value::Array(items)) => Presence::Present(
            items
                .iter()
                .filter_map(|item| item.as_object())
                .map(|obj| Tag {
                    key: as_str(obj.get("Key")),
                    value: as_str(obj.get("Value")),
                })
                .collect(),
        ),
        Some(_) => Presence::Present(Vec::new()),
    }
}

/// 取对象字段；不是对象返回 `None`。
pub fn as_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(|v| v.as_object())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn tags_tristate() {
        let absent = map(json!({}));
        assert!(presence_tags(&absent, "Tags").is_absent());

        let null = map(json!({"Tags": null}));
        assert!(presence_tags(&null, "Tags").is_null());

        let empty = map(json!({"Tags": []}));
        assert_eq!(presence_tags(&empty, "Tags"), Presence::Present(vec![]));

        let filled = map(json!({"Tags": [{"Key": "App", "Value": "order", "tagKey": "x"}]}));
        assert_eq!(
            presence_tags(&filled, "Tags").value().unwrap(),
            &vec![Tag {
                key: "App".into(),
                value: "order".into()
            }]
        );
    }

    #[test]
    fn numeric_as_string() {
        assert_eq!(as_str(Some(&json!(1001))), "1001");
        assert_eq!(as_str(Some(&json!(null))), "");
        assert_eq!(as_str(None), "");
    }

    #[test]
    fn string_limit_parses() {
        assert_eq!(as_i64(Some(&json!("100"))), Some(100));
        assert_eq!(as_i64(Some(&json!(100))), Some(100));
        assert_eq!(as_i64(Some(&json!(null))), None);
    }
}
