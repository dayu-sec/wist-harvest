//! 云主机：`DescribeInstances`（`ecs.aliyuncs.com`，`2014-05-26`）。
//!
//! 字段归一与腾讯云对齐：`ip` 取第一个私网地址、`public_ip` 取第一个公网地址、
//! `vpcid`/`subvpcid` 保留既有命名、标签三态。
//!
//! 两个单位/命名上的差异必须显式处理：
//! - 阿里云 `Memory` 是 **MB**，而契约的 `memory_capacity` 是 **GB**（向上取整，
//!   避免 512MB 的机器变成 0）；
//! - 阿里云没有腾讯云的项目（Project）维度，`ResourceGroupId` 落到 `project_id`，
//!   `project_name` 保持空串（不为了凑字段去查 Resource Manager）。

use harvest_core::error::HarvestError;
use harvest_core::model::CloudHost;
use serde_json::{Map, Value};

use super::{MapContext, charge_type_name, required_id};
use crate::client::ApiAction;
use crate::value::{as_i64, as_object, as_str, list_str_vec, presence_tags, wrapped_str_vec};

pub const DESCRIBE_INSTANCES: ApiAction = ApiAction {
    name: "DescribeInstances",
    host: "ecs.aliyuncs.com",
    version: "2014-05-26",
};

pub const OUTER_KEY: &str = "Instances";
pub const INNER_KEY: &str = "Instance";

pub fn map_instance(
    obj: &Map<String, Value>,
    ctx: &MapContext<'_>,
) -> Result<CloudHost, HarvestError> {
    let id = required_id(obj, "InstanceId", DESCRIBE_INSTANCES.name, ctx.region)?;
    let name = {
        let name = as_str(obj.get("InstanceName"));
        if name.is_empty() { id.clone() } else { name }
    };
    let state = as_str(obj.get("Status"));

    let vpc = as_object(obj.get("VpcAttributes"));
    // 阿里云的私网地址分三处：VPC 实例在 `VpcAttributes.PrivateIpAddress`，
    // 经典网络在 `InnerIpAddress`；部分响应还会平铺一个 `PrivateIpAddress`。
    let private_ips = {
        let from_vpc = vpc
            .map(|v| wrapped_str_vec(v, "PrivateIpAddress", "IpAddress"))
            .unwrap_or_default();
        if !from_vpc.is_empty() {
            from_vpc
        } else {
            let flat = wrapped_str_vec(obj, "PrivateIpAddress", "IpAddress");
            if !flat.is_empty() {
                flat
            } else {
                wrapped_str_vec(obj, "InnerIpAddress", "IpAddress")
            }
        }
    };
    // 公网地址：`PublicIpAddress` 是 ECS 分配的公网地址，`EipAddress` 是绑定的 EIP。
    let mut public_ips = wrapped_str_vec(obj, "PublicIpAddress", "IpAddress");
    if let Some(eip) = as_object(obj.get("EipAddress")) {
        let ip = as_str(eip.get("IpAddress"));
        if !ip.is_empty() && !public_ips.contains(&ip) {
            public_ips.push(ip);
        }
    }
    let ip = private_ips.first().cloned().unwrap_or_default();
    let public_ip = public_ips.first().cloned().unwrap_or_default();

    let vpcid = vpc.map(|v| as_str(v.get("VpcId"))).unwrap_or_default();
    let subvpcid = vpc.map(|v| as_str(v.get("VSwitchId"))).unwrap_or_default();

    let charge_type = as_str(obj.get("InstanceChargeType"));
    let memory_mb = as_i64(obj.get("Memory")).unwrap_or(0).max(0);

    Ok(CloudHost {
        host_id: id.clone(),
        host_name: name.clone(),
        source_asset_id: id.clone(),
        asset_code: id.clone(),
        asset_name: name,
        asset_status: state.clone(),
        host_state: state,
        // 阿里云没有等价的「限制状态」字段，不猜。
        restrict_state: String::new(),
        ip,
        public_ip,
        private_ips,
        public_ips,
        ipv6_addresses: list_str_vec(obj, "Ipv6Address", "IpAddress"),
        security_group_ids: wrapped_str_vec(obj, "SecurityGroupIds", "SecurityGroupId"),
        tags: presence_tags(obj, "Tags"),
        cpu_core_num: as_i64(obj.get("Cpu")).unwrap_or(0),
        memory_capacity: (memory_mb + 1023) / 1024,
        os_name: as_str(obj.get("OSName")),
        ins_type: as_str(obj.get("InstanceType")),
        ins_charge_type_name: charge_type_name(&charge_type),
        ins_charge_type: charge_type,
        version: harvest_core::model::COMPAT_VERSION.to_string(),
        create_time: as_str(obj.get("CreationTime")),
        // 未知到期时间为空，不推算。
        expired_time: as_str(obj.get("ExpiredTime")),
        region: ctx.region.to_string(),
        region_name: ctx.region_name.to_string(),
        zone: as_str(obj.get("ZoneId")),
        vpcid,
        subvpcid,
        project_id: as_str(obj.get("ResourceGroupId")),
        project_name: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvest_core::model::Presence;
    use serde_json::json;

    fn ctx() -> MapContext<'static> {
        MapContext {
            region: "cn-hangzhou",
            region_name: "华东1（杭州）",
        }
    }

    #[test]
    fn maps_instance_fields_and_units() {
        let value = json!({
            "InstanceId": "i-example01",
            "InstanceName": "",
            "Status": "Running",
            "PrivateIpAddress": {"IpAddress": ["10.0.1.10", "10.0.1.11"]},
            "PublicIpAddress": {"IpAddress": []},
            "Ipv6Address": ["2408:4000::1"],
            "SecurityGroupIds": {"SecurityGroupId": ["sg-example01"]},
            "Cpu": 4,
            "Memory": 8192,
            "OSName": "Ubuntu 22.04 LTS 64位",
            "InstanceType": "ecs.g6.large",
            "InstanceChargeType": "PostPaid",
            "CreationTime": "2026-11-01T02:00:00Z",
            "ZoneId": "cn-hangzhou-h",
            "ResourceGroupId": "rg-example01",
            "Tags": {"Tag": [{"TagKey": "App", "TagValue": "order"}]},
            "VpcAttributes": {"VpcId": "vpc-example01", "VSwitchId": "vsw-example01"}
        });
        let host = map_instance(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.source_asset_id, "i-example01");
        assert_eq!(host.asset_name, "i-example01", "空名称回退实例 ID");
        assert_eq!(host.ip, "10.0.1.10");
        assert_eq!(host.private_ips.len(), 2);
        assert_eq!(host.public_ip, "", "没有公网地址时单值为空串");
        assert_eq!(host.memory_capacity, 8, "8192MB → 8GB");
        assert_eq!(host.cpu_core_num, 4);
        assert_eq!(host.vpcid, "vpc-example01");
        assert_eq!(host.subvpcid, "vsw-example01");
        assert_eq!(host.project_id, "rg-example01");
        assert_eq!(host.project_name, "");
        assert_eq!(host.version, "1");
        assert_eq!(host.ins_charge_type_name, "按量付费");
        assert_eq!(host.expired_time, "", "未知到期时间为空");
        assert_eq!(host.tags.value().unwrap()[0].key, "App");
        assert_eq!(host.region_name, "华东1（杭州）");
    }

    #[test]
    fn small_memory_rounds_up_to_one_gb() {
        let value = json!({"InstanceId": "i-1", "Memory": 512});
        let host = map_instance(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.memory_capacity, 1);
    }

    #[test]
    fn tags_null_vs_empty_are_distinct() {
        let base = json!({"InstanceId": "i-1", "Tags": null});
        let host = map_instance(base.as_object().unwrap(), &ctx()).unwrap();
        assert!(host.tags.is_null());

        let empty = json!({"InstanceId": "i-1", "Tags": {"Tag": []}});
        let host = map_instance(empty.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.tags, Presence::Present(vec![]));
    }

    #[test]
    fn vpc_private_ip_and_bound_eip_are_resolved() {
        // 真实 DescribeInstances：私网地址在 VpcAttributes 里，EIP 在 EipAddress 里。
        let value = json!({
            "InstanceId": "i-1",
            "VpcAttributes": {
                "VpcId": "vpc-1",
                "VSwitchId": "vsw-1",
                "PrivateIpAddress": {"IpAddress": ["172.16.0.2"]}
            },
            "PublicIpAddress": {"IpAddress": []},
            "EipAddress": {"IpAddress": "203.0.113.10"}
        });
        let host = map_instance(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.ip, "172.16.0.2");
        assert_eq!(host.private_ips, vec!["172.16.0.2"]);
        assert_eq!(host.public_ip, "203.0.113.10");
        assert_eq!(host.vpcid, "vpc-1");
        assert_eq!(host.subvpcid, "vsw-1");
    }

    #[test]
    fn classic_network_inner_ip_is_used_as_private_ip() {
        let value = json!({
            "InstanceId": "i-1",
            "InnerIpAddress": {"IpAddress": ["10.1.1.1"]}
        });
        let host = map_instance(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(host.ip, "10.1.1.1");
    }

    #[test]
    fn missing_instance_id_is_data_format_error() {
        let value = json!({"InstanceName": "no-id"});
        let err = map_instance(value.as_object().unwrap(), &ctx()).unwrap_err();
        assert_eq!(err.class, harvest_core::ErrorClass::DataFormat);
    }
}
