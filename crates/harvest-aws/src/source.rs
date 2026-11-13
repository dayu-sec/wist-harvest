//! AWS 数据源：`ec2`（库存）+ `sts`（账号身份）。
//!
//! 与腾讯云的分工完全一致：这一层只负责「AWS 的 API 怎么调、字段怎么归一」，
//! 状态机 / spool / 完整性框架 / 发布都在 `harvest-core`。

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
use serde::Deserialize;

use crate::client::{
    ApiAction, AwsClient, ClientOptions, DESCRIBE_ADDRESSES, DESCRIBE_INSTANCES, DESCRIBE_REGIONS,
    DESCRIBE_SUBNETS, DESCRIBE_VPCS, GET_CALLER_IDENTITY, RetrySpec,
};
use crate::readers::{AwsPageSpec, address, identity, instance, network, paginate, region};

/// AWS 要求的凭据键（`session_token` 可选，不在这里）。
pub const REQUIRED_CREDENTIALS: &[&str] = &["access_key_id", "secret_access_key"];

/// `[vendor_options]`：AWS 专属参数。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AwsOptions {
    /// `aws`（默认）/ `aws-cn` / `aws-us-gov`。决定域名后缀。
    #[serde(default)]
    partition: Option<String>,
}

pub struct AwsSource {
    cfg: SourceConfig,
    client: AwsClient,
    regions: Vec<region::RegionEntry>,
    scope: Scope,
    page_size: u64,
    allow_empty: AllowEmpty,
    regions_auto: bool,
    account: Option<String>,
    /// `regions="auto"` 时用来拼第一个 DescribeRegions 端点的地域。
    ///
    /// 不能写死 `us-east-1`：`aws-cn` / `aws-us-gov` 分区里没有这个地域。
    default_region: String,
}

impl AwsSource {
    pub async fn init(cfg: SourceConfig, svc: &ServiceConfig) -> Result<Self, HarvestError> {
        if cfg.vendor() != Vendor::Aws {
            return Err(HarvestError::new(
                ErrorClass::InvalidParam,
                format!("AwsSource 不能处理 vendor = {}", cfg.vendor().as_str()),
            ));
        }
        cfg.credential
            .expect_only(&["access_key_id", "secret_access_key", "session_token"])?;

        let options: AwsOptions = harvest_core::config::parse_vendor_options(&cfg)?;
        let partition = options.partition.unwrap_or_else(|| "aws".to_string());
        let default_region = match partition.as_str() {
            "aws" => "us-east-1",
            "aws-cn" => "cn-north-1",
            "aws-us-gov" => "us-gov-west-1",
            other => {
                return Err(HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("[vendor_options].partition 不可用: {other}"),
                ));
            }
        }
        .to_string();

        let access_key_id = cfg.credential.get("access_key_id")?;
        let secret_access_key = cfg.credential.get("secret_access_key")?;
        let session_token = cfg.credential.get_optional("session_token")?;
        for secret in [
            Some(&access_key_id),
            Some(&secret_access_key),
            session_token.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            redact::register_secret(secret.expose());
        }

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

        let client = AwsClient::new(
            access_key_id,
            secret_access_key,
            session_token,
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
                partition,
            },
        )?;

        let placeholder_scope = cfg.scope(Vec::new());
        let mut source = Self {
            regions_auto: matches!(cfg.regions, RegionsSpec::Auto(_)),
            account: None,
            cfg,
            client,
            regions: Vec::new(),
            scope: placeholder_scope,
            page_size: svc.service.page_size,
            allow_empty: svc.service.allow_empty.clone(),
            default_region,
        };

        source.regions = source.resolve_regions().await?;
        source.account = source.resolve_account().await;
        source.scope = source
            .cfg
            .scope(source.regions.iter().map(|r| r.code.clone()).collect());
        Ok(source)
    }

    pub fn regions(&self) -> &[region::RegionEntry] {
        &self.regions
    }

    pub fn account_key(&self) -> Option<&str> {
        self.account.as_deref()
    }

    async fn resolve_regions(&self) -> Result<Vec<region::RegionEntry>, HarvestError> {
        match &self.cfg.regions {
            RegionsSpec::Explicit(list) => Ok(list
                .iter()
                .map(|code| region::RegionEntry {
                    code: code.clone(),
                    name: self.cfg.region_names.get(code).cloned().unwrap_or_default(),
                })
                .collect()),
            RegionsSpec::Auto(_) => {
                let outcome = self
                    .client
                    .call(&DESCRIBE_REGIONS, self.cfg_regions_region_hint(), &[])
                    .await?;
                let mut regions = region::map_regions(&outcome.body)?;
                for entry in &mut regions {
                    if let Some(name) = self.cfg.region_names.get(&entry.code) {
                        entry.name = name.clone();
                    }
                }
                Ok(regions)
            }
        }
    }

    /// `DescribeRegions` 也需要一个 region 来拼端点；显式地域用第一个，否则用分区默认地域。
    fn cfg_regions_region_hint(&self) -> &str {
        match &self.cfg.regions {
            RegionsSpec::Explicit(list) => list
                .first()
                .map(String::as_str)
                .unwrap_or(&self.default_region),
            RegionsSpec::Auto(_) => &self.default_region,
        }
    }

    /// 账号身份。失败不阻断库存，但必须告警（拿不到账号的帧不能用于退役）。
    async fn resolve_account(&self) -> Option<String> {
        let region = self
            .regions
            .first()
            .map(|entry| entry.code.clone())
            .unwrap_or_else(|| self.default_region.clone());
        match self.client.call(&GET_CALLER_IDENTITY, &region, &[]).await {
            Ok(outcome) => match identity::map_account(&outcome.body) {
                Some(account) => Some(format!("{}:{account}", Vendor::Aws.key())),
                None => {
                    redact::warn("GetCallerIdentity 响应缺少 Account：本轮快照不会被用于退役判定");
                    None
                }
            },
            Err(err) => {
                redact::warn(format!(
                    "GetCallerIdentity 失败，本轮快照不会被用于退役判定: {err}"
                ));
                None
            }
        }
    }

    fn region_name(&self, region: &str) -> &str {
        self.regions
            .iter()
            .find(|entry| entry.code == region)
            .map(|entry| entry.name.as_str())
            .unwrap_or("")
    }

    fn page_spec<'a>(&self, action: ApiAction, region: &'a str) -> AwsPageSpec<'a> {
        AwsPageSpec::new(action, region).with_page_size(self.page_size)
    }
}

/// 只依赖配置的计划生成，供 `dry-run` / `diagnose` 用（不联网）。
pub fn plan_from_config(cfg: &SourceConfig) -> Result<Vec<WorkItem>, HarvestError> {
    let regions = match &cfg.regions {
        RegionsSpec::Explicit(list) => list.clone(),
        RegionsSpec::Auto(_) => Vec::new(),
    };
    Ok(plan_items(cfg, &regions))
}

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
            // AWS 只有一路（没有腾讯云的 wanip/all 两道过滤）。
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
impl Source for AwsSource {
    fn vendor(&self) -> Vendor {
        Vendor::Aws
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
                format!("AWS 没有分页道 `{}`", item.lane),
            )
            .with_region(&item.region));
        }
        let region = item.region.as_str();
        let region_name = self.region_name(region);

        match item.asset_type {
            AssetType::CloudHost => {
                let spec = self.page_spec(DESCRIBE_INSTANCES, region);
                paginate(
                    &self.client,
                    spec,
                    |xml| instance::map_instances(xml, region, region_name),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::Eip => {
                // `DescribeAddresses` 不分页，也没有 MaxResults。
                let spec = AwsPageSpec::new(DESCRIBE_ADDRESSES, region);
                paginate(
                    &self.client,
                    spec,
                    |xml| address::map_addresses(xml, region),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::Vpc => {
                let spec = self.page_spec(DESCRIBE_VPCS, region);
                paginate(
                    &self.client,
                    spec,
                    |xml| network::map_vpcs(xml, region, region_name),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
            AssetType::SubNet => {
                let spec = self.page_spec(DESCRIBE_SUBNETS, region);
                paginate(
                    &self.client,
                    spec,
                    |xml| network::map_subnets(xml, region, region_name),
                    |rows| spool.append_rows(item, rows).map_err(HarvestError::from),
                )
                .await
            }
        }
    }

    fn verify(&self, manifest: &Manifest) -> Result<(), HarvestError> {
        harvest_core::integrity::verify_manifest(manifest, &self.allow_empty).map(|_| ())
    }

    fn coverage(&self) -> Coverage {
        let all_types = self.cfg.enabled_types().len() == AssetType::ALL.len();
        Coverage {
            scope: if self.regions_auto && all_types {
                CoverageScope::Account
            } else {
                CoverageScope::Partial
            },
            complete: false,
            account: self.account.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SourceConfig {
        SourceConfig::parse(
            r#"
source_code = "aws-prod"
vendor = "aws"
regions = ["us-east-1", "ap-east-1"]

[credential]
access_key_id = { env = "SEC_AWS_ACCESS_KEY_ID" }
secret_access_key = { env = "SEC_AWS_SECRET_ACCESS_KEY" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn plan_has_one_eip_lane_per_region() {
        let items = plan_items(&cfg(), &["us-east-1".to_string()]);
        assert_eq!(items.len(), 4, "每地域：主机1 + EIP1 + VPC1 + 子网1");
        let eip_lanes: Vec<&str> = items
            .iter()
            .filter(|item| item.asset_type == AssetType::Eip)
            .map(|item| item.lane.as_str())
            .collect();
        assert_eq!(eip_lanes, vec![LANE_DEFAULT]);
    }

    #[test]
    fn disabled_types_are_not_planned() {
        let mut cfg = cfg();
        cfg.assets.eip = false;
        let items = plan_items(&cfg, &["us-east-1".to_string()]);
        assert_eq!(items.len(), 3);
    }
}
