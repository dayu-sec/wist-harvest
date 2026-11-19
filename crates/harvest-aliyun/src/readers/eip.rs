//! 公网 IP：`DescribeEipAddresses`（`vpc.aliyuncs.com`，`2016-04-28`）。
//!
//! 与腾讯云不同，阿里云这一路**没有** `WanIP` / `EIP` 两道过滤：接口返回的就是
//! 账号在该地域的全部 EIP，所以每地域只需要一条分页道。

use harvest_core::error::HarvestError;
use harvest_core::model::Eip;
use serde_json::{Map, Value};

use super::{MapContext, required_id};
use crate::client::ApiAction;
use crate::value::{as_i64, as_str};

pub const DESCRIBE_EIP_ADDRESSES: ApiAction = ApiAction {
    name: "DescribeEipAddresses",
    host: "vpc.aliyuncs.com",
    version: "2016-04-28",
};

pub const OUTER_KEY: &str = "EipAddresses";
pub const INNER_KEY: &str = "EipAddress";

/// 该接口 `PageSize` 上限。
pub const MAX_PAGE_SIZE: u64 = 100;

pub fn map_address(obj: &Map<String, Value>, ctx: &MapContext<'_>) -> Result<Eip, HarvestError> {
    let id = required_id(obj, "AllocationId", DESCRIBE_EIP_ADDRESSES.name, ctx.region)?;
    let name = {
        let name = as_str(obj.get("Name"));
        if name.is_empty() { id.clone() } else { name }
    };

    // 新版本返回 `IpAddress`；老版本把 IP 直接放在 `EipAddress` 里。
    let ip = {
        let primary = as_str(obj.get("IpAddress"));
        if primary.is_empty() {
            as_str(obj.get("EipAddress"))
        } else {
            primary
        }
    };

    Ok(Eip {
        source_asset_id: id,
        asset_name: name,
        // 阿里云的公网 IP 都是 EIP（没有腾讯云的普通公网 IP 概念）。
        eip_address_type: "EIP".to_string(),
        eip_address_ip: ip,
        eip_address_status: as_str(obj.get("Status")),
        eip_instance_type: as_str(obj.get("InstanceType")),
        eip_instance_id: as_str(obj.get("InstanceId")),
        eip_provider: as_str(obj.get("ISP")),
        eip_bandwidth: as_i64(obj.get("Bandwidth")).unwrap_or(0),
        eip_charge_type: as_str(obj.get("InternetChargeType")),
        deadline_date: as_str(obj.get("ExpiredTime")),
        region: ctx.region.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> MapContext<'static> {
        MapContext {
            region: "cn-hangzhou",
            region_name: "华东1（杭州）",
        }
    }

    #[test]
    fn maps_eip_fields() {
        let value = json!({
            "AllocationId": "eip-example01",
            "Name": "order-public-ip",
            "IpAddress": "203.0.113.10",
            "Status": "InUse",
            "InstanceType": "EcsInstance",
            "InstanceId": "i-example01",
            "ISP": "BGP",
            "Bandwidth": 10,
            "InternetChargeType": "PayByTraffic",
            "ExpiredTime": "2026-12-01T00:00:00Z"
        });
        let eip = map_address(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(eip.source_asset_id, "eip-example01");
        assert_eq!(eip.eip_address_type, "EIP");
        assert_eq!(eip.eip_address_ip, "203.0.113.10");
        assert_eq!(eip.eip_bandwidth, 10);
        assert_eq!(eip.eip_provider, "BGP");
        assert_eq!(eip.deadline_date, "2026-12-01T00:00:00Z");
        assert_eq!(eip.region, "cn-hangzhou");
    }

    #[test]
    fn legacy_address_field_is_accepted() {
        let value = json!({"AllocationId": "eip-1", "EipAddress": "198.51.100.1"});
        let eip = map_address(value.as_object().unwrap(), &ctx()).unwrap();
        assert_eq!(eip.eip_address_ip, "198.51.100.1");
    }
}
