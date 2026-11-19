//! 阿里云响应体的取值助手。
//!
//! 阿里云 RPC 响应普遍把数组包在对象里（`{"Instances":{"Instance":[…]}}`），
//! 标签用 `TagKey`/`TagValue`，IP 列表是 `{"IpAddress":[…]}`。这些形状差异集中在
//! 这里，映射函数只关心字段名。
//!
//! 与腾讯云一样：缺失 / `null` / 空数组要能区分，不能把「字段没有」当成「值为空」。

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
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Some(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Some(false),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

/// 字符串数组。非数组一律当空数组，但调用方必须确保「请求失败」不会走到这里。
///
/// 阿里云在部分接口上会把单元素列表压成标量，所以字符串/数字也按一元列表接受。
pub fn as_str_vec(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| !item.is_null())
            .map(|item| as_str(Some(item)))
            .collect(),
        Some(Value::String(_)) | Some(Value::Number(_)) | Some(Value::Bool(_)) => {
            vec![as_str(value)]
        }
        _ => Vec::new(),
    }
}

pub fn as_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// `{"Outer":{"Inner":[…]}}` → 内层列表。
///
/// 阿里云在部分接口上会把单元素数组压成对象，所以单对象也按一元列表接受；
/// 这不改变语义（还是一个元素），但避免了「账号里只有一个资源就采不到」。
pub fn inner_items<'a>(
    obj: &'a Map<String, Value>,
    outer: &str,
    inner: &str,
) -> Option<Vec<&'a Value>> {
    let value = obj.get(outer)?.as_object()?.get(inner)?;
    match value {
        Value::Array(items) => Some(items.iter().collect()),
        Value::Null => None,
        single => Some(vec![single]),
    }
}

/// IP / 安全组这类「字符串列表包一层」的字段。
pub fn wrapped_str_vec(obj: &Map<String, Value>, outer: &str, inner: &str) -> Vec<String> {
    match obj
        .get(outer)
        .and_then(Value::as_object)
        .and_then(|wrapper| wrapper.get(inner))
    {
        Some(Value::Array(items)) => as_str_vec(Some(&Value::Array(items.clone()))),
        // 单元素被压成标量时也当成一元列表。
        Some(value @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => {
            vec![as_str(Some(value))]
        }
        _ => Vec::new(),
    }
}

/// 既可能是裸数组、也可能是 `{"Inner":[…]}` 的字段（IPv6 地址）。
pub fn list_str_vec(obj: &Map<String, Value>, key: &str, inner: &str) -> Vec<String> {
    match obj.get(key) {
        Some(Value::Array(items)) => as_str_vec(Some(&Value::Array(items.clone()))),
        Some(Value::Object(_)) => wrapped_str_vec(obj, key, inner),
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

/// 原样保留数组结构（用于需要对下游暴露云原始结构的字段）。
pub fn presence_array(map: &Map<String, Value>, key: &str) -> Presence<Vec<Value>> {
    match map.get(key) {
        None => Presence::Absent,
        Some(Value::Null) => Presence::Null,
        Some(Value::Array(items)) => Presence::Present(items.clone()),
        Some(_) => Presence::Present(Vec::new()),
    }
}

/// 标签三态：`{"Tags":{"Tag":[{"TagKey":"k","TagValue":"v"}]}}`。
pub fn presence_tags(map: &Map<String, Value>, key: &str) -> Presence<Vec<Tag>> {
    let value = match map.get(key) {
        None => return Presence::Absent,
        Some(Value::Null) => return Presence::Null,
        Some(value) => value,
    };
    // 有的接口直接给 `Tag` 数组，有的包一层 `Tags.Tag`；单元素也可能被压成对象。
    let items = match value {
        Value::Array(items) => items.clone(),
        Value::Object(wrapper) => match wrapper.get("Tag") {
            Some(Value::Null) | None => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(Value::Object(tag)) => vec![Value::Object(tag.clone())],
            Some(_) => Vec::new(),
        },
        _ => Vec::new(),
    };
    Presence::Present(
        items
            .iter()
            .filter_map(Value::as_object)
            .map(|obj| Tag {
                key: as_str(obj.get("TagKey")),
                value: as_str(obj.get("TagValue")),
            })
            .collect(),
    )
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

        let empty = map(json!({"Tags": {"Tag": []}}));
        assert_eq!(presence_tags(&empty, "Tags"), Presence::Present(vec![]));

        let filled = map(json!({"Tags": {"Tag": [
            {"TagKey": "App", "TagValue": "order"}
        ]}}));
        assert_eq!(
            presence_tags(&filled, "Tags").value().unwrap(),
            &vec![Tag {
                key: "App".into(),
                value: "order".into()
            }]
        );
    }

    #[test]
    fn wrapped_lists() {
        let value = map(json!({
            "PrivateIpAddress": {"IpAddress": ["10.0.1.10", "10.0.1.11"]},
            "SecurityGroupIds": {"SecurityGroupId": ["sg-1"]},
            "Ipv6Address": ["2408::1"]
        }));
        assert_eq!(
            wrapped_str_vec(&value, "PrivateIpAddress", "IpAddress"),
            vec!["10.0.1.10", "10.0.1.11"]
        );
        assert_eq!(
            wrapped_str_vec(&value, "SecurityGroupIds", "SecurityGroupId"),
            vec!["sg-1"]
        );
        assert_eq!(
            list_str_vec(&value, "Ipv6Address", "IpAddress"),
            vec!["2408::1"]
        );
    }

    #[test]
    fn single_element_lists_may_be_flattened() {
        // 阿里云部分接口把单元素数组压成对象/标量；两者都是「一个元素」。
        let value = map(json!({
            "Instances": {"Instance": {"InstanceId": "i-1"}},
            "Tags": {"Tag": {"TagKey": "App", "TagValue": "order"}},
            "PrivateIpAddress": {"IpAddress": "10.0.0.1"}
        }));
        assert_eq!(
            inner_items(&value, "Instances", "Instance").unwrap().len(),
            1
        );
        assert_eq!(
            presence_tags(&value, "Tags").value().unwrap(),
            &vec![Tag {
                key: "App".into(),
                value: "order".into()
            }]
        );
        assert_eq!(
            wrapped_str_vec(&value, "PrivateIpAddress", "IpAddress"),
            vec!["10.0.0.1"]
        );
    }

    #[test]
    fn numeric_as_string() {
        assert_eq!(as_str(Some(&json!(1001))), "1001");
        assert_eq!(as_str(Some(&json!(null))), "");
        assert_eq!(as_str(None), "");
    }

    #[test]
    fn bool_forms() {
        assert_eq!(as_bool(Some(&json!("true"))), Some(true));
        assert_eq!(as_bool(Some(&json!(false))), Some(false));
        assert_eq!(as_bool(Some(&json!(null))), None);
    }
}
