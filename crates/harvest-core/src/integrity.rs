//! 完整性校验：**不完整不发**。
//!
//! 判据（设计说明 §5 `Verified` 行）：
//!
//! 1. 计划内每个工作项都完成；
//! 2. 每项的实收行数与云端声明的 `TotalCount` 一致（分页总数不一致立刻失败）；
//! 3. 页数连续（至少 1 页，且抓取时逐页递增到 `TotalCount`）；
//! 4. 地域齐全（工作项覆盖的地域集合 == 计划地域集合）；
//! 5. 空 ≠ 失败：只有显式允许时才接受某类型 0 行。
//!
//! ID 去重不在这里做：需要真实行内容，见 [`crate::assemble`]。

use std::collections::BTreeMap;

use crate::error::HarvestError;
use crate::model::AssetType;
use crate::spool::Manifest;

#[derive(Debug, Clone, Default)]
pub struct VerifySummary {
    pub rows_by_type: BTreeMap<AssetType, u64>,
    pub item_count: usize,
    pub page_count: u64,
}

pub fn verify_manifest(
    manifest: &Manifest,
    allow_empty: bool,
) -> Result<VerifySummary, HarvestError> {
    if manifest.items.is_empty() {
        return Err(HarvestError::integrity("manifest 里没有任何工作项"));
    }

    // 1. 全部完成
    for record in &manifest.items {
        if !record.completed {
            return Err(HarvestError::integrity(format!(
                "工作项未完成: {}（{}.{}）",
                record.asset_type, record.region, record.lane
            ))
            .with_region(record.region.clone()));
        }
    }

    // 2 & 3. 总数一致 + 页数连续
    let mut summary = VerifySummary {
        item_count: manifest.items.len(),
        ..Default::default()
    };
    for record in &manifest.items {
        if record.pages == 0 {
            return Err(HarvestError::integrity(format!(
                "工作项 0 页: {}（{}.{}）",
                record.asset_type, record.region, record.lane
            ))
            .with_region(record.region.clone()));
        }
        if let Some(expected) = record.expected_rows
            && expected != record.rows
        {
            return Err(HarvestError::integrity(format!(
                "分页总数不一致: {}（{}.{}）云端 TotalCount={expected}，实收={}",
                record.asset_type, record.region, record.lane, record.rows
            ))
            .with_region(record.region.clone()));
        }
        *summary.rows_by_type.entry(record.asset_type).or_insert(0) += record.rows;
        summary.page_count += record.pages;
    }

    // 4. 地域齐全
    let planned_regions = manifest.planned_regions();
    let configured: Vec<String> = {
        let mut v = manifest.regions.clone();
        v.sort();
        v.dedup();
        v
    };
    if !configured.is_empty() && planned_regions != configured {
        return Err(HarvestError::integrity(format!(
            "地域不齐: 计划 {:?}，实际 {:?}",
            configured, planned_regions
        )));
    }

    // 5. 空 ≠ 失败
    if !allow_empty {
        for (asset_type, rows) in &summary.rows_by_type {
            if *rows == 0 {
                return Err(HarvestError::integrity(format!(
                    "类型 {asset_type} 采到 0 行；确认云端确实为空后需显式开启 service.allow_empty"
                )));
            }
        }
    }

    Ok(summary)
}
