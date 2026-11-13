//! 完整性校验：**不完整不发**。
//!
//! 判据（设计说明 §5 `Verified` 行）：
//!
//! 1. 计划内每个工作项都完成；
//! 2. 每项的实收行数与云端声明的 `TotalCount` 一致（分页总数不一致立刻失败）；
//! 3. 页数连续（至少 1 页，且抓取时逐页递增到 `TotalCount`）；
//! 4. 地域齐全（工作项覆盖的地域集合 == 计划地域集合）；
//! 5. 空 ≠ 失败：只有显式允许时才接受某类型 0 行（`allow_empty` 可以按类型给）。
//!
//! ID 去重不在这里做：需要真实行内容，见 [`crate::assemble`]。

use std::collections::BTreeMap;

use crate::config::AllowEmpty;
use crate::error::HarvestError;
use crate::model::AssetType;
use crate::source::Completeness;
use crate::spool::Manifest;

#[derive(Debug, Clone, Default)]
pub struct VerifySummary {
    pub rows_by_type: BTreeMap<AssetType, u64>,
    pub item_count: usize,
    pub page_count: u64,
}

pub fn verify_manifest(
    manifest: &Manifest,
    allow_empty: &AllowEmpty,
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

    // 2 & 3. 完整性证据 + 页数连续
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
        // 先看新证据；旧 manifest（没有 `completeness`）回落到 `expected_rows`。
        let claimed_total = match &record.completeness {
            Some(Completeness::Total { total }) => Some(*total),
            // 游标走到尽头就是完整性证据（无总数的云只能靠这个）。
            Some(Completeness::Exhausted) => None,
            None => record.expected_rows,
        };
        if let Some(expected) = claimed_total
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

    // 5. 空 ≠ 失败（按类型判定，开关也可以按类型给）
    for (asset_type, rows) in &summary.rows_by_type {
        if *rows == 0 && !allow_empty.allows(*asset_type) {
            return Err(HarvestError::integrity(format!(
                "类型 {asset_type} 采到 0 行；确认云端确实为空后需显式开启 service.allow_empty（可只针对该类型）"
            )));
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Vendor;
    use crate::source::{Completeness, LANE_DEFAULT};
    use crate::spool::ItemRecord;
    use crate::state::RunState;

    fn manifest(record: ItemRecord) -> Manifest {
        let regions = vec![record.region.clone()];
        Manifest {
            schema: crate::SCHEMA_SPOOL_MANIFEST.to_string(),
            snap_id: "snap".into(),
            source_code: "s".into(),
            producer: "s:inventory".into(),
            group: "inventory".into(),
            env_ref: "prod".into(),
            vendor: Vendor::TencentCloud,
            scope_hash: "abc123".into(),
            regions,
            state: RunState::Spooled,
            created_at: "2026-11-13T00:00:00.000Z".into(),
            updated_at: "2026-11-13T00:00:00.000Z".into(),
            collected_at: "2026-11-13T08:00:00+08:00".into(),
            items: vec![record],
            error: None,
        }
    }

    fn record(rows: u64, completeness: Option<Completeness>, expected: Option<u64>) -> ItemRecord {
        ItemRecord {
            asset_type: AssetType::CloudHost,
            region: "ap-guangzhou".into(),
            lane: LANE_DEFAULT.into(),
            file: "items/CLOUD_HOST/ap-guangzhou.default.ndjson".into(),
            completed: true,
            rows,
            pages: 1,
            expected_rows: expected,
            completeness,
            retries: 0,
            duration_ms: 1,
            request_ids: vec![],
            fetched_at: None,
            error: None,
        }
    }

    #[test]
    fn total_evidence_must_match_rows() {
        let m = manifest(record(2, Some(Completeness::Total { total: 3 }), Some(3)));
        let err = verify_manifest(&m, &AllowEmpty::All(false)).unwrap_err();
        assert!(err.message.contains("总数不一致"), "{}", err.message);
    }

    #[test]
    fn exhausted_cursor_is_completeness_evidence_without_a_total() {
        // 无总数的云（AWS/Azure）：游标耗尽就能发，不需要编一个 Total。
        let m = manifest(record(2, Some(Completeness::Exhausted), None));
        verify_manifest(&m, &AllowEmpty::All(false)).expect("游标耗尽应算完整");
    }

    #[test]
    fn legacy_manifest_falls_back_to_expected_rows() {
        // 升级前写下的 manifest 没有 `completeness`，不能因此跳过校验。
        let legacy = manifest(record(2, None, Some(3)));
        let err = verify_manifest(&legacy, &AllowEmpty::All(false)).unwrap_err();
        assert!(err.message.contains("总数不一致"), "{}", err.message);

        // 旧 manifest 既无 completeness 也无 expected_rows：保持旧行为（不额外校验）。
        let no_evidence = manifest(record(2, None, None));
        verify_manifest(&no_evidence, &AllowEmpty::All(false)).expect("旧行为保持");
    }
}
