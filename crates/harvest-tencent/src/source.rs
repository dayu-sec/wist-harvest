//! 腾讯云数据源：把四个 reader 组装成 [`Source`]。
//!
//! 采集组 `inventory` 的四类资产：
//!
//! | 类型 | API | 分页道 |
//! | --- | --- | --- |
//! | `CLOUD_HOST` | `cvm.DescribeInstances` | 每地域 1 道 |
//! | `EIP` | `vpc.DescribeAddresses` | 每地域 2 道（`wanip` / `all`） |
//! | `VPC` | `vpc.DescribeVpcs` | 每地域 1 道 |
//! | `SUB_NET` | `vpc.DescribeSubnets` | 每地域 1 道 |

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
use harvest_core::source::{FetchStat, LANE_ALL, LANE_DEFAULT, LANE_WANIP, Source, WorkItem};
use harvest_core::spool::{Manifest, Spool};
use serde_json::{Value, json};

use crate::client::{ApiAction, ClientOptions, RetrySpec, TencentClient};
use crate::readers::{MapContext, PageSpec, billing, cvm, eip, network, paginate};
use crate::value::{as_i64, as_str};

pub const DESCRIBE_REGIONS: ApiAction = ApiAction {
    name: "DescribeRegions",
    host: "cvm.tencentcloudapi.com",
    version: "2017-03-12",
    service: "cvm",
};

pub const DESCRIBE_PROJECTS: ApiAction = ApiAction {
    name: "DescribeProjects",
    host: "tag.tencentcloudapi.com",
    version: "2018-08-13",
    service: "tag",
};

/// 账号身份（`AccountId`）。
///
/// 退役判定靠 `vendor:AccountId` 圈定目标集，所以这个值必须真实、稳定：
/// 拿不到就宁可不声明（下游不退役），也不要拿 `source_code` 之类的东西顶替。
pub const GET_CALLER_IDENTITY: ApiAction = ApiAction {
    name: "GetCallerIdentity",
    host: "sts.tencentcloudapi.com",
    version: "2018-08-13",
    service: "sts",
};

/// 项目列表接口一次能取的最大条数。
const PROJECT_PAGE: u64 = 1000;
/// 项目分页的防御上限。
const PROJECT_MAX_PAGES: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionEntry {
    pub code: String,
    pub name: String,
}

pub struct TencentSource {
    cfg: SourceConfig,
    client: TencentClient,
    regions: Vec<RegionEntry>,
    projects: BTreeMap<String, String>,
    page_size: u64,
    allow_empty: AllowEmpty,
    /// 地域是不是 `auto` 解出来的（账号可见地域全集）。
    ///
    /// 它决定这一帧能不能声明 `coverage.scope = account`：手写地域子集时，
    /// 「某个实例不在结果里」完全可能是「它在另一个地域」。
    regions_auto: bool,
    /// 账号键 `vendor:AccountId`。取不到就 `None`（下游不退役）。
    account: Option<String>,
}

impl TencentSource {
    /// 解析凭据、建客户端、解析地域（`regions="auto"` 时调 `DescribeRegions`）
    /// 与项目名称表（失败不阻断库存）。
    pub async fn init(cfg: SourceConfig, svc: &ServiceConfig) -> Result<Self, HarvestError> {
        let secret_id = cfg.credential.secret_id.load()?;
        let secret_key = cfg.credential.secret_key.load()?;
        // 注册到全局脱敏器：之后任何日志/报告/错误都不可能带出凭据明文。
        redact::register_secret(secret_id.expose());
        redact::register_secret(secret_key.expose());

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

        let client = TencentClient::new(
            secret_id,
            secret_key,
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

        let mut source = Self {
            regions_auto: matches!(cfg.regions, RegionsSpec::Auto(_)),
            account: None,
            cfg,
            client,
            regions: Vec::new(),
            projects: BTreeMap::new(),
            page_size: svc.service.page_size,
            allow_empty: svc.service.allow_empty.clone(),
        };

        source.regions = source.resolve_regions().await?;
        source.projects = source.resolve_projects().await;
        source.account = source.resolve_account().await;
        Ok(source)
    }

    /// 当前生效的范围（地域已解析）。
    pub fn scope(&self) -> Scope {
        self.cfg
            .scope(self.regions.iter().map(|r| r.code.clone()).collect())
    }

    pub fn regions(&self) -> &[RegionEntry] {
        &self.regions
    }

    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    pub fn config(&self) -> &SourceConfig {
        &self.cfg
    }

    /// 账号键 `vendor:AccountId`（取不到就是 `None`）。
    pub fn account_key(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// 抓一个成本范围，逐行交给 `on_row`（有界内存，边抓边写分片）。
    ///
    /// 与资产采集分开：成本不走 spool / 数据平面，也不吃 `verify` 那套「帧完整性」
    /// （分片自身的 `sha256` + 行数就是它的完整性证据）。
    ///
    /// `billing_account_uin` 必须由 `sources.d` 里显式配置：账单接口按 UIN 记账，
    /// 跟凭据里的 SecretId 没有推导关系，不能猜（见 `harvest-app::cost`）。
    pub async fn fetch_cost<F>(
        &self,
        range: &harvest_core::cost::CostRange,
        mut on_row: F,
    ) -> Result<billing::BillFetchStat, HarvestError>
    where
        F: FnMut(&serde_json::Map<String, serde_json::Value>) -> Result<(), HarvestError>,
    {
        billing::fetch_range(&self.client, range, &mut on_row).await
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
                let outcome = self
                    .client
                    .call(&DESCRIBE_REGIONS, None, &json!({}))
                    .await?;
                let set = outcome
                    .body
                    .get("RegionSet")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        HarvestError::data_format("DescribeRegions 响应缺少 RegionSet")
                    })?;
                let mut regions: Vec<RegionEntry> = set
                    .iter()
                    .filter_map(|item| {
                        let obj = item.as_object()?;
                        let state = as_str(obj.get("RegionState"));
                        // 兼容：没有状态字段时按可用处理，不静默丢掉地域。
                        if !state.is_empty() && state != "AVAILABLE" {
                            return None;
                        }
                        let code = as_str(obj.get("Region"));
                        if code.is_empty() {
                            return None;
                        }
                        Some(RegionEntry {
                            code,
                            name: as_str(obj.get("RegionName")),
                        })
                    })
                    .collect();
                regions.sort_by(|a, b| a.code.cmp(&b.code));
                if regions.is_empty() {
                    return Err(HarvestError::integrity(
                        "DescribeRegions 没有返回任何可用地域（范围无法确定）",
                    ));
                }
                Ok(regions)
            }
        }
    }

    /// 账号身份。**失败不阻断库存**，但必须告警：拿不到账号就等于本轮快照不能用于退役，
    /// 已销毁的实例会一直留在目录里。
    async fn resolve_account(&self) -> Option<String> {
        // `sts:GetCallerIdentity` 要求带地域（走 `X-TC-Region` 头）；它跟账号无关，
        // 给一个已解析的地域就行。这里用第一个，不再多存一份配置。
        let region = self.regions.first().map(|entry| entry.code.as_str());
        match self
            .client
            .call(&GET_CALLER_IDENTITY, region, &json!({}))
            .await
        {
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

    /// 项目名称表。文档明确：失败不阻断库存，保留 project_id、名称置空。
    async fn resolve_projects(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let mut offset: u64 = 0;
        let mut pages: u64 = 0;
        loop {
            let body = json!({ "AllList": 1, "Limit": PROJECT_PAGE, "Offset": offset });
            match self.client.call(&DESCRIBE_PROJECTS, None, &body).await {
                Ok(outcome) => {
                    let items = outcome
                        .body
                        .get("Projects")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let total = as_i64(outcome.body.get("TotalCount"))
                        .unwrap_or(items.len() as i64)
                        .max(0) as u64;
                    for item in &items {
                        if let Some(obj) = item.as_object() {
                            let id = as_str(obj.get("ProjectId"));
                            if !id.is_empty() {
                                map.insert(id, as_str(obj.get("ProjectName")));
                            }
                        }
                    }
                    offset += items.len() as u64;
                    pages += 1;
                    if items.is_empty() || offset >= total || pages >= PROJECT_MAX_PAGES {
                        break;
                    }
                }
                Err(err) => {
                    redact::warn(format!(
                        "项目名称获取失败，保留 project_id、名称置空（不阻断库存）: {err}"
                    ));
                    break;
                }
            }
        }
        map
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
            projects: &self.projects,
        }
    }
}

/// 只依赖配置的计划生成，供 `dry-run` 用（不联网）。
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
            // 每地域两路：一路带 address-type=WanIP 过滤，一路不过滤。
            items.push(WorkItem::new(AssetType::Eip, region.clone(), LANE_WANIP));
            items.push(WorkItem::new(AssetType::Eip, region.clone(), LANE_ALL));
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
impl Source for TencentSource {
    fn vendor(&self) -> Vendor {
        Vendor::TencentCloud
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
        let ctx = self.map_context(&item.region);
        let page_size = self.page_size;

        match item.asset_type {
            AssetType::CloudHost => {
                let spec = PageSpec {
                    action: cvm::DESCRIBE_INSTANCES,
                    set_key: cvm::SET_KEY,
                    limit_as_string: false,
                    extra_body: None,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| cvm::map_instance(obj, &ctx),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::Eip => {
                let extra_body = match item.lane.as_str() {
                    LANE_WANIP => Some(json!({ "Filters": eip::wanip_filter() })),
                    LANE_ALL => None,
                    other => {
                        return Err(HarvestError::new(
                            ErrorClass::Other,
                            format!("未知的 EIP 分页道: {other}"),
                        )
                        .with_region(&item.region));
                    }
                };
                let spec = PageSpec {
                    action: eip::DESCRIBE_ADDRESSES,
                    set_key: eip::SET_KEY,
                    limit_as_string: false,
                    extra_body,
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
                    set_key: network::VPC_SET_KEY,
                    // 沿用既有实现：VPC 分页用字符串形式的 Limit/Offset。
                    limit_as_string: true,
                    extra_body: None,
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
                    action: network::DESCRIBE_SUBNETS,
                    set_key: network::SUBNET_SET_KEY,
                    limit_as_string: true,
                    extra_body: None,
                };
                paginate(
                    &self.client,
                    &spec,
                    &item.region,
                    page_size,
                    |obj| network::map_subnet(obj, &ctx),
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
    ///
    /// 手写地域子集时，「某台机不在结果里」完全可能是「它在另一个地域」——
    /// 那种帧只能 `partial`，否则下游会把采集范围当销毁证据。
    ///
    /// 关掉某一类（`assets.<type> = false`）同理：账号级全集的替换语义不含类型，
    /// 少一类就变成子集。这条收敛在 `run::fill_and_publish` 里也会按实际计划再做一次，
    /// 这里写清楚是为了让 `Source` 自己也不撒谎。
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

/// 供报告/诊断使用的凭据描述（不含值）。
pub fn credential_report(cfg: &SourceConfig) -> Value {
    json!({
        "secret_id": cfg.credential.secret_id.describe(),
        "secret_key": cfg.credential.secret_key.describe(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SourceConfig {
        SourceConfig::parse(
            r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
regions = ["ap-guangzhou", "ap-shanghai"]

[credential]
secret_id = { env = "SEC_TENCENT_SECRET_ID" }
secret_key = { env = "SEC_TENCENT_SECRET_KEY" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn plan_has_two_eip_lanes_per_region() {
        let items = plan_items(
            &cfg(),
            &["ap-guangzhou".to_string(), "ap-shanghai".to_string()],
        );
        assert_eq!(items.len(), 2 * 5, "每地域：主机1 + EIP2 + VPC1 + 子网1");
        let eip_lanes: Vec<&str> = items
            .iter()
            .filter(|i| i.asset_type == AssetType::Eip)
            .map(|i| i.lane.as_str())
            .collect();
        assert_eq!(eip_lanes, vec!["wanip", "all", "wanip", "all"]);
    }

    #[test]
    fn disabled_types_are_not_planned() {
        let mut cfg = cfg();
        cfg.assets.eip = false;
        cfg.assets.subnet = false;
        let items = plan_items(&cfg, &["ap-guangzhou".to_string()]);
        assert_eq!(items.len(), 2);
    }
}
