//! `tags` 三态：字段缺席 / `null` / `[]` 必须在 JSON 上真正区分。

use harvest_core::model::{CloudHost, Presence, Subnet, Tag};

fn host(tags: Presence<Vec<Tag>>) -> CloudHost {
    CloudHost {
        source_asset_id: "ins-1".into(),
        tags,
        ..Default::default()
    }
}

#[test]
fn absent_omits_the_field_entirely() {
    let json = serde_json::to_value(host(Presence::Absent)).unwrap();
    assert!(
        json.as_object().unwrap().get("tags").is_none(),
        "字段缺席时不能输出 tags: {json}"
    );
}

#[test]
fn null_keeps_null() {
    let json = serde_json::to_value(host(Presence::Null)).unwrap();
    assert!(json.as_object().unwrap().contains_key("tags"));
    assert!(json["tags"].is_null(), "{json}");
}

#[test]
fn empty_array_keeps_empty_array() {
    let json = serde_json::to_value(host(Presence::Present(vec![]))).unwrap();
    assert_eq!(json["tags"], serde_json::json!([]));
}

#[test]
fn roundtrip_preserves_all_three_states() {
    for original in [
        Presence::Absent,
        Presence::Null,
        Presence::Present(vec![]),
        Presence::Present(vec![Tag {
            key: "App".into(),
            value: "order".into(),
        }]),
    ] {
        let json = serde_json::to_string(&host(original.clone())).unwrap();
        let back: CloudHost = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags, original, "序列化往返丢失了三态: {json}");
    }
}

#[test]
fn compatibility_constants_are_emitted() {
    let json = serde_json::to_value(Subnet {
        source_asset_id: "subnet-1".into(),
        parent_source_asset_id: "vpc-1".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(json["enable"], serde_json::json!(1));
    assert_eq!(json["network_type"], serde_json::json!(""));
    assert_eq!(json["risk_level"], serde_json::json!("MEDIUM"));
    assert_eq!(json["manage_asset_type"], serde_json::json!("NET_WORK"));
    assert_eq!(json["first_asset_type"], serde_json::json!("NET_WORK"));
    assert_eq!(json["asset_type"], serde_json::json!("SUB_NET"));

    let host_json = serde_json::to_value(host(Presence::Absent)).unwrap();
    assert_eq!(host_json["version"], serde_json::json!("1"));
}
