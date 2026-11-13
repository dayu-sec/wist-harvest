//! 分页驱动 + 四类资产的字段归一。
//!
//! 分页约定来自交接文档 §3.1/§3.3/§3.4：
//!
//! - CVM `DescribeInstances`、`DescribeAddresses` 用**整数** `Limit`/`Offset`；
//! - `DescribeVpcs`、`DescribeSubnets` 用**字符串**形式的 `Limit`/`Offset`
//!   （沿用既有实现；见 [`PageSpec::limit_as_string`]）。

pub mod billing;
pub mod cvm;
pub mod eip;
pub mod network;

use std::collections::BTreeMap;

use async_trait::async_trait;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::page::{PageBatch, PageCursor, PageFetcher};
use harvest_core::source::FetchStat;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::client::{ApiAction, TencentClient, request_id};
use crate::value::{as_i64, as_str};

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

/// 腾讯云的分页：`Limit` + `Offset` + `TotalCount`。
///
/// 循环、总数校验、完整性证据都在 `harvest_core::page` 里，这里只负责
/// “发哪一页”与“游标怎么推进”。
struct TencentPageFetcher<'a, F> {
    client: &'a TencentClient,
    spec: &'a PageSpec,
    region: &'a str,
    page_size: u64,
    map_row: F,
}

#[async_trait]
impl<F, T> PageFetcher for TencentPageFetcher<'_, F>
where
    F: Fn(&Map<String, Value>) -> Result<T, HarvestError> + Send + Sync,
    T: Serialize + Send + Sync,
{
    type Row = T;

    async fn fetch_page(&self, cursor: Option<&PageCursor>) -> Result<PageBatch<T>, HarvestError> {
        let action = self.spec.action.name;
        let offset: u64 = match cursor {
            Some(cursor) => cursor
                .as_str()
                .parse()
                .map_err(|_| HarvestError::data_format(format!("{action} 的分页游标非法")))?,
            None => 0,
        };

        let mut body = Map::new();
        if self.spec.limit_as_string {
            body.insert("Limit".into(), json!(self.page_size.to_string()));
            body.insert("Offset".into(), json!(offset.to_string()));
        } else {
            body.insert("Limit".into(), json!(self.page_size));
            body.insert("Offset".into(), json!(offset));
        }
        if let Some(extra) = &self.spec.extra_body
            && let Some(extra) = extra.as_object()
        {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }

        let outcome = self
            .client
            .call(&self.spec.action, Some(self.region), &Value::Object(body))
            .await
            .map_err(|err| err.with_region(self.region))?;

        let total = as_i64(outcome.body.get("TotalCount")).ok_or_else(|| {
            HarvestError::data_format(format!("{action} 响应缺少 TotalCount"))
                .with_action(action)
                .with_region(self.region)
        })? as u64;

        let items = outcome
            .body
            .get(self.spec.set_key)
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                HarvestError::data_format(format!("{action} 响应缺少数组 {}", self.spec.set_key))
                    .with_action(action)
                    .with_region(self.region)
            })?;

        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let obj = item.as_object().ok_or_else(|| {
                HarvestError::data_format(format!("{} 的数组元素不是对象", self.spec.set_key))
                    .with_action(action)
                    .with_region(self.region)
            })?;
            rows.push((self.map_row)(obj)?);
        }

        // 用实际返回条数推进，而不是假定每页满 `page_size`。
        let next = if rows.is_empty() {
            None
        } else {
            let next_offset = offset + rows.len() as u64;
            (next_offset < total).then(|| PageCursor::new(next_offset.to_string()))
        };

        Ok(PageBatch {
            rows,
            next,
            total: Some(total),
            request_id: request_id(&outcome.body),
            retries: outcome.retries,
        })
    }
}

/// 抓一个分页任务：逐页映射 → 逐页落 spool（有界内存）→ 返回完整性证据。
pub async fn paginate<T, F, S>(
    client: &TencentClient,
    spec: &PageSpec,
    region: &str,
    page_size: u64,
    map_row: F,
    on_page: S,
) -> Result<FetchStat, HarvestError>
where
    T: Serialize + Send + Sync,
    F: Fn(&Map<String, Value>) -> Result<T, HarvestError> + Send + Sync,
    S: FnMut(&[T]) -> Result<(), HarvestError> + Send,
{
    let fetcher = TencentPageFetcher {
        client,
        spec,
        region,
        page_size,
        map_row,
    };
    harvest_core::page::drive(&fetcher, on_page).await
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
