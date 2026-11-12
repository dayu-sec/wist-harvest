//! VPC / 子网：`DescribeVpcs`、`DescribeSubnets`（`vpc.tencentcloudapi.com`）。
//!
//! 两类都带兼容常量：`manage_asset_type`/`first_asset_type` = `NET_WORK`、
//! `enable=1`、`network_type=""`、`risk_level="MEDIUM"`（不是真实风险评估结论）。
//! VPC 是根节点，`parent_source_asset_id="-1"`；子网的父节点是所属 VPC，
//! 没有 VPC ID 就是数据格式问题。
//!
//! 可选字段保留三态：缺席不输出、`null` 保留 `null`、`[]` 保留 `[]`。

use harvest_core::error::HarvestError;
use harvest_core::model::{
    COMPAT_ENABLE, COMPAT_NET_WORK, COMPAT_NETWORK_TYPE, COMPAT_RISK_LEVEL, COMPAT_VPC_ROOT_PARENT,
    Subnet, Vpc,
};
use serde_json::{Map, Value};

use super::{MapContext, required_id};
use crate::client::ApiAction;
use crate::value::{
    as_str, presence_array, presence_bool, presence_i64, presence_str, presence_str_vec,
    presence_tags,
};

pub const DESCRIBE_VPCS: ApiAction = ApiAction {
    name: "DescribeVpcs",
    host: "vpc.tencentcloudapi.com",
    version: "2017-03-12",
    service: "vpc",
};

pub const DESCRIBE_SUBNETS: ApiAction = ApiAction {
    name: "DescribeSubnets",
    host: "vpc.tencentcloudapi.com",
    version: "2017-03-12",
    service: "vpc",
};

pub const VPC_SET_KEY: &str = "VpcSet";
pub const SUBNET_SET_KEY: &str = "SubnetSet";

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
        cloud_create_time: presence_str(obj, "CreatedTime"),
        tags: presence_tags(obj, "TagSet"),
        is_default: presence_bool(obj, "IsDefault"),
        assistant_cidr_set: presence_array(obj, "AssistantCidrSet"),
        ipv6_cidr_block_set: presence_array(obj, "Ipv6CidrBlockSet"),
        enable_dhcp: presence_bool(obj, "EnableDhcp"),
        dhcp_options_id: presence_str(obj, "DhcpOptionsId"),
        dns_servers: presence_str_vec(obj, "DnsServerSet"),
        domain_name: presence_str(obj, "DomainName"),
    })
}

pub fn map_subnet(obj: &Map<String, Value>, ctx: &MapContext<'_>) -> Result<Subnet, HarvestError> {
    let id = required_id(obj, "SubnetId", DESCRIBE_SUBNETS.name, ctx.region)?;
    let parent = as_str(obj.get("VpcId"));
    if parent.is_empty() {
        return Err(HarvestError::data_format(format!(
            "DescribeSubnets 响应缺少 VpcId（子网 {id} 没有父 VPC 无法解析层级）"
        ))
        .with_action(DESCRIBE_SUBNETS.name)
        .with_region(ctx.region));
    }
    let name = {
        let name = as_str(obj.get("SubnetName"));
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
        zone: as_str(obj.get("Zone")),
        enable: COMPAT_ENABLE,
        cloud_create_time: presence_str(obj, "CreatedTime"),
        tags: presence_tags(obj, "TagSet"),
        is_default: presence_bool(obj, "IsDefault"),
        route_table_id: presence_str(obj, "RouteTableId"),
        network_acl_id: presence_str(obj, "NetworkAclId"),
        available_ip_address_count: presence_i64(obj, "AvailableIpAddressCount"),
        total_ip_address_count: presence_i64(obj, "TotalIpAddressCount"),
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
    fn vpc_root_and_compat_constants() {
        let value = json!({
            "VpcId": "vpc-example01",
            "VpcName": "production-vpc",
            "CidrBlock": "10.0.0.0/16",
            "CreatedTime": "2026-10-01 10:00:00",
            "TagSet": [{"Key": "Environment", "Value": "production"}],
            "IsDefault": false,
            "AssistantCidrSet": [],
            "Ipv6CidrBlockSet": [],
            "EnableDhcp": true,
            "DhcpOptionsId": "dopt-example01",
            "DnsServerSet": ["10.0.0.2"],
            "DomainName": "internal.example"
        });
        let vpc = map_vpc(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(vpc.parent_source_asset_id, "-1");
        assert_eq!(vpc.manage_asset_type, "NET_WORK");
        assert_eq!(vpc.first_asset_type, "NET_WORK");
        assert_eq!(vpc.asset_type, "VPC");
        assert_eq!(vpc.enable, 1);
        assert_eq!(vpc.risk_level, "MEDIUM");
        assert_eq!(vpc.network_type, "");
        assert_eq!(vpc.is_default, Presence::Present(false));
        assert_eq!(vpc.assistant_cidr_set, Presence::Present(vec![]));
        assert_eq!(vpc.dns_servers, Presence::Present(vec!["10.0.0.2".into()]));
    }

    #[test]
    fn optional_fields_keep_tristate() {
        let value = json!({"VpcId": "vpc-1", "DomainName": null});
        let vpc = map_vpc(value.as_object().unwrap(), &ctx()).unwrap();
        assert!(vpc.domain_name.is_null());
        assert!(vpc.tags.is_absent(), "没有 TagSet 字段就不输出");
        assert!(vpc.cidrv6.is_empty(), "Ipv6CidrBlock 缺失 → 空串");
    }

    #[test]
    fn subnet_requires_parent_vpc() {
        let value = json!({"SubnetId": "subnet-1"});
        let err = map_subnet(value.as_object().unwrap(), &ctx()).unwrap_err();
        assert!(err.message.contains("VpcId"), "{}", err.message);
    }

    #[test]
    fn subnet_fields() {
        let value = json!({
            "SubnetId": "subnet-example01",
            "SubnetName": "production-zone-6",
            "VpcId": "vpc-example01",
            "CidrBlock": "10.0.1.0/24",
            "Zone": "ap-guangzhou-6",
            "CreatedTime": "2026-10-01 10:05:00",
            "TagSet": [],
            "IsDefault": false,
            "RouteTableId": "rtb-example01",
            "NetworkAclId": "acl-example01",
            "AvailableIpAddressCount": 240,
            "TotalIpAddressCount": 256
        });
        let subnet = map_subnet(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(subnet.parent_source_asset_id, "vpc-example01");
        assert_eq!(subnet.asset_type, "SUB_NET");
        assert_eq!(
            subnet.route_table_id,
            Presence::Present("rtb-example01".into())
        );
        assert_eq!(subnet.available_ip_address_count, Presence::Present(240));
        assert_eq!(subnet.tags, Presence::Present(vec![]));
        assert_eq!(subnet.risk_level, "MEDIUM");
    }
}
