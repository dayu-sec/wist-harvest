//! 公网 IP：`DescribeAddresses`（`vpc.tencentcloudapi.com`，`2017-03-12`）。
//!
//! 每个地域必须做**两路完整分页**：
//!
//! 1. `Filters=[{"Name":"address-type","Values":["WanIP"]}]` —— 普通公网 IP；
//! 2. 不带地址类型过滤 —— 弹性公网 IP。
//!
//! 官方说明第 2 路不含普通公网 IP，两路会重叠，因此合并按 `AddressId` 去重，
//! **不能把两路 `TotalCount` 相加当最终数量**。

use harvest_core::error::HarvestError;
use harvest_core::model::Eip;
use serde_json::{Map, Value, json};

use super::{MapContext, required_id};
use crate::client::ApiAction;
use crate::value::{as_i64, as_str};

pub const DESCRIBE_ADDRESSES: ApiAction = ApiAction {
    name: "DescribeAddresses",
    host: "vpc.tencentcloudapi.com",
    version: "2017-03-12",
    service: "vpc",
};

pub const SET_KEY: &str = "AddressSet";

/// 第一路的过滤条件。
pub fn wanip_filter() -> Value {
    json!([{ "Name": "address-type", "Values": ["WanIP"] }])
}

pub fn map_address(obj: &Map<String, Value>, ctx: &MapContext<'_>) -> Result<Eip, HarvestError> {
    let id = required_id(obj, "AddressId", DESCRIBE_ADDRESSES.name, ctx.region)?;
    let name = {
        let name = as_str(obj.get("AddressName"));
        if name.is_empty() { id.clone() } else { name }
    };

    Ok(Eip {
        source_asset_id: id,
        asset_name: name,
        eip_address_type: as_str(obj.get("AddressType")),
        eip_address_ip: as_str(obj.get("AddressIp")),
        eip_address_status: as_str(obj.get("AddressStatus")),
        eip_instance_type: as_str(obj.get("InstanceType")),
        eip_instance_id: as_str(obj.get("InstanceId")),
        eip_provider: as_str(obj.get("InternetServiceProvider")),
        eip_bandwidth: as_i64(obj.get("Bandwidth")).unwrap_or(0),
        eip_charge_type: as_str(obj.get("InternetChargeType")),
        deadline_date: as_str(obj.get("DeadlineDate")),
        region: ctx.region.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn maps_eip_fields() {
        let value = json!({
            "AddressId": "eip-example01",
            "AddressName": "order-public-ip",
            "AddressType": "EIP",
            "AddressIp": "203.0.113.10",
            "AddressStatus": "BIND",
            "InstanceType": "CVM",
            "InstanceId": "ins-example01",
            "InternetServiceProvider": "BGP",
            "Bandwidth": 10,
            "InternetChargeType": "TRAFFIC_POSTPAID_BY_HOUR",
            "DeadlineDate": null
        });
        let eip = map_address(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(eip.source_asset_id, "eip-example01");
        assert_eq!(eip.eip_address_ip, "203.0.113.10");
        assert_eq!(eip.eip_bandwidth, 10);
        assert_eq!(eip.deadline_date, "");
        assert_eq!(eip.region, "ap-guangzhou");
    }

    #[test]
    fn wanip_filter_shape() {
        assert_eq!(
            wanip_filter(),
            json!([{"Name": "address-type", "Values": ["WanIP"]}])
        );
    }
}
