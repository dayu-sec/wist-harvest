//! 分页驱动 + 四类资产的字段归一。
//!
//! 分页约定来自交接文档 §3.1/§3.3/§3.4：
//!
//! - CVM `DescribeInstances`、`DescribeAddresses` 用**整数** `Limit`/`Offset`；
//! - `DescribeVpcs`、`DescribeSubnets` 用**字符串**形式的 `Limit`/`Offset`
//!   （沿用既有实现；见 [`PageSpec::limit_as_string`]）。

pub mod cvm;
pub mod eip;
pub mod network;

use std::collections::BTreeMap;

use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::source::FetchStat;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::client::{ApiAction, TencentClient, request_id};
use crate::value::{as_i64, as_str};

/// 防御性上限：分页失控时不至于把进程拖死。
const MAX_PAGES: u64 = 10_000;

/// 一个分页任务的形态。
#[derive(Debug, Clone)]
pub struct PageSpec {
    pub action: ApiAction,
    /// 响应里装数据的数组键，例如 `InstanceSet`。
    pub set_key: &'static str,
    /// `Limit`/`Offset` 是否用字符串形式提交。
    pub limit_as_string: bool,
    /// 附加的请求体字段（如 EIP 的 `Filters`）。
    pub extra_body: Option<Value>,
}

/// 归一一个地域时需要的上下文。
#[derive(Debug, Clone)]
pub struct MapContext<'a> {
    pub region: &'a str,
    pub region_name: &'a str,
    pub projects: &'a BTreeMap<String, String>,
}

/// 抓一个分页任务：逐页映射 → 逐页落 spool（有界内存）→ 返回完整性证据。
pub async fn paginate<T, F, S>(
    client: &TencentClient,
    spec: &PageSpec,
    region: &str,
    page_size: u64,
    mut map_row: F,
    mut on_page: S,
) -> Result<FetchStat, HarvestError>
where
    T: Serialize,
    F: FnMut(&Map<String, Value>) -> Result<T, HarvestError>,
    S: FnMut(&[T]) -> Result<(), HarvestError>,
{
    let started = std::time::Instant::now();
    let mut offset: u64 = 0;
    let mut total: Option<u64> = None;
    let mut rows: u64 = 0;
    let mut pages: u64 = 0;
    let mut retries: u64 = 0;
    let mut request_ids: Vec<String> = Vec::new();

    loop {
        let mut body = Map::new();
        if spec.limit_as_string {
            body.insert("Limit".into(), json!(page_size.to_string()));
            body.insert("Offset".into(), json!(offset.to_string()));
        } else {
            body.insert("Limit".into(), json!(page_size));
            body.insert("Offset".into(), json!(offset));
        }
        if let Some(extra) = &spec.extra_body
            && let Some(extra) = extra.as_object()
        {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }

        let outcome = client
            .call(&spec.action, Some(region), &Value::Object(body))
            .await
            .map_err(|err| err.with_region(region))?;
        retries += u64::from(outcome.retries);
        if let Some(id) = request_id(&outcome.body) {
            request_ids.push(id);
        }

        let page_total = as_i64(outcome.body.get("TotalCount")).ok_or_else(|| {
            HarvestError::data_format(format!("{} 响应缺少 TotalCount", spec.action.name))
                .with_action(spec.action.name)
                .with_region(region)
        })? as u64;
        match total {
            None => total = Some(page_total),
            Some(previous) if previous != page_total => {
                return Err(HarvestError::integrity(format!(
                    "{} 分页总数前后不一致: 首页 {previous}，当前页 {page_total}",
                    spec.action.name
                ))
                .with_action(spec.action.name)
                .with_region(region));
            }
            Some(_) => {}
        }

        let items = outcome
            .body
            .get(spec.set_key)
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                HarvestError::data_format(format!(
                    "{} 响应缺少数组 {}",
                    spec.action.name, spec.set_key
                ))
                .with_action(spec.action.name)
                .with_region(region)
            })?;

        pages += 1;
        let mut page_rows = Vec::with_capacity(items.len());
        for item in items {
            let obj = item.as_object().ok_or_else(|| {
                HarvestError::data_format(format!("{} 的数组元素不是对象", spec.set_key))
                    .with_action(spec.action.name)
                    .with_region(region)
            })?;
            page_rows.push(map_row(obj)?);
        }
        rows += page_rows.len() as u64;
        on_page(&page_rows)?;

        if items.is_empty() {
            break;
        }
        // 用实际返回条数推进，而不是假定每页满 page_size。
        offset += items.len() as u64;
        if offset >= page_total {
            break;
        }
        if pages >= MAX_PAGES {
            return Err(HarvestError::data_format(format!(
                "{} 分页超过 {MAX_PAGES} 页，疑似服务端 TotalCount 异常",
                spec.action.name
            ))
            .with_action(spec.action.name)
            .with_region(region));
        }
    }

    if let Some(expected) = total
        && expected != rows
    {
        return Err(HarvestError::integrity(format!(
            "分页总数不一致: {} 云端 TotalCount={expected}，实收={rows}",
            spec.action.name
        ))
        .with_action(spec.action.name)
        .with_region(region));
    }

    Ok(FetchStat {
        rows,
        pages,
        expected_rows: total,
        retries,
        request_ids,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// 计费方式代码 → 名称。未识别代码原样保留，不硬套枚举。
pub fn charge_type_name(code: &str) -> String {
    match code {
        "PREPAID" => "预付费(包年包月)".to_string(),
        "POSTPAID_BY_HOUR" => "按小时后付费".to_string(),
        "CDHPAID" => "独享子机".to_string(),
        "SPOTPAID" => "竞价付费".to_string(),
        "CDCPAID" => "专用集群付费".to_string(),
        other => other.to_string(),
    }
}

/// 必填 ID：云资源主键不能是空的，空了就是数据格式问题。
pub fn required_id(
    obj: &Map<String, Value>,
    key: &str,
    action: &'static str,
    region: &str,
) -> Result<String, HarvestError> {
    let id = as_str(obj.get(key));
    if id.is_empty() {
        return Err(
            HarvestError::new(ErrorClass::DataFormat, format!("{action} 响应缺少 {key}"))
                .with_action(action)
                .with_region(region),
        );
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_type_keeps_unknown_codes() {
        assert_eq!(charge_type_name("PREPAID"), "预付费(包年包月)");
        assert_eq!(charge_type_name("FUTURE_CODE"), "FUTURE_CODE");
    }
}
