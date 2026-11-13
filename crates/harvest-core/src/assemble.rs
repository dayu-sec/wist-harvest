//! 从 spool 组装一帧完整快照。
//!
//! 两件容易做错的事：
//!
//! - **EIP 双路合并**：同一地域两路分页（`wanip` 带过滤、`all` 不带）会重叠，
//!   必须按 `AddressId` 去重，且不能用两路 `TotalCount` 相加当最终数量。
//! - **输出稳定**：数组按 `source_asset_id` 排序，保证同一 `snap_id` 重放时
//!   帧字节一致（幂等可对比）。

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;

use crate::error::HarvestError;
use crate::model::{
    AssetType, Assets, CloudHost, Coverage, Eip, NativeSnapshot, Stats, Subnet, TypeStat, Vpc,
};
use crate::scope::Scope;
use crate::spool::Spool;

#[derive(Debug, Clone)]
pub struct AssembleOutcome {
    pub snapshot: NativeSnapshot,
    /// 双路合并时被丢掉的重复 EIP 条数（记账用，不是错误）。
    pub duplicate_eip_rows: u64,
}

pub fn build_snapshot(
    spool: &Spool,
    scope: &Scope,
    coverage: Coverage,
) -> Result<AssembleOutcome, HarvestError> {
    let manifest = spool.manifest();

    let hosts = load_sorted::<CloudHost>(spool, AssetType::CloudHost)?;
    check_unique(
        AssetType::CloudHost,
        hosts.iter().map(|h| h.source_asset_id.as_str()),
    )?;

    let eips = load_eips(spool)?;
    let (eips, duplicate_eip_rows) = dedupe_eips(eips);

    let vpcs = load_sorted::<Vpc>(spool, AssetType::Vpc)?;
    check_unique(
        AssetType::Vpc,
        vpcs.iter().map(|v| v.source_asset_id.as_str()),
    )?;

    let subnets = load_sorted::<Subnet>(spool, AssetType::SubNet)?;
    check_unique(
        AssetType::SubNet,
        subnets.iter().map(|s| s.source_asset_id.as_str()),
    )?;

    let mut host_regions: BTreeMap<String, u64> = BTreeMap::new();
    for host in &hosts {
        *host_regions.entry(host.region.clone()).or_insert(0) += 1;
    }

    let stats = Stats {
        cloud_host: TypeStat {
            total: hosts.len() as u64,
            regions: Some(host_regions),
        },
        eip: TypeStat {
            total: eips.len() as u64,
            regions: None,
        },
        vpc: TypeStat {
            total: vpcs.len() as u64,
            regions: None,
        },
        sub_net: TypeStat {
            total: subnets.len() as u64,
            regions: None,
        },
    };

    let collected_at = if manifest.collected_at.is_empty() {
        manifest.created_at.clone()
    } else {
        manifest.collected_at.clone()
    };

    let snapshot = NativeSnapshot {
        schema_native: crate::SCHEMA_NATIVE.to_string(),
        tenant_id: manifest.tenant_id.clone(),
        source_code: manifest.source_code.clone(),
        env_ref: manifest.env_ref.clone(),
        snap_id: manifest.snap_id.clone(),
        vendor: manifest.vendor,
        collected_at,
        regions: scope.regions.clone(),
        assets: Assets {
            cloud_host: hosts,
            eip: eips,
            vpc: vpcs,
            sub_net: subnets,
        },
        stats,
        coverage,
    };

    Ok(AssembleOutcome {
        snapshot,
        duplicate_eip_rows,
    })
}

fn load_sorted<T: DeserializeOwned + HasId>(
    spool: &Spool,
    asset_type: AssetType,
) -> Result<Vec<T>, HarvestError> {
    let mut rows: Vec<T> = Vec::new();
    for record in spool.manifest().items_of(asset_type) {
        rows.extend(read_checked::<T>(spool, record)?);
    }
    rows.sort_by(|a, b| a.source_asset_id().cmp(b.source_asset_id()));
    Ok(rows)
}

/// 读一个工作项的行。读取失败/行数不符一律是**完整性问题**（→ `Aborted`，零帧），
/// 不是普通 IO 错误 —— 它直接决定这一帧能不能发。
fn read_checked<T: DeserializeOwned>(
    spool: &Spool,
    record: &crate::spool::ItemRecord,
) -> Result<Vec<T>, HarvestError> {
    spool
        .read_rows::<T>(record)
        .map_err(|err| HarvestError::integrity(err.to_string()))
}

/// EIP 双路合并：按 `AddressId` 去重，保留首次出现的（lane 顺序来自 plan，稳定）。
fn load_eips(spool: &Spool) -> Result<Vec<Eip>, HarvestError> {
    let mut rows: Vec<Eip> = Vec::new();
    for record in spool.manifest().items_of(AssetType::Eip) {
        rows.extend(read_checked::<Eip>(spool, record)?);
    }
    Ok(rows)
}

fn dedupe_eips(rows: Vec<Eip>) -> (Vec<Eip>, u64) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut dropped = 0u64;
    for row in rows {
        if seen.insert(row.source_asset_id.clone()) {
            out.push(row);
        } else {
            dropped += 1;
        }
    }
    out.sort_by(|a, b| a.source_asset_id.cmp(&b.source_asset_id));
    (out, dropped)
}

fn check_unique<'a>(
    asset_type: AssetType,
    ids: impl Iterator<Item = &'a str>,
) -> Result<(), HarvestError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for id in ids {
        if id.is_empty() {
            return Err(HarvestError::integrity(format!(
                "{asset_type} 存在空 source_asset_id"
            )));
        }
        if !seen.insert(id) {
            return Err(HarvestError::integrity(format!(
                "{asset_type} 存在重复 source_asset_id: {id}"
            )));
        }
    }
    Ok(())
}

/// 让组装逻辑能对任意资产类型统一排序/查重。
pub trait HasId {
    fn source_asset_id(&self) -> &str;
}

impl HasId for CloudHost {
    fn source_asset_id(&self) -> &str {
        &self.source_asset_id
    }
}

impl HasId for Eip {
    fn source_asset_id(&self) -> &str {
        &self.source_asset_id
    }
}

impl HasId for Vpc {
    fn source_asset_id(&self) -> &str {
        &self.source_asset_id
    }
}

impl HasId for Subnet {
    fn source_asset_id(&self) -> &str {
        &self.source_asset_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eip(id: &str) -> Eip {
        Eip {
            source_asset_id: id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn eip_dedupe_keeps_first_and_sorts() {
        let rows = vec![eip("eip-b"), eip("eip-a"), eip("eip-b"), eip("eip-c")];
        let (out, dropped) = dedupe_eips(rows);
        assert_eq!(dropped, 1);
        let ids: Vec<&str> = out.iter().map(|e| e.source_asset_id.as_str()).collect();
        assert_eq!(ids, vec!["eip-a", "eip-b", "eip-c"]);
    }

    #[test]
    fn duplicate_id_on_non_eip_is_an_error() {
        let err = check_unique(AssetType::Vpc, ["vpc-1", "vpc-1"].into_iter()).unwrap_err();
        assert_eq!(err.class, crate::ErrorClass::Integrity);
    }

    #[test]
    fn empty_id_is_an_error() {
        assert!(check_unique(AssetType::CloudHost, [""].into_iter()).is_err());
    }
}
