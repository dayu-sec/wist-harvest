//! AWS 的读接口与分页驱动。
//!
//! 与腾讯云的差异（都落在这一层，不污染 core）：
//!
//! - 游标是 `NextToken`，**没有总数** → 完整性证据是「游标耗尽」（`Completeness::Exhausted`）；
//! - 响应是 XML，标签大小写混用（`tagSet` 里的 `key`/`value` 是小写）；
//! - EIP 只有一路（不分 `wanip`/`all`）；
//! - 很多字段 AWS 不提供（VPC 创建时间、子网路由表/ACL、EIP 带宽）→ 保持 `Absent`，
//!   不编造 0 或空串。

pub mod address;
pub mod identity;
pub mod instance;
pub mod network;
pub mod region;

use async_trait::async_trait;
use harvest_core::error::HarvestError;
use harvest_core::model::{Presence, Tag};
use harvest_core::page::{PageBatch, PageCursor, PageFetcher};
use harvest_core::source::FetchStat;
use serde::{Deserialize, Serialize};

use crate::client::{ApiAction, AwsClient};

/// 一次分页任务需要的东西。
pub struct AwsPageSpec<'a> {
    pub action: ApiAction,
    pub region: &'a str,
    /// 除 `Action`/`Version`/分页参数以外的参数。
    pub params: Vec<(String, String)>,
    /// 分页大小参数（`MaxResults`）。`None` = 这个接口不分页。
    pub page_size: Option<u64>,
    pub page_size_param: &'static str,
}

impl<'a> AwsPageSpec<'a> {
    pub fn new(action: ApiAction, region: &'a str) -> Self {
        Self {
            action,
            region,
            params: Vec::new(),
            page_size: None,
            page_size_param: "MaxResults",
        }
    }

    pub fn with_page_size(mut self, size: u64) -> Self {
        self.page_size = Some(size.clamp(5, 1000));
        self
    }
}

/// 只取分页外壳（`nextToken` / `requestId`），正文由各 reader 自己解析。
#[derive(Debug, Default, Deserialize)]
struct PageEnvelope {
    #[serde(rename = "nextToken", default)]
    next_token: Option<String>,
    #[serde(rename = "requestId", default)]
    request_id: Option<String>,
}

struct AwsPager<'a, F> {
    client: &'a AwsClient,
    spec: &'a AwsPageSpec<'a>,
    map_page: F,
}

#[async_trait]
impl<F, T> PageFetcher for AwsPager<'_, F>
where
    F: Fn(&str) -> Result<Vec<T>, HarvestError> + Send + Sync,
    T: Serialize + Send + Sync,
{
    type Row = T;

    async fn fetch_page(&self, cursor: Option<&PageCursor>) -> Result<PageBatch<T>, HarvestError> {
        let mut params = self.spec.params.clone();
        if let Some(size) = self.spec.page_size {
            params.push((self.spec.page_size_param.to_string(), size.to_string()));
        }
        if let Some(cursor) = cursor {
            params.push(("NextToken".to_string(), cursor.as_str().to_string()));
        }

        let outcome = self
            .client
            .call(&self.spec.action, self.spec.region, &params)
            .await?;

        let envelope: PageEnvelope = quick_xml::de::from_str(&outcome.body).map_err(|err| {
            HarvestError::data_format(format!(
                "{} 响应不是可解析的 XML: {err}",
                self.spec.action.name
            ))
        })?;
        let rows = (self.map_page)(&outcome.body)?;

        Ok(PageBatch {
            rows,
            next: envelope
                .next_token
                .filter(|token| !token.trim().is_empty())
                .map(PageCursor::new),
            // AWS 的 EC2 查询接口不返回总数。
            total: None,
            request_id: envelope.request_id,
            retries: outcome.retries,
        })
    }
}

/// 抓一个分页任务：逐页解析 → 逐页落 spool → 汇总完整性证据。
pub async fn paginate<T, F, S>(
    client: &AwsClient,
    spec: AwsPageSpec<'_>,
    map_page: F,
    on_page: S,
) -> Result<FetchStat, HarvestError>
where
    T: Serialize + Send + Sync,
    F: Fn(&str) -> Result<Vec<T>, HarvestError> + Send + Sync,
    S: FnMut(&[T]) -> Result<(), HarvestError> + Send,
{
    let pager = AwsPager {
        client,
        spec: &spec,
        map_page,
    };
    harvest_core::page::drive(&pager, on_page).await
}

/// 把 XML 解析成结构；失败时带上 action 名，便于定位。
pub fn parse_xml<T: for<'de> Deserialize<'de>>(
    body: &str,
    action: &str,
) -> Result<T, HarvestError> {
    quick_xml::de::from_str(body)
        .map_err(|err| HarvestError::data_format(format!("{action} 响应解析失败: {err}")))
}

/// XML 文本节点 → 字符串。缺失与空串都归一成 `""`。
pub fn text(value: Option<String>) -> String {
    value.unwrap_or_default()
}

/// EC2 的标签项（**小写** `key`/`value`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TagItem {
    #[serde(rename = "key", default)]
    pub key: Option<String>,
    #[serde(rename = "value", default)]
    pub value: Option<String>,
}

/// `tagSet` 容器。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TagSet {
    #[serde(rename = "item", default)]
    pub items: Vec<TagItem>,
}

/// 标签三态：`tagSet` 缺席 → `Absent`；存在但为空 → `Present([])`。
pub fn tags(value: Option<Vec<TagItem>>) -> Presence<Vec<Tag>> {
    match value {
        None => Presence::absent(),
        Some(items) => Presence::present(
            items
                .into_iter()
                .map(|item| Tag {
                    key: item.key.unwrap_or_default(),
                    value: item.value.unwrap_or_default(),
                })
                .collect(),
        ),
    }
}

/// 从标签里取 `Name`（AWS 上这是“显示名”的惯例）。
pub fn name_from_tags(value: Option<&Vec<TagItem>>, fallback: &str) -> String {
    value
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.key.as_deref() == Some("Name"))
                .and_then(|item| item.value.clone())
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
