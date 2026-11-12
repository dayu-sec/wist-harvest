//! 云主机：`DescribeInstances`（`cvm.tencentcloudapi.com`，`2017-03-12`）。
//!
//! 字段映射严格照交接文档 §3.2：`ip` 取第一个私网地址、`public_ip` 取第一个
//! 公网地址、`vpcid`/`subvpcid` 保留既有命名、`tasks` 三态。

use harvest_core::error::HarvestError;
use harvest_core::model::CloudHost;
use serde_json::{Map, Value};

use super::{MapContext, charge_type_name, required_id};
use crate::client::ApiAction;
use crate::value::{as_i64, as_object, as_str, as_str_vec, presence_tags};

pub const DESCRIBE_INSTANCES: ApiAction = ApiAction {
    name: "DescribeInstances",
    host: "cvm.tencentcloudapi.com",
    version: "2017-03-12",
    service: "cvm",
};

pub const SET_KEY: &str = "InstanceSet";

pub fn map_instance(
    obj: &Map<String, Value>,
    ctx: &MapContext<'_>,
) -> Result<CloudHost, HarvestError> {
    let id = required_id(obj, "InstanceId", DESCRIBE_INSTANCES.name, ctx.region)?;
    let name = {
        let name = as_str(obj.get("InstanceName"));
        if name.is_empty() { id.clone() } else { name }
    };
    let state = as_str(obj.get("InstanceState"));

    let private_ips = as_str_vec(obj.get("PrivateIpAddresses"));
    let public_ips = as_str_vec(obj.get("PublicIpAddresses"));
    let ip = private_ips.first().cloned().unwrap_or_default();
    let public_ip = public_ips.first().cloned().unwrap_or_default();

    let placement = as_object(obj.get("Placement"));
    let zone = placement.map(|p| as_str(p.get("Zone"))).unwrap_or_default();
    let project_id = placement
        .map(|p| {
            let raw = as_str(p.get("ProjectId"));
            if raw.is_empty() {
                // 当前缺失默认 "0"（交接文档 §3.2），但不得当成业务 ID。
                "0".to_string()
            } else {
                raw
            }
        })
        .unwrap_or_else(|| "0".to_string());
    let project_name = ctx.projects.get(&project_id).cloned().unwrap_or_default();

    let vpc = as_object(obj.get("VirtualPrivateCloud"));
    let vpcid = vpc.map(|v| as_str(v.get("VpcId"))).unwrap_or_default();
    let subvpcid = vpc.map(|v| as_str(v.get("SubnetId"))).unwrap_or_default();

    let charge_type = as_str(obj.get("InstanceChargeType"));

    Ok(CloudHost {
        host_id: id.clone(),
        host_name: name.clone(),
        source_asset_id: id.clone(),
        asset_code: id.clone(),
        asset_name: name,
        asset_status: state.clone(),
        host_state: state,
        restrict_state: as_str(obj.get("RestrictState")),
        ip,
        public_ip,
        private_ips,
        public_ips,
        ipv6_addresses: as_str_vec(obj.get("IPv6Addresses")),
        security_group_ids: as_str_vec(obj.get("SecurityGroupIds")),
        tags: presence_tags(obj, "Tags"),
        cpu_core_num: as_i64(obj.get("CPU")).unwrap_or(0),
        memory_capacity: as_i64(obj.get("Memory")).unwrap_or(0),
        os_name: as_str(obj.get("OsName")),
        ins_type: as_str(obj.get("InstanceType")),
        ins_charge_type_name: charge_type_name(&charge_type),
        ins_charge_type: charge_type,
        version: harvest_core::model::COMPAT_VERSION.to_string(),
        create_time: as_str(obj.get("CreatedTime")),
        // 未知到期时间为空，不推算。
        expired_time: as_str(obj.get("ExpiredTime")),
        region: ctx.region.to_string(),
        region_name: ctx.region_name.to_string(),
        zone,
        vpcid,
        subvpcid,
        project_id,
        project_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvest_core::model::Presence;
    use serde_json::json;

    fn ctx() -> MapContext<'static> {
        MapContext {
            region: "ap-guangzhou",
            region_name: "华南地区(广州)",
            projects: &EMPTY,
        }
    }

    static EMPTY: std::sync::LazyLock<std::collections::BTreeMap<String, String>> =
        std::sync::LazyLock::new(std::collections::BTreeMap::new);

    #[test]
    fn maps_legacy_field_names() {
        let value = json!({
            "InstanceId": "ins-example01",
            "InstanceName": "",
            "InstanceState": "RUNNING",
            "RestrictState": "NORMAL",
            "PrivateIpAddresses": ["10.0.1.10", "10.0.1.11"],
            "PublicIpAddresses": [],
            "IPv6Addresses": [],
            "SecurityGroupIds": ["sg-example01"],
            "CPU": 4,
            "Memory": 8,
            "OsName": "Ubuntu 22.04 LTS",
            "InstanceType": "S5.LARGE8",
            "InstanceChargeType": "POSTPAID_BY_HOUR",
            "CreatedTime": "2026-11-01T02:00:00Z",
            "Placement": {"Zone": "ap-guangzhou-6"},
            "VirtualPrivateCloud": {"VpcId": "vpc-example01", "SubnetId": "subnet-example01"}
        });
        let host = map_instance(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.source_asset_id, "ins-example01");
        assert_eq!(host.asset_name, "ins-example01", "空名称回退实例 ID");
        assert_eq!(host.ip, "10.0.1.10");
        assert_eq!(host.private_ips.len(), 2);
        assert_eq!(host.public_ip, "", "没有公网地址时单值为空串");
        assert_eq!(host.vpcid, "vpc-example01");
        assert_eq!(host.subvpcid, "subnet-example01");
        assert_eq!(host.project_id, "0");
        assert_eq!(host.project_name, "");
        assert_eq!(host.version, "1");
        assert_eq!(host.ins_charge_type_name, "按小时后付费");
        assert_eq!(host.expired_time, "", "未知到期时间为空");
        assert!(host.tags.is_absent(), "云响应没有 Tags 时省略字段");
        assert_eq!(host.region_name, "华南地区(广州)");
    }

    #[test]
    fn tags_null_vs_empty_are_distinct() {
        let base = json!({"InstanceId": "ins-1", "Tags": null});
        let host = map_instance(base.as_object().unwrap(), &ctx()).unwrap();
        assert!(host.tags.is_null());

        let empty = json!({"InstanceId": "ins-1", "Tags": []});
        let host = map_instance(empty.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.tags, Presence::Present(vec![]));
    }

    #[test]
    fn missing_instance_id_is_data_format_error() {
        let value = json!({"InstanceName": "no-id"});
        let err = map_instance(value.as_object().unwrap(), &ctx()).unwrap_err();
        assert_eq!(err.class, harvest_core::ErrorClass::DataFormat);
    }
}
