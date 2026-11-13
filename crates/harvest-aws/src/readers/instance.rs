//! 云主机：`ec2:DescribeInstances`。
//!
//! 字段映射到平台既有的规范模型（`CloudHost`）。AWS 能提供什么就映射什么，
//! 提供不了的（内存、计费方式）保持 0/空，**不猜**：猜出来的数会被成本链路当真。

use harvest_core::error::HarvestError;
use harvest_core::model::{COMPAT_VERSION, CloudHost};
use serde::Deserialize;

use super::{TagSet, name_from_tags, parse_xml, tags, text};
use crate::client::DESCRIBE_INSTANCES;

#[derive(Debug, Default, Deserialize)]
pub struct DescribeInstancesResponse {
    #[serde(rename = "reservationSet", default)]
    reservation_set: ReservationSet,
}

#[derive(Debug, Default, Deserialize)]
struct ReservationSet {
    #[serde(rename = "item", default)]
    items: Vec<Reservation>,
}

#[derive(Debug, Default, Deserialize)]
struct Reservation {
    #[serde(rename = "instancesSet", default)]
    instances: InstanceSet,
}

#[derive(Debug, Default, Deserialize)]
struct InstanceSet {
    #[serde(rename = "item", default)]
    items: Vec<Instance>,
}

#[derive(Debug, Default, Deserialize)]
struct Instance {
    #[serde(rename = "instanceId", default)]
    instance_id: Option<String>,
    #[serde(rename = "instanceType", default)]
    instance_type: Option<String>,
    #[serde(rename = "instanceState", default)]
    state: Option<InstanceState>,
    #[serde(rename = "launchTime", default)]
    launch_time: Option<String>,
    #[serde(rename = "placement", default)]
    placement: Option<Placement>,
    #[serde(rename = "subnetId", default)]
    subnet_id: Option<String>,
    #[serde(rename = "vpcId", default)]
    vpc_id: Option<String>,
    #[serde(rename = "privateIpAddress", default)]
    private_ip: Option<String>,
    /// EC2-Classic 的地址；VPC 里是空的。
    #[serde(rename = "ipAddress", default)]
    classic_ip: Option<String>,
    #[serde(rename = "platformDetails", default)]
    platform_details: Option<String>,
    #[serde(rename = "groupSet", default)]
    group_set: GroupSet,
    #[serde(rename = "networkInterfaceSet", default)]
    network_interface_set: NetworkInterfaceSet,
    /// `None` = 响应里没有 `tagSet`；`Some(vec![])` = 有但为空（三态要保住）。
    #[serde(rename = "tagSet", default)]
    tag_set: Option<TagSet>,
    #[serde(rename = "cpuOptions", default)]
    cpu_options: Option<CpuOptions>,
    #[serde(rename = "instanceLifecycle", default)]
    lifecycle: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct InstanceState {
    #[serde(rename = "name", default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Placement {
    #[serde(rename = "availabilityZone", default)]
    availability_zone: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GroupSet {
    #[serde(rename = "item", default)]
    items: Vec<GroupItem>,
}

#[derive(Debug, Default, Deserialize)]
struct GroupItem {
    #[serde(rename = "groupId", default)]
    group_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct NetworkInterfaceSet {
    #[serde(rename = "item", default)]
    items: Vec<NetworkInterface>,
}

#[derive(Debug, Default, Deserialize)]
struct NetworkInterface {
    #[serde(rename = "privateIpAddress", default)]
    private_ip_address: Option<String>,
    #[serde(rename = "association", default)]
    association: Option<AddressAssociation>,
    #[serde(rename = "privateIpAddressesSet", default)]
    private_ip_addresses_set: Option<PrivateIpSet>,
}

#[derive(Debug, Default, Deserialize)]
struct AddressAssociation {
    #[serde(rename = "publicIp", default)]
    public_ip: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PrivateIpSet {
    #[serde(rename = "item", default)]
    items: Vec<PrivateIpItem>,
}

#[derive(Debug, Default, Deserialize)]
struct PrivateIpItem {
    #[serde(rename = "privateIpAddress", default)]
    private_ip_address: Option<String>,
    #[serde(rename = "association", default)]
    association: Option<AddressAssociation>,
}

#[derive(Debug, Default, Deserialize)]
struct CpuOptions {
    #[serde(rename = "coreCount", default)]
    core_count: Option<String>,
}

/// 解析一页并映射成规范行。
pub fn map_instances(
    xml: &str,
    region: &str,
    region_name: &str,
) -> Result<Vec<CloudHost>, HarvestError> {
    let parsed: DescribeInstancesResponse = parse_xml(xml, DESCRIBE_INSTANCES.name)?;
    let mut out = Vec::new();
    for reservation in parsed.reservation_set.items {
        for instance in reservation.instances.items {
            out.push(map_instance(&instance, region, region_name)?);
        }
    }
    Ok(out)
}

fn map_instance(
    instance: &Instance,
    region: &str,
    region_name: &str,
) -> Result<CloudHost, HarvestError> {
    let id = text(instance.instance_id.clone());
    if id.is_empty() {
        return Err(
            HarvestError::data_format("DescribeInstances 响应缺少 instanceId")
                .with_action(DESCRIBE_INSTANCES.name)
                .with_region(region),
        );
    }

    let (private_ips, public_ips) = collect_ips(instance);
    let ip = private_ips.first().cloned().unwrap_or_default();
    let public_ip = public_ips.first().cloned().unwrap_or_default();
    let state = instance
        .state
        .as_ref()
        .map(|state| text(state.name.clone()))
        .unwrap_or_default();

    let tags_items = instance.tag_set.as_ref().map(|set| set.items.clone());
    let name = name_from_tags(tags_items.as_ref(), &id);

    // AWS 的 `DescribeInstances` 不返回内存；`CpuOptions.CoreCount` 是配置的核数
    // （不是 vCPU）。拿不到就 0，不按机型硬编码一张表去猜。
    let cpu_core_num = instance
        .cpu_options
        .as_ref()
        .and_then(|options| options.core_count.as_deref())
        .and_then(|count| count.parse::<i64>().ok())
        .unwrap_or(0);

    let charge_type = match instance.lifecycle.as_deref() {
        Some("spot") => "SPOT",
        _ => "",
    };
    let charge_type_name = match charge_type {
        "SPOT" => "竞价实例",
        _ => "",
    };

    Ok(CloudHost {
        host_id: id.clone(),
        host_name: name.clone(),
        source_asset_id: id.clone(),
        asset_code: id.clone(),
        asset_name: name,
        asset_status: state.clone(),
        host_state: state,
        // AWS 没有“限制状态”这个概念。
        restrict_state: String::new(),
        ip,
        public_ip,
        private_ips,
        public_ips,
        ipv6_addresses: Vec::new(),
        security_group_ids: instance
            .group_set
            .items
            .iter()
            .map(|group| text(group.group_id.clone()))
            .filter(|id| !id.is_empty())
            .collect(),
        tags: tags(tags_items),
        cpu_core_num,
        memory_capacity: 0,
        os_name: text(instance.platform_details.clone()),
        ins_type: text(instance.instance_type.clone()),
        ins_charge_type: charge_type.to_string(),
        ins_charge_type_name: charge_type_name.to_string(),
        version: COMPAT_VERSION.to_string(),
        create_time: text(instance.launch_time.clone()),
        expired_time: String::new(),
        region: region.to_string(),
        region_name: region_name.to_string(),
        zone: instance
            .placement
            .as_ref()
            .map(|placement| text(placement.availability_zone.clone()))
            .unwrap_or_default(),
        vpcid: text(instance.vpc_id.clone()),
        subvpcid: text(instance.subnet_id.clone()),
        project_id: String::new(),
        project_name: String::new(),
    })
}

fn collect_ips(instance: &Instance) -> (Vec<String>, Vec<String>) {
    let mut private = Vec::new();
    let mut public = Vec::new();
    push_unique(&mut private, &text(instance.private_ip.clone()));
    push_unique(&mut public, &text(instance.classic_ip.clone()));

    for interface in &instance.network_interface_set.items {
        push_unique(&mut private, &text(interface.private_ip_address.clone()));
        if let Some(association) = &interface.association {
            push_unique(&mut public, &text(association.public_ip.clone()));
        }
        if let Some(set) = &interface.private_ip_addresses_set {
            for item in &set.items {
                push_unique(&mut private, &text(item.private_ip_address.clone()));
                if let Some(association) = &item.association {
                    push_unique(&mut public, &text(association.public_ip.clone()));
                }
            }
        }
    }
    (private, public)
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !list.iter().any(|existing| existing == value) {
        list.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvest_core::model::Presence;

    fn fixture(tag_set: &str) -> String {
        format!(
            r#"<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-1</requestId>
  <reservationSet>
    <item>
      <instancesSet>
        <item>
          <instanceId>i-0abc</instanceId>
          <instanceType>t3.micro</instanceType>
          <instanceState><code>16</code><name>running</name></instanceState>
          <launchTime>2026-11-01T00:00:00.000Z</launchTime>
          <placement><availabilityZone>us-east-1a</availabilityZone></placement>
          <subnetId>subnet-1</subnetId>
          <vpcId>vpc-1</vpcId>
          <privateIpAddress>10.0.1.10</privateIpAddress>
          <platformDetails>Linux/UNIX</platformDetails>
          <groupSet><item><groupId>sg-1</groupId></item></groupSet>
          <networkInterfaceSet>
            <item>
              <privateIpAddress>10.0.1.10</privateIpAddress>
              <association><publicIp>203.0.113.10</publicIp></association>
              <privateIpAddressesSet>
                <item><privateIpAddress>10.0.1.10</privateIpAddress><primary>true</primary></item>
                <item><privateIpAddress>10.0.1.11</privateIpAddress></item>
              </privateIpAddressesSet>
            </item>
          </networkInterfaceSet>
          <cpuOptions><coreCount>2</coreCount></cpuOptions>
          {tag_set}
        </item>
      </instancesSet>
    </item>
  </reservationSet>
</DescribeInstancesResponse>"#
        )
    }

    #[test]
    fn maps_the_canonical_fields() {
        let xml = fixture(r#"<tagSet><item><key>Name</key><value>web-01</value></item></tagSet>"#);
        let hosts = map_instances(&xml, "us-east-1", "us-east-1").unwrap();
        assert_eq!(hosts.len(), 1);
        let host = &hosts[0];
        assert_eq!(host.source_asset_id, "i-0abc");
        assert_eq!(host.host_name, "web-01");
        assert_eq!(host.ip, "10.0.1.10");
        assert_eq!(host.public_ip, "203.0.113.10");
        assert_eq!(host.private_ips, vec!["10.0.1.10", "10.0.1.11"]);
        assert_eq!(host.public_ips, vec!["203.0.113.10"]);
        assert_eq!(host.vpcid, "vpc-1");
        assert_eq!(host.subvpcid, "subnet-1");
        assert_eq!(host.zone, "us-east-1a");
        assert_eq!(host.host_state, "running");
        assert_eq!(host.ins_type, "t3.micro");
        assert_eq!(host.cpu_core_num, 2);
        assert_eq!(host.version, "1");
        assert_eq!(host.security_group_ids, vec!["sg-1"]);
    }

    #[test]
    fn tags_keep_three_states() {
        let absent = map_instances(&fixture(""), "us-east-1", "us-east-1").unwrap();
        assert!(absent[0].tags.is_absent(), "没有 tagSet 字段 → 字段缺席");

        let empty = map_instances(&fixture("<tagSet/>"), "us-east-1", "us-east-1").unwrap();
        assert_eq!(empty[0].tags, Presence::Present(vec![]));
    }

    #[test]
    fn missing_instance_id_is_a_format_error() {
        let xml = "<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceType>t3.micro</instanceType></item></instancesSet></item></reservationSet></DescribeInstancesResponse>";
        let err = map_instances(xml, "us-east-1", "us-east-1").unwrap_err();
        assert_eq!(err.class, harvest_core::ErrorClass::DataFormat);
    }
}
