//! `dry-run`：打印生效配置与采集计划。
//!
//! 刻意**不联网、不写 spool**：切换闸门要做"同窗口字段级 diff"时，需要的是
//! "会采什么"而不是"再采一遍"。`regions = "auto"` 无法离线解析，就明确说
//! 明"运行时解析"，不猜。

use std::path::Path;

use harvest_core::config::{RegionsSpec, ServiceConfig, SourceConfig};
use serde_json::json;

use crate::vendor;

pub fn run(config_path: &Path, source_path: &Path) -> i32 {
    let svc = match ServiceConfig::load(config_path) {
        Ok(svc) => svc,
        Err(err) => {
            eprintln!("服务配置不可用: {err}");
            return 3;
        }
    };
    let source_cfg = match SourceConfig::load(source_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("数据源配置不可用: {err}");
            return 3;
        }
    };

    let (regions, regions_note) = match &source_cfg.regions {
        RegionsSpec::Explicit(list) => (list.clone(), json!(null)),
        RegionsSpec::Auto(_) => (
            Vec::new(),
            json!("regions = \"auto\"：运行时用 DescribeRegions 解析，dry-run 不联网"),
        ),
    };

    let items = match vendor::plan_from_config(&source_cfg) {
        Ok(items) => items,
        Err(err) => {
            eprintln!("采集计划不可用: {err}");
            return 3;
        }
    };
    let plan = items
        .iter()
        .map(|item| {
            json!({
                "asset_type": item.asset_type.as_str(),
                "region": item.region,
                "lane": item.lane,
            })
        })
        .collect::<Vec<_>>();

    let out = json!({
        "service": {
            "spool_dir": svc.service.spool_dir.display().to_string(),
            "state_dir": svc.service.state_dir.display().to_string(),
            "page_size": svc.service.page_size,
            "max_frame_bytes": svc.service.max_frame_bytes,
            "max_response_bytes": svc.service.max_response_bytes,
            "request_timeout_ms": svc.service.request_timeout_ms,
            "allow_empty": svc.service.allow_empty.describe(),
            "concurrency": svc.service.concurrency,
        },
        "retry": {
            "max_attempts": svc.retry.max_attempts,
            "base_delay_ms": svc.retry.base_delay_ms,
            "max_delay_ms": svc.retry.max_delay_ms,
        },
        "sink": {
            "endpoint": svc.sink.endpoint,
            "tls": !svc.sink.allow_plaintext,
            "mtls": svc.sink.cert_file.is_some() && svc.sink.key_file.is_some(),
        },
        "source": {
            "source_code": source_cfg.source_code,
            "group": source_cfg.group,
            "env_ref": source_cfg.env_ref,
            "producer": format!("{}:{}", source_cfg.source_code, source_cfg.group),
            "vendor": source_cfg.vendor().as_str(),
            "regions": regions,
            "regions_note": regions_note,
            "assets": {
                "cloud_host": source_cfg.assets.cloud_host,
                "eip": source_cfg.assets.eip,
                "vpc": source_cfg.assets.vpc,
                "subnet": source_cfg.assets.subnet,
            },
        },
        "credential": source_cfg.credential.describe(),
        "work_items": plan.len(),
        "plan": plan,
    });

    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}
