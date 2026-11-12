//! `Source` / `Sink` 契约。
//!
//! 设计说明 §3 把 `Source` 放在 `harvest-source` crate 里；P0 只有腾讯云一个
//! 实现，为避免多一个空 crate，契约就先落在 `harvest-core`（见 README 取舍说明）。

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::HarvestError;
use crate::model::{AssetType, Vendor};
use crate::spool::{Manifest, Spool};

/// 默认分页道。
pub const LANE_DEFAULT: &str = "default";
/// EIP 的第一路：`Filters=[{"Name":"address-type","Values":["WanIP"]}]`。
pub const LANE_WANIP: &str = "wanip";
/// EIP 的第二路：不带地址类型过滤。
pub const LANE_ALL: &str = "all";

/// 一个工作项 = 一类资产 × 一个地域 × 一条分页道。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkItem {
    pub asset_type: AssetType,
    pub region: String,
    pub lane: String,
}

impl WorkItem {
    pub fn new(asset_type: AssetType, region: impl Into<String>, lane: impl Into<String>) -> Self {
        Self {
            asset_type,
            region: region.into(),
            lane: lane.into(),
        }
    }

    /// 稳定且可用于目录名的键，例如 `CLOUD_HOST/ap-guangzhou.default`。
    pub fn key(&self) -> String {
        format!("{}/{}.{}", self.asset_type, self.region, self.lane)
    }
}

/// 单个工作项的抓取统计。是完整性证据，也进运行报告。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchStat {
    pub rows: u64,
    pub pages: u64,
    /// 云端声明的总数（`TotalCount`）。用于事后核对分页是否连续。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rows: Option<u64>,
    #[serde(default)]
    pub retries: u64,
    #[serde(default)]
    pub request_ids: Vec<String>,
    #[serde(default)]
    pub duration_ms: u64,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn vendor(&self) -> Vendor;

    /// 类型 × 地域 × 分页计划。必须在拿租约之后、抓取之前确定。
    fn plan(&self) -> Vec<WorkItem>;

    /// 抓一个工作项，边抓边写 spool（有界内存）。
    async fn fetch(&self, item: &WorkItem, spool: &mut Spool) -> Result<FetchStat, HarvestError>;

    /// 完整性判定。不通过即 `Aborted`，一帧不发。
    fn verify(&self, manifest: &Manifest) -> Result<(), HarvestError>;

    /// 本帧的覆盖范围声明（`complete` 由运行器在 [`Source::verify`] 通过后置真）。
    ///
    /// 默认 `partial`：没有明确证据的采集器不能声明「账号级全量」，否则下游会把
    /// 「这次没采到」当成「已经不存在」。要声明账号级全集，必须真的把账号可见的
    /// 所有地域、所有配置的资产类型都采到了（例如腾讯云的 `regions = "auto"`）。
    fn coverage(&self) -> crate::model::Coverage {
        crate::model::Coverage::partial()
    }
}

/// 待发布的一帧。`bytes` 就是数据平面要收的帧体。
#[derive(Debug, Clone)]
pub struct CompleteSnapshot {
    pub snap_id: String,
    pub bytes: Vec<u8>,
    pub rows: u64,
    pub per_type: BTreeMap<AssetType, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishAck {
    pub bytes: u64,
    pub ack: String,
    pub duration_ms: u64,
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn publish_snapshot(&self, snap: &CompleteSnapshot) -> Result<PublishAck, HarvestError>;
}
