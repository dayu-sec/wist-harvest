//! 阿里云数据源：把四个 reader 组装成 [`Source`]。
//!
//! 采集组 `inventory` 的四类资产：
//!
//! | 类型 | API | 分页道 |
//! | --- | --- | --- |
//! | `CLOUD_HOST` | `ecs.DescribeInstances` | 每地域 1 道 |
//! | `EIP` | `vpc.DescribeEipAddresses` | 每地域 1 道 |
//! | `VPC` | `vpc.DescribeVpcs` | 每地域 1 道 |
//! | `SUB_NET` | `vpc.DescribeVSwitches` | 每地域 1 道 |
//!
//! 与腾讯云的两处差异：EIP 没有 `wanip`/`all` 两路过滤；阿里云没有项目（Project）
//! 维度，实例的 `ResourceGroupId` 落到 `project_id`。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use harvest_core::config::{AllowEmpty, RegionsSpec, ServiceConfig, SourceConfig};
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::model::{AssetType, Coverage, CoverageScope, Vendor};
use harvest_core::ratelimit::{RateLimiter, RateSpec};
use harvest_core::redact;
use harvest_core::scope::Scope;
use harvest_core::source::{FetchStat, LANE_DEFAULT, Source, WorkItem};
use harvest_core::spool::{Manifest, Spool};
use serde_json::Value;

use crate::client::{AliyunClient, ApiAction, ClientOptions, RetrySpec};
use crate::readers::{MapContext, PageSpec, billing, eip, instance, network, paginate};
use crate::value::{as_str, inner_items};

pub const DESCRIBE_REGIONS: ApiAction = ApiAction {
    name: "DescribeRegions",
    host: "ecs.aliyuncs.com",
    version: "2014-05-26",
};

/// 账号身份（`AccountId`）。
///
/// 退役判定靠 `vendor:AccountId` 圈定目标集，所以这个值必须真实、稳定：
/// 拿不到就宁可不声明（下游不退役），也不要拿别的字段顶替。
pub const GET_CALLER_IDENTITY: ApiAction = ApiAction {
    name: "GetCallerIdentity",
    host: "sts.aliyuncs.com",
    version: "2015-04-01",
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionEntry {
    pub code: String,
    pub name: String,
}

pub struct AliyunSource {
    cfg: SourceConfig,
    client: AliyunClient,
    regions: Vec<RegionEntry>,
    page_size: u64,
    allow_empty: AllowEmpty,
    /// 已解析的范围（地域在 `init` 里确定）。
    scope: Scope,
    /// 地域是不是 `auto` 解出来的（账号可见地域全集）。
    regions_auto: bool,
    /// 账号键 `vendor:AccountId`。取不到就 `None`（下游不退役）。
    account: Option<String>,
}

/// 阿里云要求的凭据键（`[credential]` 下的名字）。
pub const REQUIRED_CREDENTIALS: &[&str] = &["access_key_id", "access_key_secret"];

/// `[vendor_options]`：阿里云目前没有额外参数，写任何键都报错（防手误）。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AliyunOptions {}

impl AliyunSource {
    /// 解析凭据、建客户端、解析地域（`regions="auto"` 时调 `DescribeRegions`）。
    pub async fn init(cfg: SourceConfig, svc: &ServiceConfig) -> Result<Self, HarvestError> {
        if cfg.vendor() != Vendor::Aliyun {
            return Err(HarvestError::new(
                ErrorClass::InvalidParam,
                format!("AliyunSource 不能处理 vendor = {}", cfg.vendor().as_str()),
            ));
        }
        cfg.credential.expect_only(REQUIRED_CREDENTIALS)?;
        let _: AliyunOptions = harvest_core::config::parse_vendor_options(&cfg)?;

        let access_key_id = cfg.credential.get("access_key_id")?;
        let access_key_secret = cfg.credential.get("access_key_secret")?;
        // 注册到全局脱敏器：之后任何日志/报告/错误都不可能带出凭据明文。
        redact::register_secret(access_key_id.expose());
        redact::register_secret(access_key_secret.expose());

        let overrides: BTreeMap<String, RateSpec> = svc
            .rate
            .actions
            .iter()
            .map(|(action, spec)| {
                (
                    action.clone(),
                    RateSpec::new(spec.qps, spec.burst.unwrap_or(spec.qps)),
                )
            })
            .collect();
        let limiter = Arc::new(RateLimiter::new(
            RateSpec::new(svc.rate.default_qps, svc.rate.default_burst),
            overrides,
        ));

        let client = AliyunClient::new(
            access_key_id,
            access_key_secret,
            limiter,
            ClientOptions {
                timeout: Duration::from_millis(svc.service.request_timeout_ms),
                max_response_bytes: svc.service.max_response_bytes,
                retry: RetrySpec {
                    max_attempts: svc.retry.max_attempts,
                    base_delay: Duration::from_millis(svc.retry.base_delay_ms),
                    max_delay: Duration::from_millis(svc.retry.max_delay_ms),
                },
                endpoint_override: cfg.endpoint_override.clone(),
            },
        )?;

        let placeholder_scope = cfg.scope(Vec::new());
        let mut source = Self {
            regions_auto: matches!(cfg.regions, RegionsSpec::Auto(_)),
            account: None,
            cfg,
            client,
            regions: Vec::new(),
            page_size: svc.service.page_size,
            allow_empty: svc.service.allow_empty.clone(),
            scope: placeholder_scope,
        };

        source.regions = source.resolve_regions().await?;
        source.account = source.resolve_account().await;
        source.scope = source
            .cfg
            .scope(source.regions.iter().map(|r| r.code.clone()).collect());
        Ok(source)
    }

    /// 当前生效的范围（地域已解析）。
    pub fn regions(&self) -> &[RegionEntry] {
        &self.regions
    }

    pub fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    /// 账号键 `vendor:AccountId`（取不到就是 `None`）。
    pub fn account_key(&self) -> Option<&str> {
        self.account.as_deref()
    }

    async fn resolve_regions(&self) -> Result<Vec<RegionEntry>, HarvestError> {
        match &self.cfg.regions {
            RegionsSpec::Explicit(list) => Ok(list
                .iter()
                .map(|code| RegionEntry {
                    code: code.clone(),
                    name: self.cfg.region_names.get(code).cloned().unwrap_or_default(),
                })
                .collect()),
            RegionsSpec::Auto(_) => {
                let outcome = self.client.call(&DESCRIBE_REGIONS, None, &[]).await?;
                let body = outcome
                    .body
                    .as_object()
                    .ok_or_else(|| HarvestError::data_format("DescribeRegions 响应不是对象"))?;
                let set = inner_items(body, "Regions", "Region").ok_or_else(|| {
                    HarvestError::data_format("DescribeRegions 响应缺少 Regions.Region")
                })?;
                let mut regions: Vec<RegionEntry> = set
                    .into_iter()
                    .filter_map(Value::as_object)
                    .filter_map(|obj| {
                        let code = as_str(obj.get("RegionId"));
                        if code.is_empty() {
                            return None;
                        }
                        Some(RegionEntry {
                            code,
                            name: as_str(obj.get("LocalName")),
                        })
                    })
                    .collect();
                for entry in &mut regions {
                    if let Some(name) = self.cfg.region_names.get(&entry.code) {
                        entry.name = name.clone();
                    }
                }
                regions.sort_by(|a, b| a.code.cmp(&b.code));
                if regions.is_empty() {
                    return Err(HarvestError::integrity(
                        "DescribeRegions 没有返回任何地域（范围无法确定）",
                    ));
                }
                Ok(regions)
            }
        }
    }

    /// 账号身份。**失败不阻断库存**，但必须告警：拿不到账号就等于本轮快照不能用于退役。
    async fn resolve_account(&self) -> Option<String> {
        match self.client.call(&GET_CALLER_IDENTITY, None, &[]).await {
            Ok(outcome) => {
                let account_id = as_str(outcome.body.get("AccountId"));
                if account_id.is_empty() {
                    redact::warn(
                        "GetCallerIdentity 响应缺少 AccountId：本轮快照不会被用于退役判定",
                    );
                    return None;
                }
                Some(format!("{}:{account_id}", self.cfg.vendor().key()))
            }
            Err(err) => {
                redact::warn(format!(
                    "GetCallerIdentity 失败，本轮快照不会被用于退役判定: {err}"
                ));
                None
            }
        }
    }

    fn map_context<'a>(&'a self, region: &'a str) -> MapContext<'a> {
        MapContext {
            region,
            region_name: self
                .regions
                .iter()
                .find(|r| r.code == region)
                .map(|r| r.name.as_str())
                .unwrap_or(""),
        }
    }
}

/// 只依赖配置的计划生成，供 `dry-run` / `diagnose` 用（不联网）。
///
/// 返回空计划表示 `regions = "auto"`（需要联网才知地域），由调用方标注说明。
pub fn plan_from_config(cfg: &SourceConfig) -> Result<Vec<WorkItem>, HarvestError> {
    let regions = match &cfg.regions {
        RegionsSpec::Explicit(list) => list.clone(),
        RegionsSpec::Auto(_) => Vec::new(),
    };
    Ok(plan_items(cfg, &regions))
}

/// 只依赖配置的计划生成。
pub fn plan_items(cfg: &SourceConfig, regions: &[String]) -> Vec<WorkItem> {
    let mut items = Vec::new();
    for region in regions {
        if cfg.assets.cloud_host {
            items.push(WorkItem::new(
                AssetType::CloudHost,
                region.clone(),
                LANE_DEFAULT,
            ));
        }
        if cfg.assets.eip {
            items.push(WorkItem::new(AssetType::Eip, region.clone(), LANE_DEFAULT));
        }
        if cfg.assets.vpc {
            items.push(WorkItem::new(AssetType::Vpc, region.clone(), LANE_DEFAULT));
        }
        if cfg.assets.subnet {
            items.push(WorkItem::new(
                AssetType::SubNet,
                region.clone(),
                LANE_DEFAULT,
            ));
        }
    }
    items
}

#[async_trait]
impl Source for AliyunSource {
    fn vendor(&self) -> Vendor {
        Vendor::Aliyun
    }

    fn scope(&self) -> Scope {
        self.scope.clone()
    }

    fn plan(&self) -> Vec<WorkItem> {
        plan_items(
            &self.cfg,
            &self
                .regions
                .iter()
                .map(|r| r.code.clone())
                .collect::<Vec<_>>(),
        )
    }

    async fn fetch(&self, item: &WorkItem, spool: &mut Spool) -> Result<FetchStat, HarvestError> {
        if item.lane != LANE_DEFAULT {
            return Err(HarvestError::new(
                ErrorClass::Other,
                format!("阿里云没有分页道 `{}`", item.lane),
            )
            .with_region(&item.region));
        }
        let ctx = self.map_context(&item.region);
        let page_size = self.page_size;

        match item.asset_type {
            AssetType::CloudHost => {
                let spec = PageSpec {
                    action: instance::DESCRIBE_INSTANCES,
                    outer_key: instance::OUTER_KEY,
                    inner_key: instance::INNER_KEY,
                    max_page_size: 100,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| instance::map_instance(obj, &ctx),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::Eip => {
                let spec = PageSpec {
                    action: eip::DESCRIBE_EIP_ADDRESSES,
                    outer_key: eip::OUTER_KEY,
                    inner_key: eip::INNER_KEY,
                    max_page_size: eip::MAX_PAGE_SIZE,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| eip::map_address(obj, &ctx),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::Vpc => {
                let spec = PageSpec {
                    action: network::DESCRIBE_VPCS,
                    outer_key: network::VPC_OUTER_KEY,
                    inner_key: network::VPC_INNER_KEY,
                    max_page_size: network::MAX_PAGE_SIZE,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| network::map_vpc(obj, &ctx),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::SubNet => {
                let spec = PageSpec {
                    action: network::DESCRIBE_VSWITCHES,
                    outer_key: network::VSWITCH_OUTER_KEY,
                    inner_key: network::VSWITCH_INNER_KEY,
                    max_page_size: network::MAX_PAGE_SIZE,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| network::map_vswitch(obj, &ctx),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
        }
    }

    fn verify(&self, manifest: &Manifest) -> Result<(), HarvestError> {
        harvest_core::integrity::verify_manifest(manifest, &self.allow_empty).map(|_| ())
    }

    /// 只有 `regions = "auto"`（`DescribeRegions` 拿账号可见地域全集）**且四类资产全采**
    /// 才敢声称账号级全量。
    fn coverage(&self) -> Coverage {
        let all_types = self.cfg.enabled_types().len() == AssetType::ALL.len();
        Coverage {
            scope: if self.regions_auto && all_types {
                CoverageScope::Account
            } else {
                CoverageScope::Partial
            },
            // 由运行器在 verify 通过后置真；这里不做判断。
            complete: false,
            account: self.account.clone(),
        }
    }
}

#[async_trait]
impl harvest_core::source::CostSource for AliyunSource {
    async fn fetch_cost(
        &self,
        range: &harvest_core::cost::CostRange,
        on_row: &mut (dyn FnMut(harvest_core::cost::CostDetail) -> Result<(), HarvestError> + Send),
    ) -> Result<harvest_core::cost::CostFetchStat, HarvestError> {
        let currency = self.cfg.cost.currency.clone();
        let mut out_of_range = 0u64;
        let mut stat = billing::fetch_range(&self.client, range, |row| {
            match billing::map_detail(row, range, &currency)? {
                Some(detail) => on_row(detail),
                // 整月拉取、按日筛选时的常态。
                None => {
                    out_of_range += 1;
                    Ok(())
                }
            }
        })
        .await?;
        stat.out_of_range = out_of_range;
        Ok(stat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SourceConfig {
        SourceConfig::parse(
            r#"
source_code = "aliyun-prod"
vendor = "ALIYUN"
regions = ["cn-hangzhou", "cn-beijing"]

[credential]
access_key_id = { env = "SEC_ALIYUN_ACCESS_KEY_ID" }
access_key_secret = { env = "SEC_ALIYUN_ACCESS_KEY_SECRET" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn plan_has_one_lane_per_type_per_region() {
        let items = plan_items(
            &cfg(),
            &["cn-hangzhou".to_string(), "cn-beijing".to_string()],
        );
        assert_eq!(items.len(), 2 * 4, "每地域：主机1 + EIP1 + VPC1 + 子网1");
        assert!(items.iter().all(|item| item.lane == LANE_DEFAULT));
    }

    #[test]
    fn disabled_types_are_not_planned() {
        let mut cfg = cfg();
        cfg.assets.eip = false;
        cfg.assets.subnet = false;
        let items = plan_items(&cfg, &["cn-hangzhou".to_string()]);
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn wrong_vendor_is_rejected() {
        let raw = r#"
source_code = "tencent-prod"
vendor = "TENCENT_CLOUD"

[credential]
secret_id = { env = "A" }
secret_key = { env = "B" }
"#;
        let cfg = SourceConfig::parse(raw).unwrap();
        let err = match AliyunSource::init(cfg, &ServiceConfig::parse("").unwrap()).await {
            Ok(_) => panic!("vendor 不匹配时初始化应当失败"),
            Err(err) => err,
        };
        assert!(err.message.contains("vendor"), "{}", err.message);
    }
}
