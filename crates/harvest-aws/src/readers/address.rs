//! 弹性公网 IP：`ec2:DescribeAddresses`。
//!
//! 与腾讯云不同：AWS 只有一路（没有 `wanip`/`all` 两道），也不返回带宽。
//! 因此 `eip_bandwidth` 保持 0（未知），不编造。

use harvest_core::error::HarvestError;
use harvest_core::model::Eip;
use serde::Deserialize;

use super::{TagSet, name_from_tags, parse_xml, text};
use crate::client::DESCRIBE_ADDRESSES;

#[derive(Debug, Default, Deserialize)]
pub struct DescribeAddressesResponse {
    #[serde(rename = "addressesSet", default)]
    addresses_set: AddressesSet,
}

#[derive(Debug, Default, Deserialize)]
struct AddressesSet {
    #[serde(rename = "item", default)]
    items: Vec<Address>,
}

#[derive(Debug, Default, Deserialize)]
struct Address {
    #[serde(rename = "publicIp", default)]
    public_ip: Option<String>,
    #[serde(rename = "allocationId", default)]
    allocation_id: Option<String>,
    #[serde(rename = "domain", default)]
    domain: Option<String>,
    #[serde(rename = "instanceId", default)]
    instance_id: Option<String>,
    #[serde(rename = "associationId", default)]
    association_id: Option<String>,
    #[serde(rename = "networkInterfaceId", default)]
    network_interface_id: Option<String>,
    #[serde(rename = "tagSet", default)]
    tag_set: Option<TagSet>,
}

pub fn map_addresses(xml: &str, region: &str) -> Result<Vec<Eip>, HarvestError> {
    let parsed: DescribeAddressesResponse = parse_xml(xml, DESCRIBE_ADDRESSES.name)?;
    let mut out = Vec::new();
    for address in parsed.addresses_set.items {
        out.push(map_address(&address, region)?);
    }
    Ok(out)
}

fn map_address(address: &Address, region: &str) -> Result<Eip, HarvestError> {
    let public_ip = text(address.public_ip.clone());
    let allocation_id = text(address.allocation_id.clone());
    // VPC 里的 EIP 有 AllocationId；EC2-Classic 只有 PublicIp。两者都没有才是坏数据。
    let id = if !allocation_id.is_empty() {
        allocation_id
    } else {
        public_ip.clone()
    };
    if id.is_empty() {
        return Err(
            HarvestError::data_format("DescribeAddresses 响应缺少 allocationId/publicIp")
                .with_action(DESCRIBE_ADDRESSES.name)
                .with_region(region),
        );
    }

    let instance_id = text(address.instance_id.clone());
    let interface_id = text(address.network_interface_id.clone());
    let association_id = text(address.association_id.clone());
    let bound = !association_id.is_empty() || !instance_id.is_empty() || !interface_id.is_empty();

    let domain = text(address.domain.clone());
    let address_type = match domain.as_str() {
        "vpc" => "EIP",
        "standard" => "CLASSIC",
        _ => "",
    };
    let (instance_type, instance) = if !instance_id.is_empty() {
        ("EC2", instance_id)
    } else if !interface_id.is_empty() {
        ("NETWORK_INTERFACE", interface_id)
    } else {
        ("", String::new())
    };

    let tag_items = address.tag_set.as_ref().map(|set| set.items.clone());
    let name = name_from_tags(tag_items.as_ref(), &id);

    Ok(Eip {
        source_asset_id: id,
        asset_name: name,
        eip_address_type: address_type.to_string(),
        eip_address_ip: public_ip,
        eip_address_status: if bound { "BIND" } else { "UNBIND" }.to_string(),
        eip_instance_type: instance_type.to_string(),
        eip_instance_id: instance,
        // AWS 的 EIP 走 BGP，没有“线路供应商”这个可选档。
        eip_provider: "BGP".to_string(),
        // 带宽是 EIP 之外的属性（实例/带宽包），这里不编造。
        eip_bandwidth: 0,
        eip_charge_type: String::new(),
        deadline_date: String::new(),
        region: region.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> &'static str {
        r#"<DescribeAddressesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-1</requestId>
  <addressesSet>
    <item>
      <publicIp>203.0.113.10</publicIp>
      <allocationId>eipalloc-1</allocationId>
      <domain>vpc</domain>
      <instanceId>i-0abc</instanceId>
      <associationId>eipassoc-1</associationId>
      <tagSet><item><key>Name</key><value>nat-eip</value></item></tagSet>
    </item>
    <item>
      <publicIp>203.0.113.11</publicIp>
      <allocationId>eipalloc-2</allocationId>
      <domain>vpc</domain>
    </item>
  </addressesSet>
</DescribeAddressesResponse>"#
    }

    #[test]
    fn maps_bound_and_unbound_addresses() {
        let eips = map_addresses(fixture(), "us-east-1").unwrap();
        assert_eq!(eips.len(), 2);
        assert_eq!(eips[0].source_asset_id, "eipalloc-1");
        assert_eq!(eips[0].asset_name, "nat-eip");
        assert_eq!(eips[0].eip_address_type, "EIP");
        assert_eq!(eips[0].eip_address_status, "BIND");
        assert_eq!(eips[0].eip_instance_type, "EC2");
        assert_eq!(eips[0].eip_instance_id, "i-0abc");
        assert_eq!(eips[0].eip_bandwidth, 0, "AWS 不提供带宽，不编造");

        assert_eq!(eips[1].eip_address_status, "UNBIND");
        assert_eq!(eips[1].eip_instance_type, "");
    }
}
