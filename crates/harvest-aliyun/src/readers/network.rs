//! VPC / 子网：`DescribeVpcs`、`DescribeVSwitches`（`vpc.aliyuncs.com`，`2016-04-28`）。
//!
//! 兼容常量与腾讯云一致：`manage_asset_type`/`first_asset_type` = `NET_WORK`、
//! `enable=1`、`network_type=""`、`risk_level="MEDIUM"`（不是真实风险评估结论）。
//! VPC 是根节点（`parent_source_asset_id="-1"`）；子网的父节点是所属 VPC，
//! 没有 `VpcId` 就是数据格式问题。
//!
//! 阿里云没有返回的字段一律保持 **absent**（不输出），而不是编一个默认值：
//! 下游把「字段缺席」当未知，把「空值」当已知为空，两者不能混。

use harvest_core::error::HarvestError;
use harvest_core::model::{
    COMPAT_ENABLE, COMPAT_NET_WORK, COMPAT_NETWORK_TYPE, COMPAT_RISK_LEVEL, COMPAT_VPC_ROOT_PARENT,
    Subnet, Vpc,
};
use serde_json::{Map, Value};

use super::{MapContext, required_id};
use crate::client::ApiAction;
use crate::value::{as_str, presence_bool, presence_i64, presence_str, presence_tags};

pub const DESCRIBE_VPCS: ApiAction = ApiAction {
    name: "DescribeVpcs",
    host: "vpc.aliyuncs.com",
    version: "2016-04-28",
};

pub const DESCRIBE_VSWITCHES: ApiAction = ApiAction {
    name: "DescribeVSwitches",
    host: "vpc.aliyuncs.com",
    version: "2016-04-28",
};

pub const VPC_OUTER_KEY: &str = "Vpcs";
pub const VPC_INNER_KEY: &str = "Vpc";
pub const VSWITCH_OUTER_KEY: &str = "VSwitches";
pub const VSWITCH_INNER_KEY: &str = "VSwitch";

/// 该接口 `PageSize` 上限。
pub const MAX_PAGE_SIZE: u64 = 50;

pub fn map_vpc(obj: &Map<String, Value>, ctx: &MapContext<'_>) -> Result<Vpc, HarvestError> {
    let id = required_id(obj, "VpcId", DESCRIBE_VPCS.name, ctx.region)?;
    let name = {
        let name = as_str(obj.get("VpcName"));
        if name.is_empty() { id.clone() } else { name }
    };

    Ok(Vpc {
        source_asset_id: id.clone(),
        manage_asset_type: COMPAT_NET_WORK.to_string(),
        first_asset_type: COMPAT_NET_WORK.to_string(),
        asset_type: "VPC".to_string(),
        asset_code: id,
        asset_name: name,
        parent_source_asset_id: COMPAT_VPC_ROOT_PARENT.to_string(),
        cidr: as_str(obj.get("CidrBlock")),
        cidrv6: as_str(obj.get("Ipv6CidrBlock")),
        network_type: COMPAT_NETWORK_TYPE.to_string(),
        risk_level: COMPAT_RISK_LEVEL.to_string(),
        region: ctx.region.to_string(),
        region_name: ctx.region_name.to_string(),
        enable: COMPAT_ENABLE,
        cloud_create_time: presence_str(obj, "CreationTime"),
        tags: presence_tags(obj, "Tags"),
        is_default: presence_bool(obj, "IsDefault"),
        // 以下字段阿里云 VPC 接口不返回，保持 absent。
        assistant_cidr_set: Default::default(),
        ipv6_cidr_block_set: Default::default(),
        enable_dhcp: Default::default(),
        dhcp_options_id: Default::default(),
        dns_servers: Default::default(),
        domain_name: Default::default(),
    })
}

pub fn map_vswitch(obj: &Map<String, Value>, ctx: &MapContext<'_>) -> Result<Subnet, HarvestError> {
    let id = required_id(obj, "VSwitchId", DESCRIBE_VSWITCHES.name, ctx.region)?;
    let parent = as_str(obj.get("VpcId"));
    if parent.is_empty() {
        return Err(HarvestError::data_format(format!(
            "DescribeVSwitches 响应缺少 VpcId（交换机 {id} 没有父 VPC 无法解析层级）"
        ))
        .with_action(DESCRIBE_VSWITCHES.name)
        .with_region(ctx.region));
    }
    let name = {
        let name = as_str(obj.get("VSwitchName"));
        if name.is_empty() { id.clone() } else { name }
    };

    Ok(Subnet {
        source_asset_id: id.clone(),
        manage_asset_type: COMPAT_NET_WORK.to_string(),
        first_asset_type: COMPAT_NET_WORK.to_string(),
        asset_type: "SUB_NET".to_string(),
        asset_code: id,
        asset_name: name,
        parent_source_asset_id: parent,
        cidr: as_str(obj.get("CidrBlock")),
        cidrv6: as_str(obj.get("Ipv6CidrBlock")),
        network_type: COMPAT_NETWORK_TYPE.to_string(),
        risk_level: COMPAT_RISK_LEVEL.to_string(),
        region: ctx.region.to_string(),
        region_name: ctx.region_name.to_string(),
        zone: as_str(obj.get("ZoneId")),
        enable: COMPAT_ENABLE,
        cloud_create_time: presence_str(obj, "CreationTime"),
        tags: presence_tags(obj, "Tags"),
        is_default: presence_bool(obj, "IsDefault"),
        route_table_id: presence_str(obj, "RouteTableId"),
        network_acl_id: presence_str(obj, "NetworkAclId"),
        available_ip_address_count: presence_i64(obj, "AvailableIpAddressCount"),
        // 阿里云不返回交换机总 IP 数，保持 absent。
        total_ip_address_count: Default::default(),
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
    fn vpc_root_and_compat_constants() {
        let value = json!({
            "VpcId": "vpc-example01",
            "VpcName": "production-vpc",
            "CidrBlock": "10.0.0.0/16",
            "Ipv6CidrBlock": "2408:4000::/56",
            "CreationTime": "2026-10-01T10:00:00Z",
            "IsDefault": false,
            "Tags": {"Tag": [{"TagKey": "Environment", "TagValue": "production"}]}
        });
        let vpc = map_vpc(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(vpc.parent_source_asset_id, "-1");
        assert_eq!(vpc.manage_asset_type, "NET_WORK");
        assert_eq!(vpc.first_asset_type, "NET_WORK");
        assert_eq!(vpc.asset_type, "VPC");
        assert_eq!(vpc.enable, 1);
        assert_eq!(vpc.risk_level, "MEDIUM");
        assert_eq!(vpc.network_type, "");
        assert_eq!(vpc.cidrv6, "2408:4000::/56");
        assert_eq!(vpc.is_default, Presence::Present(false));
        assert!(vpc.dns_servers.is_absent(), "阿里云不返回 DNS，保持缺席");
    }

    #[test]
    fn optional_fields_keep_tristate() {
        let value = json!({"VpcId": "vpc-1", "Tags": null});
        let vpc = map_vpc(value.as_object().unwrap(), &ctx()).unwrap();
        assert!(vpc.tags.is_null());
        assert!(
            vpc.cloud_create_time.is_absent(),
            "没有 CreationTime 就不输出"
        );
        assert!(vpc.cidr.is_empty(), "CidrBlock 缺失 → 空串");
    }

    #[test]
    fn vswitch_requires_parent_vpc() {
        let value = json!({"VSwitchId": "vsw-1"});
        let err = map_vswitch(value.as_object().unwrap(), &ctx()).unwrap_err();
        assert!(err.message.contains("VpcId"), "{}", err.message);
    }

    #[test]
    fn vswitch_fields() {
        let value = json!({
            "VSwitchId": "vsw-example01",
            "VSwitchName": "production-zone-h",
            "VpcId": "vpc-example01",
            "CidrBlock": "10.0.1.0/24",
            "ZoneId": "cn-hangzhou-h",
            "CreationTime": "2026-10-01T10:05:00Z",
            "IsDefault": false,
            "AvailableIpAddressCount": 240,
            "NetworkAclId": "acl-example01",
            "Tags": {"Tag": []}
        });
        let subnet = map_vswitch(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(subnet.parent_source_asset_id, "vpc-example01");
        assert_eq!(subnet.asset_type, "SUB_NET");
        assert_eq!(subnet.zone, "cn-hangzhou-h");
        assert_eq!(
            subnet.network_acl_id,
            Presence::Present("acl-example01".into())
        );
        assert_eq!(subnet.available_ip_address_count, Presence::Present(240));
        assert!(subnet.route_table_id.is_absent());
        assert!(subnet.total_ip_address_count.is_absent());
        assert_eq!(subnet.tags, Presence::Present(vec![]));
        assert_eq!(subnet.risk_level, "MEDIUM");
    }
}
