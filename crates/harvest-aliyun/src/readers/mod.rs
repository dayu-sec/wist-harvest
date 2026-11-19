//! 分页驱动 + 四类资产的字段归一。
//!
//! 阿里云 RPC 统一用 `PageNumber` / `PageSize`（**1 起**）+ `TotalCount`。
//! 数组形状是 `{"Outer":{"Inner":[…]}}`，所以每个读取器要声明外层与内层两个键。
//!
//! `PageSize` 的上限各家接口不同（ECS / EIP 100，VPC / VSwitch 50），
//! 由 [`PageSpec::max_page_size`] 声明并在请求前夹紧：配置只能收紧能力，
//! 不能因为默认 `page_size = 100` 就向只接受 50 的接口发 100。

pub mod billing;
pub mod eip;
pub mod instance;
pub mod network;

use async_trait::async_trait;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::page::{PageBatch, PageCursor, PageFetcher};
use harvest_core::source::FetchStat;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::client::{AliyunClient, ApiAction, request_id};
use crate::value::{as_i64, as_str, inner_items};

/// 一个分页任务的形态。
#[derive(Debug, Clone)]
pub struct PageSpec {
    pub action: ApiAction,
    /// 外层对象键，例如 `Instances`。
    pub outer_key: &'static str,
    /// 内层数组键，例如 `Instance`。
    pub inner_key: &'static str,
    /// 该接口允许的最大 `PageSize`。
    pub max_page_size: u64,
}

/// 归一一个地域时需要的上下文。
#[derive(Debug, Clone)]
pub struct MapContext<'a> {
    pub region: &'a str,
    pub region_name: &'a str,
}

/// 阿里云的分页：`PageNumber` + `PageSize` + `TotalCount`。
///
/// 循环、总数校验、完整性证据都在 `harvest_core::page`；这里只负责
/// “发哪一页”与“游标怎么推进”。
struct AliyunPageFetcher<'a, F> {
    client: &'a AliyunClient,
    spec: &'a PageSpec,
    region: &'a str,
    page_size: u64,
    map_row: F,
}

#[async_trait]
impl<F, T> PageFetcher for AliyunPageFetcher<'_, F>
where
    F: Fn(&Map<String, Value>) -> Result<T, HarvestError> + Send + Sync,
    T: Serialize + Send + Sync,
{
    type Row = T;

    async fn fetch_page(&self, cursor: Option<&PageCursor>) -> Result<PageBatch<T>, HarvestError> {
        let action = self.spec.action.name;
        let page: u64 = match cursor {
            Some(cursor) => cursor
                .as_str()
                .parse()
                .map_err(|_| HarvestError::data_format(format!("{action} 的分页游标非法")))?,
            None => 1,
        };

        let page_size = self.page_size.min(self.spec.max_page_size).max(1);
        let params = [
            ("RegionId", self.region.to_string()),
            ("PageNumber", page.to_string()),
            ("PageSize", page_size.to_string()),
        ];

        let outcome = self
            .client
            .call(&self.spec.action, Some(self.region), &params)
            .await
            .map_err(|err| err.with_region(self.region))?;

        let total = as_i64(outcome.body.get("TotalCount")).ok_or_else(|| {
            HarvestError::data_format(format!("{action} 响应缺少 TotalCount"))
                .with_action(action)
                .with_region(self.region)
        })? as u64;

        let body = outcome
            .body
            .as_object()
            .ok_or_else(|| HarvestError::data_format(format!("{action} 响应不是对象")))?;

        let items = match inner_items(body, self.spec.outer_key, self.spec.inner_key) {
            Some(items) => items,
            // 空结果时阿里云可能省略整个外层对象；`TotalCount == 0` 才允许，
            // 否则就是响应结构不对。
            None if total == 0 => Vec::new(),
            None => {
                return Err(HarvestError::data_format(format!(
                    "{action} 响应缺少数组 {}.{}",
                    self.spec.outer_key, self.spec.inner_key
                ))
                .with_action(action)
                .with_region(self.region));
            }
        };

        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            let obj = item.as_object().ok_or_else(|| {
                HarvestError::data_format(format!(
                    "{}.{} 的数组元素不是对象",
                    self.spec.outer_key, self.spec.inner_key
                ))
                .with_action(action)
                .with_region(self.region)
            })?;
            rows.push((self.map_row)(obj)?);
        }

        // 用实际返回条数推进，而不是假定每页满 `page_size`。
        //
        // `PageNumber` 分页没有“从当前 offset 继续”的能力：如果在中间页返回了
        // 短页（< 请求的 PageSize），再请求下一页会**跳过**这段数据。所以短页就是
        // 最后一页；若云端 `TotalCount` 还说有更多行，交给驱动层判成完整性问题
        // （`Aborted`），而不是静默漏采。
        let fetched = (page - 1) * page_size + rows.len() as u64;
        let next = (rows.len() as u64 == page_size && fetched < total)
            .then(|| PageCursor::new((page + 1).to_string()));

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
    client: &AliyunClient,
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
    let fetcher = AliyunPageFetcher {
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
        "PrePaid" => "预付费(包年包月)".to_string(),
        "PostPaid" => "按量付费".to_string(),
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
        assert_eq!(charge_type_name("PrePaid"), "预付费(包年包月)");
        assert_eq!(charge_type_name("PostPaid"), "按量付费");
        assert_eq!(charge_type_name("FUTURE_CODE"), "FUTURE_CODE");
    }
}
