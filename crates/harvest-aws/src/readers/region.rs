//! 地域：`ec2:DescribeRegions`。
//!
//! AWS 的“账号可见地域全集”比腾讯云多一层：region 有 `optInStatus`
//! （`opt-in-not-required` / `opted-in` / `not-opted-in`）。只有前两种算“我这边能采到”，
//! `not-opted-in` 必须排除——否则 `coverage.scope=account` 会变成假话。

use harvest_core::error::HarvestError;
use serde::Deserialize;

use super::{parse_xml, text};
use crate::client::DESCRIBE_REGIONS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionEntry {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct DescribeRegionsResponse {
    #[serde(rename = "regionInfo", default)]
    region_info: RegionInfo,
}

#[derive(Debug, Default, Deserialize)]
struct RegionInfo {
    #[serde(rename = "item", default)]
    items: Vec<RegionItem>,
}

#[derive(Debug, Default, Deserialize)]
struct RegionItem {
    #[serde(rename = "regionName", default)]
    region_name: Option<String>,
    #[serde(rename = "optInStatus", default)]
    opt_in_status: Option<String>,
}

/// 解析可见地域（排序去重后返回）。一个都没有视为范围无法确定。
pub fn map_regions(xml: &str) -> Result<Vec<RegionEntry>, HarvestError> {
    let parsed: DescribeRegionsResponse = parse_xml(xml, DESCRIBE_REGIONS.name)?;
    let mut regions: Vec<RegionEntry> = parsed
        .region_info
        .items
        .into_iter()
        .filter(|item| match item.opt_in_status.as_deref() {
            // 没给状态时按可用处理：不静默丢地域。
            None | Some("") | Some("opt-in-not-required") | Some("opted-in") => true,
            _ => false,
        })
        .filter_map(|item| {
            let code = text(item.region_name);
            if code.is_empty() {
                return None;
            }
            Some(RegionEntry {
                // EC2 不提供展示名，先用地域码，现场可用 `region_names` 覆盖。
                name: code.clone(),
                code,
            })
        })
        .collect();
    regions.sort_by(|a, b| a.code.cmp(&b.code));
    regions.dedup_by(|a, b| a.code == b.code);
    if regions.is_empty() {
        return Err(HarvestError::integrity(
            "DescribeRegions 没有返回任何可用地域（范围无法确定）",
        ));
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_regions_that_are_not_opted_in() {
        let xml = r#"<DescribeRegionsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <regionInfo>
    <item><regionName>us-east-1</regionName><optInStatus>opt-in-not-required</optInStatus></item>
    <item><regionName>ap-east-1</regionName><optInStatus>opted-in</optInStatus></item>
    <item><regionName>me-south-1</regionName><optInStatus>not-opted-in</optInStatus></item>
  </regionInfo>
</DescribeRegionsResponse>"#;
        let regions = map_regions(xml).unwrap();
        let codes: Vec<&str> = regions.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(codes, vec!["ap-east-1", "us-east-1"]);
    }

    #[test]
    fn an_empty_region_list_is_an_error_not_an_empty_scope() {
        let xml = "<DescribeRegionsResponse><regionInfo/></DescribeRegionsResponse>";
        let err = map_regions(xml).unwrap_err();
        assert_eq!(err.class, harvest_core::ErrorClass::Integrity);
    }
}
