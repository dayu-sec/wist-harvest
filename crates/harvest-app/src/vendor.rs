//! 云厂商注册表：**app 只认 vendor 字符串，不认具体类型**。
//!
//! 新增一家云 = 新增一个 `harvest-{vendor}` crate + 在这里加一条分支 + 在
//! `sources.d` 里写 `vendor = "..."`。core 与其它厂商不需要改动，这就是 P1 的验收线。

use harvest_core::config::{ServiceConfig, SourceConfig};
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::model::Vendor;
use harvest_core::source::{CostSource, Source, WorkItem};

/// 按配置里的 `vendor` 构建数据源（会联网：解析地域/身份）。
pub async fn build_source(
    cfg: &SourceConfig,
    svc: &ServiceConfig,
) -> Result<Box<dyn Source>, HarvestError> {
    match cfg.vendor() {
        Vendor::TencentCloud => Ok(Box::new(
            harvest_tencent::TencentSource::init(cfg.clone(), svc).await?,
        )),
        Vendor::Aws => Ok(Box::new(
            harvest_aws::AwsSource::init(cfg.clone(), svc).await?,
        )),
    }
}

/// 按配置里的 `vendor` 构建成本源。
///
/// 目前只有腾讯云实现了账单 reader；AWS 明确报「未实现」，而不是悄悄按腾讯云跑。
pub async fn build_cost_source(
    cfg: &SourceConfig,
    svc: &ServiceConfig,
) -> Result<Box<dyn CostSource>, HarvestError> {
    match cfg.vendor() {
        Vendor::TencentCloud => Ok(Box::new(
            harvest_tencent::TencentSource::init(cfg.clone(), svc).await?,
        )),
        Vendor::Aws => Err(HarvestError::new(
            ErrorClass::InvalidParam,
            "AWS 账单采集尚未实现（成本契约 v1 目前只有腾讯云）",
        )),
    }
}

/// 不联网的采集计划（`dry-run` / `diagnose` 用）。
///
/// `regions = "auto"` 时返回空计划——地域要联网才知道，不能猜。
pub fn plan_from_config(cfg: &SourceConfig) -> Result<Vec<WorkItem>, HarvestError> {
    match cfg.vendor() {
        Vendor::TencentCloud => harvest_tencent::plan_from_config(cfg),
        Vendor::Aws => harvest_aws::plan_from_config(cfg),
    }
}

/// 该厂商的凭据必填键（`diagnose` 用来判断「配没配齐」）。
pub fn required_credentials(vendor: Vendor) -> &'static [&'static str] {
    match vendor {
        Vendor::TencentCloud => harvest_tencent::REQUIRED_CREDENTIALS,
        Vendor::Aws => harvest_aws::REQUIRED_CREDENTIALS,
    }
}
