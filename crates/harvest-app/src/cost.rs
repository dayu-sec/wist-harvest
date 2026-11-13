//! `cost`：抓一个账期范围，落分片 + manifest（契约 `harvest-cost-v1`）。
//!
//! 与 `once` 的区别：成本**不走数据平面**（设计说明 §7.1）。这里是「云 → 对象存储」
//! 的一条独立主干，`wist-cost` 在计算区按 manifest 入账，atlas 根本不参与。
//!
//! 一次只处理**一个范围**（七元组）。18 个月回填 = 跑 18 次（外层脚本或调度器的事）：
//! 把范围选择留在命令行，是为了让「跑哪个月、哪个口径」这件事永远是显式的。

use std::path::PathBuf;

use chrono::{Datelike, FixedOffset, NaiveDate, Utc};
use harvest_core::cost::{
    COST_MANIFEST_SCHEMA, CostBasis, CostRange, CostShardWriter, PeriodSource,
};
use harvest_core::error::HarvestError;
use harvest_core::model::Vendor;
use serde_json::json;

use crate::round::RoundConfig;

/// 跑一次成本采集，返回进程退出码。
pub async fn run(
    config_path: PathBuf,
    source_path: PathBuf,
    month: Option<String>,
    day: Option<String>,
    output_dir: Option<PathBuf>,
) -> i32 {
    let cfg = match RoundConfig::load(&config_path, &source_path) {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("{message}");
            return 3;
        }
    };

    let range = match build_range(&cfg, month.as_deref(), day.as_deref()) {
        Ok(range) => range,
        Err(message) => {
            eprintln!("账期范围不可用: {message}");
            return 3;
        }
    };

    let source = match crate::vendor::build_cost_source(&cfg.source, &cfg.service).await {
        Ok(source) => source,
        Err(err) => {
            eprintln!("数据源初始化失败: {err}");
            return 3;
        }
    };

    let root = output_dir
        .or_else(|| cfg.source.cost.output_dir.clone())
        .unwrap_or_else(|| cfg.service.service.state_dir.join("cost"));
    let fetched_at = Utc::now().to_rfc3339();
    let batch_id = format!(
        "{}:cost-{}",
        range.source_code,
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );

    let mut writer = CostShardWriter::new(
        &root,
        range.clone(),
        cfg.source.cost.currency.clone(),
        PeriodSource::CloudApi,
        fetched_at,
        batch_id.clone(),
    )
    .with_rows_per_shard(cfg.source.cost.rows_per_shard);

    let mut on_row = |detail: harvest_core::cost::CostDetail| -> Result<(), HarvestError> {
        writer.push(detail)
    };
    let stat = match source.fetch_cost(&range, &mut on_row).await {
        Ok(stat) => stat,
        Err(err) => {
            // 失败就**不发布**：分片只在暂存区，已发布的批次一字未动。
            eprintln!("抓取失败（未发布：既有 manifest 与分片保持不变）: {err}");
            return 2;
        }
    };
    writer.set_request_ids(stat.request_ids.clone());
    let out_of_range = stat.out_of_range;

    let (manifest_path, manifest) = match writer.finish() {
        Ok(done) => done,
        Err(err) => {
            eprintln!("发布成本分片/manifest 失败（旧批次保持有效）: {err}");
            return 2;
        }
    };
    assert_eq!(manifest.schema, COST_MANIFEST_SCHEMA);

    let out = json!({
        "command": "cost",
        "batch_id": batch_id,
        "range": {
            "source_code": range.source_code,
            "vendor": range.vendor.as_str(),
            "billing_account_uin": range.billing_account_uin,
            "cost_basis": range.cost_basis.as_str(),
            "period_start": range.period_start.to_string(),
            "period_end": range.period_end.to_string(),
        },
        "manifest": manifest_path.display().to_string(),
        "object_root": root.display().to_string(),
        "shards": manifest.shards.len(),
        "rows": manifest.total_rows,
        "fetched_rows": stat.rows,
        "out_of_range_rows": out_of_range,
        "pages": stat.pages,
        "retries": stat.retries,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}

/// 从命令行参数构造范围。
///
/// - `--day YYYY-MM-DD` → `CONSUMPTION`（单日）
/// - `--month YYYY-MM` → `CHARGE`（整月）
/// - 都不给 → 上一个自然月的 `CHARGE`（账单日是月初，默认行为应当是「结算上个月」）
fn build_range(
    cfg: &RoundConfig,
    month: Option<&str>,
    day: Option<&str>,
) -> Result<CostRange, String> {
    if month.is_some() && day.is_some() {
        return Err("--month 与 --day 互斥（两个口径是不同范围，不能混）".to_string());
    }
    let billing_account_uin = billing_account_uin(cfg)?;
    let (cost_basis, period_start, period_end) = match (month, day) {
        (Some(month), None) => {
            let start = parse_month_start(month)?;
            let end = NaiveDate::from_ymd_opt(start.year(), start.month(), days_in_month(start))
                .ok_or_else(|| format!("{month} 的月末日不可用"))?;
            (CostBasis::Charge, start, end)
        }
        (None, Some(day)) => {
            let date = NaiveDate::parse_from_str(day, "%Y-%m-%d")
                .map_err(|err| format!("--day 必须是 YYYY-MM-DD: {err}"))?;
            (CostBasis::Consumption, date, date)
        }
        (None, None) => {
            // **按北京时间**算「上个月」：默认值就是给「月初结算上个月」用的，
            // 而 UTC 在月初 00:00–08:00 还停在上个月末，会把默认账期又退一个月。
            let (start, end) = previous_month_range(Utc::now());
            (CostBasis::Charge, start, end)
        }
        (Some(_), Some(_)) => unreachable!("已在上面拦掉"),
    };

    let range = CostRange {
        source_code: cfg.source.source_code.clone(),
        vendor: Vendor::TencentCloud,
        billing_account_uin,
        cost_basis,
        period_start,
        period_end,
    };
    range.validate().map_err(|err| err.to_string())?;
    Ok(range)
}

/// 账单口径是 UIN。账号键是 `vendor:uin`，这里取后段。
fn billing_account_uin(cfg: &RoundConfig) -> Result<String, String> {
    // 数据源里显式写了就用它（联调 / 代客采集时账号键与凭据可能不同）。
    if let Some(uin) = &cfg.source.billing_account {
        return Ok(uin.clone());
    }
    Err(
        "数据源没有 `billing_account`：账单接口按账号记账（腾讯云是 UIN），不能从凭据里猜。\
         请在 sources.d/<source>.toml 里写上 `billing_account = \"1000...\"`"
            .to_string(),
    )
}

fn parse_month_start(month: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")
        .map_err(|err| format!("--month 必须是 YYYY-MM: {err}"))
}

/// 「上一个自然月」，按 Asia/Shanghai 挂钟时间算。
///
/// 用 UTC 算会在月初 00:00–08:00 CST 错一个月（那时 UTC 还是上个月末）。
fn previous_month_range(now: chrono::DateTime<Utc>) -> (NaiveDate, NaiveDate) {
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00 是合法偏移");
    let today = now.with_timezone(&offset).date_naive();
    let (year, month) = if today.month() == 1 {
        (today.year() - 1, 12)
    } else {
        (today.year(), today.month() - 1)
    };
    let start = NaiveDate::from_ymd_opt(year, month, 1).expect("上个月首日必然存在");
    let end =
        NaiveDate::from_ymd_opt(year, month, days_in_month(start)).expect("上个月末日必然存在");
    (start, end)
}

fn days_in_month(start: NaiveDate) -> u32 {
    let (year, month) = (start.year(), start.month());
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("下个月一定存在");
    next.pred_opt().expect("每月首日都有前一天").day()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_option_is_charge_basis_covering_the_whole_month() {
        let start = parse_month_start("2026-10").unwrap();
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        assert_eq!(days_in_month(start), 31);
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()),
            28
        );
        assert_eq!(
            days_in_month(NaiveDate::from_ymd_opt(2028, 2, 1).unwrap()),
            29
        );
    }

    #[test]
    fn a_malformed_month_is_rejected() {
        assert!(parse_month_start("2026-13").is_err());
        assert!(parse_month_start("202610").is_err());
    }

    #[test]
    fn default_month_follows_the_shanghai_wall_clock() {
        let at = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&Utc)
        };
        // UTC 还是 10-31 23:30，北京时间已经是 11-01 07:30：上个月应当是 10 月。
        let (start, end) = previous_month_range(at("2026-10-31T23:30:00Z"));
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 10, 31).unwrap());

        // 跨年：北京时间 2026-01-01 01:00 → 上个月是 2025-12。
        let (start, end) = previous_month_range(at("2025-12-31T17:00:00Z"));
        assert_eq!(start, NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2025, 12, 31).unwrap());
    }
}
