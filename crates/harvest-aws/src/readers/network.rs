//! VPC 与子网：`ec2:DescribeVpcs` / `ec2:DescribeSubnets`。
//!
//! AWS 与腾讯云的字段差异在这里吸收：
//! - VPC 没有创建时间 → `cloud_create_time` 保持 `Absent`；
//! - 子网没有路由表/ACL/总 IP 数（要另外的接口）→ 对应字段 `Absent`，不填假值。

use harvest_core::error::HarvestError;
use harvest_core::model::{
    COMPAT_ENABLE, COMPAT_NET_WORK, COMPAT_NETWORK_TYPE, COMPAT_RISK_LEVEL, COMPAT_VPC_ROOT_PARENT,
    Presence, Subnet, Vpc,
};
use serde::{Deserialize, Serialize};

use super::{TagSet, name_from_tags, parse_xml, tags, text};
use crate::client::{DESCRIBE_SUBNETS, DESCRIBE_VPCS};

// ---------------------------------------------------------------------------
// VPC
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct DescribeVpcsResponse {
    #[serde(rename = "vpcSet", default)]
    vpc_set: VpcSet,
}

#[derive(Debug, Default, Deserialize)]
struct VpcSet {
    #[serde(rename = "item", default)]
    items: Vec<VpcItem>,
}

#[derive(Debug, Default, Deserialize)]
struct VpcItem {
    #[serde(rename = "vpcId", default)]
    vpc_id: Option<String>,
    #[serde(rename = "cidrBlock", default)]
    cidr_block: Option<String>,
    #[serde(rename = "dhcpOptionsId", default)]
    dhcp_options_id: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: Option<String>,
    #[serde(rename = "cidrBlockAssociationSet", default)]
    cidr_block_association_set: Option<CidrBlockAssociationSet>,
    #[serde(rename = "ipv6CidrBlockAssociationSet", default)]
    ipv6_cidr_block_association_set: Option<Ipv6CidrBlockAssociationSet>,
    #[serde(rename = "tagSet", default)]
    tag_set: Option<TagSet>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CidrBlockAssociationSet {
    #[serde(rename = "item", default)]
    items: Vec<CidrBlockAssociation>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CidrBlockAssociation {
    #[serde(
        rename = "associationId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    association_id: Option<String>,
    #[serde(rename = "cidrBlock", default, skip_serializing_if = "Option::is_none")]
    cidr_block: Option<String>,
    #[serde(
        rename = "cidrBlockState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    cidr_block_state: Option<BlockState>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Ipv6CidrBlockAssociationSet {
    #[serde(rename = "item", default)]
    items: Vec<Ipv6BlockAssociation>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Ipv6BlockAssociation {
    #[serde(
        rename = "associationId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    association_id: Option<String>,
    #[serde(
        rename = "ipv6CidrBlock",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    ipv6_cidr_block: Option<String>,
    #[serde(
        rename = "ipv6CidrBlockState",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    ipv6_cidr_block_state: Option<BlockState>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct BlockState {
    #[serde(rename = "state", default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

pub fn map_vpcs(xml: &str, region: &str, region_name: &str) -> Result<Vec<Vpc>, HarvestError> {
    let parsed: DescribeVpcsResponse = parse_xml(xml, DESCRIBE_VPCS.name)?;
    let mut out = Vec::new();
    for vpc in parsed.vpc_set.items {
        out.push(map_vpc(&vpc, region, region_name)?);
    }
    Ok(out)
}

fn map_vpc(item: &VpcItem, region: &str, region_name: &str) -> Result<Vpc, HarvestError> {
    let id = text(item.vpc_id.clone());
    if id.is_empty() {
        return Err(HarvestError::data_format("DescribeVpcs 响应缺少 vpcId")
            .with_action(DESCRIBE_VPCS.name)
            .with_region(region));
    }
    let tag_items = item.tag_set.as_ref().map(|set| set.items.clone());
    let name = name_from_tags(tag_items.as_ref(), &id);
    let cidrv6 = item
        .ipv6_cidr_block_association_set
        .as_ref()
        .and_then(|set| set.items.first())
        .and_then(|assoc| assoc.ipv6_cidr_block.clone())
        .unwrap_or_default();

    Ok(Vpc {
        source_asset_id: id.clone(),
        manage_asset_type: COMPAT_NET_WORK.to_string(),
        first_asset_type: COMPAT_NET_WORK.to_string(),
        asset_type: "VPC".to_string(),
        asset_code: id,
        asset_name: name,
        parent_source_asset_id: COMPAT_VPC_ROOT_PARENT.to_string(),
        cidr: text(item.cidr_block.clone()),
        cidrv6,
        network_type: COMPAT_NETWORK_TYPE.to_string(),
        risk_level: COMPAT_RISK_LEVEL.to_string(),
        region: region.to_string(),
        region_name: region_name.to_string(),
        enable: COMPAT_ENABLE,
        // AWS 不在 DescribeVpcs 里给创建时间。
        cloud_create_time: Presence::absent(),
        tags: tags(tag_items),
        is_default: presence_bool(item.is_default.as_deref()),
        assistant_cidr_set: match &item.cidr_block_association_set {
            None => Presence::absent(),
            Some(set) => Presence::present(
                set.items
                    .iter()
                    .filter_map(|assoc| serde_json::to_value(assoc).ok())
                    .collect(),
            ),
        },
        ipv6_cidr_block_set: match &item.ipv6_cidr_block_association_set {
            None => Presence::absent(),
            Some(set) => Presence::present(
                set.items
                    .iter()
                    .filter_map(|assoc| serde_json::to_value(assoc).ok())
                    .collect(),
            ),
        },
        // AWS 的 DHCP/DNS 细节要么在 DhcpOptions 里，要么根本没有。
        enable_dhcp: Presence::absent(),
        dhcp_options_id: presence_string(item.dhcp_options_id.as_deref()),
        dns_servers: Presence::absent(),
        domain_name: Presence::absent(),
    })
}

// ---------------------------------------------------------------------------
// 子网
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct DescribeSubnetsResponse {
    #[serde(rename = "subnetSet", default)]
    subnet_set: SubnetSet,
}

#[derive(Debug, Default, Deserialize)]
struct SubnetSet {
    #[serde(rename = "item", default)]
    items: Vec<SubnetItem>,
}

#[derive(Debug, Default, Deserialize)]
struct SubnetItem {
    #[serde(rename = "subnetId", default)]
    subnet_id: Option<String>,
    #[serde(rename = "vpcId", default)]
    vpc_id: Option<String>,
    #[serde(rename = "cidrBlock", default)]
    cidr_block: Option<String>,
    #[serde(rename = "ipv6CidrBlockAssociationSet", default)]
    ipv6_cidr_block_association_set: Option<Ipv6CidrBlockAssociationSet>,
    #[serde(rename = "availableIpAddressCount", default)]
    available_ip_address_count: Option<String>,
    #[serde(rename = "availabilityZone", default)]
    availability_zone: Option<String>,
    #[serde(rename = "defaultForAz", default)]
    default_for_az: Option<String>,
    #[serde(rename = "tagSet", default)]
    tag_set: Option<TagSet>,
}

pub fn map_subnets(
    xml: &str,
    region: &str,
    region_name: &str,
) -> Result<Vec<Subnet>, HarvestError> {
    let parsed: DescribeSubnetsResponse = parse_xml(xml, DESCRIBE_SUBNETS.name)?;
    let mut out = Vec::new();
    for subnet in parsed.subnet_set.items {
        out.push(map_subnet(&subnet, region, region_name)?);
    }
    Ok(out)
}

fn map_subnet(item: &SubnetItem, region: &str, region_name: &str) -> Result<Subnet, HarvestError> {
    let id = text(item.subnet_id.clone());
    if id.is_empty() {
        return Err(
            HarvestError::data_format("DescribeSubnets 响应缺少 subnetId")
                .with_action(DESCRIBE_SUBNETS.name)
                .with_region(region),
        );
    }
    let parent = text(item.vpc_id.clone());
    if parent.is_empty() {
        return Err(HarvestError::data_format(format!(
            "DescribeSubnets 响应缺少 vpcId（子网 {id} 没有父 VPC 无法解析层级）"
        ))
        .with_action(DESCRIBE_SUBNETS.name)
        .with_region(region));
    }
    let tag_items = item.tag_set.as_ref().map(|set| set.items.clone());
    let name = name_from_tags(tag_items.as_ref(), &id);
    let cidrv6 = item
        .ipv6_cidr_block_association_set
        .as_ref()
        .and_then(|set| set.items.first())
        .and_then(|assoc| assoc.ipv6_cidr_block.clone())
        .unwrap_or_default();

    Ok(Subnet {
        source_asset_id: id.clone(),
        manage_asset_type: COMPAT_NET_WORK.to_string(),
        first_asset_type: COMPAT_NET_WORK.to_string(),
        asset_type: "SUB_NET".to_string(),
        asset_code: id,
        asset_name: name,
        parent_source_asset_id: parent,
        cidr: text(item.cidr_block.clone()),
        cidrv6,
        network_type: COMPAT_NETWORK_TYPE.to_string(),
        risk_level: COMPAT_RISK_LEVEL.to_string(),
        region: region.to_string(),
        region_name: region_name.to_string(),
        zone: text(item.availability_zone.clone()),
        enable: COMPAT_ENABLE,
        cloud_create_time: Presence::absent(),
        tags: tags(tag_items),
        is_default: presence_bool(item.default_for_az.as_deref()),
        // AWS 的路由表 / NACL 不在 DescribeSubnets 里，要另外查 → 保持缺席。
        route_table_id: Presence::absent(),
        network_acl_id: Presence::absent(),
        available_ip_address_count: presence_i64(item.available_ip_address_count.as_deref()),
        total_ip_address_count: Presence::absent(),
    })
}

fn presence_bool(value: Option<&str>) -> Presence<bool> {
    match value {
        None => Presence::absent(),
        Some(raw) => match raw.trim() {
            "true" => Presence::present(true),
            "false" => Presence::present(false),
            "" => Presence::null(),
            _ => Presence::absent(),
        },
    }
}

fn presence_string(value: Option<&str>) -> Presence<String> {
    match value {
        None => Presence::absent(),
        Some("") => Presence::null(),
        Some(text) => Presence::present(text.to_string()),
    }
}

fn presence_i64(value: Option<&str>) -> Presence<i64> {
    match value {
        None => Presence::absent(),
        Some("") => Presence::null(),
        Some(text) => match text.trim().parse::<i64>() {
            Ok(parsed) => Presence::present(parsed),
            Err(_) => Presence::absent(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpc_maps_and_keeps_missing_fields_absent() {
        let xml = r#"<DescribeVpcsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <vpcSet>
    <item>
      <vpcId>vpc-1</vpcId>
      <cidrBlock>10.0.0.0/16</cidrBlock>
      <dhcpOptionsId>dopt-1</dhcpOptionsId>
      <isDefault>false</isDefault>
      <cidrBlockAssociationSet>
        <item><associationId>vpc-cidr-assoc-1</associationId><cidrBlock>10.0.0.0/16</cidrBlock><cidrBlockState><state>associated</state></cidrBlockState></item>
      </cidrBlockAssociationSet>
      <tagSet><item><key>Name</key><value>prod-vpc</value></item></tagSet>
    </item>
  </vpcSet>
</DescribeVpcsResponse>"#;
        let vpcs = map_vpcs(xml, "us-east-1", "us-east-1").unwrap();
        assert_eq!(vpcs.len(), 1);
        let vpc = &vpcs[0];
        assert_eq!(vpc.asset_name, "prod-vpc");
        assert_eq!(vpc.parent_source_asset_id, "-1");
        assert_eq!(vpc.cidr, "10.0.0.0/16");
        assert_eq!(vpc.is_default, Presence::Present(false));
        assert_eq!(vpc.dhcp_options_id, Presence::Present("dopt-1".into()));
        assert!(vpc.cloud_create_time.is_absent());
        assert!(vpc.dns_servers.is_absent());
        assert!(matches!(vpc.assistant_cidr_set, Presence::Present(_)));
    }

    #[test]
    fn subnet_requires_a_parent_vpc() {
        let xml = r#"<DescribeSubnetsResponse><subnetSet><item><subnetId>subnet-1</subnetId></item></subnetSet></DescribeSubnetsResponse>"#;
        let err = map_subnets(xml, "us-east-1", "us-east-1").unwrap_err();
        assert!(err.message.contains("vpcId"), "{}", err.message);
    }

    #[test]
    fn subnet_maps_counts_and_leaves_what_aws_does_not_give_absent() {
        let xml = r#"<DescribeSubnetsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <subnetSet>
    <item>
      <subnetId>subnet-1</subnetId>
      <vpcId>vpc-1</vpcId>
      <cidrBlock>10.0.1.0/24</cidrBlock>
      <availableIpAddressCount>251</availableIpAddressCount>
      <availabilityZone>us-east-1a</availabilityZone>
      <defaultForAz>true</defaultForAz>
      <tagSet/>
    </item>
  </subnetSet>
</DescribeSubnetsResponse>"#;
        let subnets = map_subnets(xml, "us-east-1", "us-east-1").unwrap();
        let subnet = &subnets[0];
        assert_eq!(subnet.parent_source_asset_id, "vpc-1");
        assert_eq!(subnet.available_ip_address_count, Presence::Present(251));
        assert_eq!(subnet.is_default, Presence::Present(true));
        assert!(
            subnet.route_table_id.is_absent(),
            "AWS 不在这个接口给路由表"
        );
        assert!(subnet.total_ip_address_count.is_absent());
        assert_eq!(subnet.tags, Presence::Present(vec![]));
    }
}
