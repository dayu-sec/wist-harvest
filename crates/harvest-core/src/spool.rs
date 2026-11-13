//! spool：采集结果先落本地，**收齐之前不发送**。
//!
//! 目录：`<spool_root>/<tenant>/<producer>/<snap_id>/`
//!
//! ```text
//! manifest.json
//! items/CLOUD_HOST/ap-guangzhou.default.ndjson
//! items/EIP/ap-guangzhou.wanip.ndjson
//! items/EIP/ap-guangzhou.all.ndjson
//! ```
//!
//! 续跑语义：`manifest.json` 记录每个工作项的 `completed`，进程重启后已完成项
//! 不再重抓；未完成项整项重抓（`begin_item` 截断文件，所以不会出现半份数据
//! 与新数据拼接）。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, HarvestError};
use crate::model::{AssetType, Vendor};
use crate::scope::Scope;
use crate::source::{FetchStat, WorkItem};
use crate::state::RunState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub snap_id: String,
    pub tenant_id: String,
    pub source_code: String,
    pub producer: String,
    pub group: String,
    pub env_ref: String,
    pub vendor: Vendor,
    pub scope_hash: String,
    pub regions: Vec<String>,
    pub state: RunState,
    pub created_at: String,
    pub updated_at: String,
    /// 观测时间（+08:00）。建 spool 时固定，续跑/重发沿用，保证同 snap_id 的
    /// 帧体可复现。
    #[serde(default)]
    pub collected_at: String,
    pub items: Vec<ItemRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ManifestError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestError {
    pub class: ErrorClass,
    pub message: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemRecord {
    pub asset_type: AssetType,
    pub region: String,
    pub lane: String,
    /// 相对 spool 目录的路径。
    pub file: String,
    pub completed: bool,
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub pages: u64,
    /// 云端声明的总数，完整性校验用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_rows: Option<u64>,
    #[serde(default)]
    pub retries: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub request_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ItemRecord {
    fn from_item(item: &WorkItem) -> Self {
        Self {
            asset_type: item.asset_type,
            region: item.region.clone(),
            lane: item.lane.clone(),
            file: item_file(item),
            completed: false,
            rows: 0,
            pages: 0,
            expected_rows: None,
            retries: 0,
            duration_ms: 0,
            request_ids: Vec::new(),
            fetched_at: None,
            error: None,
        }
    }

    pub fn item(&self) -> WorkItem {
        WorkItem::new(self.asset_type, self.region.clone(), self.lane.clone())
    }
}

fn item_file(item: &WorkItem) -> String {
    // 地域/分页道来自云响应或配置，拼进路径前统一安全化：
    // 正常地域码（`ap-guangzhou`）不受影响，但不会因为一个奇怪字符就跑出 spool 目录。
    format!(
        "items/{}/{}.{}.ndjson",
        item.asset_type,
        crate::scope::sanitize(&item.region),
        crate::scope::sanitize(&item.lane)
    )
}

impl Manifest {
    pub fn item(&self, item: &WorkItem) -> Option<&ItemRecord> {
        self.items.iter().find(|r| {
            r.asset_type == item.asset_type && r.region == item.region && r.lane == item.lane
        })
    }

    pub fn item_mut(&mut self, item: &WorkItem) -> Option<&mut ItemRecord> {
        self.items.iter_mut().find(|r| {
            r.asset_type == item.asset_type && r.region == item.region && r.lane == item.lane
        })
    }

    pub fn complete_items(&self) -> impl Iterator<Item = &ItemRecord> {
        self.items.iter().filter(|r| r.completed)
    }

    /// 某类型的全部已完成项。
    pub fn items_of(&self, asset_type: AssetType) -> Vec<&ItemRecord> {
        self.complete_items()
            .filter(|r| r.asset_type == asset_type)
            .collect()
    }

    pub fn rows_of(&self, asset_type: AssetType) -> u64 {
        self.items_of(asset_type).iter().map(|r| r.rows).sum()
    }

    /// 参与本轮计划的地域（来自工作项，不是配置，避免"配置写了但一项没采"）。
    pub fn planned_regions(&self) -> Vec<String> {
        let mut regions: Vec<String> = self.items.iter().map(|r| r.region.clone()).collect();
        regions.sort();
        regions.dedup();
        regions
    }

    pub fn planned_types(&self) -> Vec<AssetType> {
        let mut types: Vec<AssetType> = self.items.iter().map(|r| r.asset_type).collect();
        types.sort();
        types.dedup();
        types
    }
}

pub struct Spool {
    dir: PathBuf,
    manifest: Manifest,
}

impl Spool {
    /// 建一份新 spool。目录里已有 `manifest.json` 时直接沿用它（同 `snap_id` 重放），
    /// 而不是重建一份——重建会丢掉已完成的记账，让重放变回全量重抓。
    pub fn create(
        spool_root: &Path,
        scope: &Scope,
        snap_id: &str,
        plan: &[WorkItem],
    ) -> Result<Self, SpoolError> {
        let dir = spool_root.join(scope.spool_prefix()).join(snap_id);
        if dir.join("manifest.json").exists() {
            let existing = Self::open_existing(&dir)?;
            // 重放的前提是「同一份计划」：否则旧 manifest 里没有新工作项的记账，
            // 抓到的行会静默丢掉，最后发出去的还是旧数据。宁可报错让人换 snap_id。
            if let Some((expected, actual)) = existing.plan_mismatch(plan) {
                return Err(SpoolError::new(format!(
                    "snap_id {snap_id} 已有 spool，但采集计划不一致（计划 {:?} vs spool {:?}）；换一个 snap_id，或先移走 {}",
                    expected,
                    actual,
                    dir.display()
                )));
            }
            crate::redact::warn(format!(
                "spool 已存在，沿用既有 manifest（不重建）: {}",
                dir.display()
            ));
            return Ok(existing);
        }
        std::fs::create_dir_all(dir.join("items"))?;
        let created_at_utc = crate::time::utc_now();
        let now = crate::time::rfc3339_utc(created_at_utc);
        let manifest = Manifest {
            schema: crate::SCHEMA_SPOOL_MANIFEST.to_string(),
            snap_id: snap_id.to_string(),
            tenant_id: scope.tenant_id.clone(),
            source_code: scope.source_code.clone(),
            producer: scope.producer(),
            group: scope.group.clone(),
            env_ref: scope.env_ref.clone(),
            vendor: scope.vendor,
            scope_hash: scope.scope_hash(),
            regions: scope.regions.clone(),
            state: RunState::Planned,
            created_at: now.clone(),
            updated_at: now,
            collected_at: crate::time::rfc3339_cn(created_at_utc),
            items: plan.iter().map(ItemRecord::from_item).collect(),
            error: None,
        };
        let mut spool = Self { dir, manifest };
        spool.save()?;
        Ok(spool)
    }

    pub fn open_existing(dir: &Path) -> Result<Self, SpoolError> {
        let raw = std::fs::read_to_string(dir.join("manifest.json"))?;
        let manifest: Manifest = serde_json::from_str(&raw)
            .map_err(|err| SpoolError::new(format!("manifest.json 解析失败: {err}")))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    /// 续跑必须保证计划没变：配置改了地域/类型就是另一个范围，不能混进同一帧。
    pub fn plan_mismatch(&self, plan: &[WorkItem]) -> Option<(Vec<String>, Vec<String>)> {
        let manifest = self.manifest();
        let mut expected: Vec<String> = plan.iter().map(WorkItem::key).collect();
        expected.sort();
        let mut actual: Vec<String> = manifest.items.iter().map(|r| r.item().key()).collect();
        actual.sort();
        if expected == actual {
            None
        } else {
            Some((expected, actual))
        }
    }

    /// 找同一范围下"没做完"的 spool（可续跑）。按 `created_at` 取最新一份。
    ///
    /// 跳过两类：`Done`（已完成）与 `Abandoned`（计划变了，永远不可能变合法）。
    /// 「不完整不发」不等于「不完整就一直续跑」—— 后者会让采集器卡死在旧 spool 上。
    pub fn find_resumable(spool_root: &Path, scope: &Scope) -> Result<Option<PathBuf>, SpoolError> {
        let prefix = spool_root.join(scope.spool_prefix());
        let entries = match std::fs::read_dir(&prefix) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let wanted_hash = scope.scope_hash();
        let mut best: Option<(String, PathBuf)> = None;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let manifest_path = dir.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Manifest>(&raw) else {
                continue;
            };
            if manifest.scope_hash != wanted_hash
                || matches!(manifest.state, RunState::Done | RunState::Abandoned)
            {
                continue;
            }
            match &best {
                Some((created_at, _)) if created_at >= &manifest.created_at => {}
                _ => best = Some((manifest.created_at.clone(), dir)),
            }
        }
        Ok(best.map(|(_, dir)| dir))
    }

    /// snap_id 的时间戳只到秒，同一秒内的两轮会遇到同一个目录名。
    /// 这里保证"新一轮一定拿到没人用过的 snap_id"：已存在的目录一律跳过。
    /// 未完成的 spool 由 [`Spool::find_resumable`] 负责续跑，不靠这个函数保命。
    pub fn next_available_snap_id(spool_root: &Path, scope: &Scope, base: &str) -> String {
        let prefix = spool_root.join(scope.spool_prefix());
        for attempt in 0..1000u32 {
            let candidate = if attempt == 0 {
                base.to_string()
            } else {
                format!("{base}-{attempt}")
            };
            if !prefix.join(&candidate).exists() {
                return candidate;
            }
        }
        format!("{base}-{}", crate::time::utc_now().timestamp_millis())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn manifest_mut(&mut self) -> &mut Manifest {
        &mut self.manifest
    }

    pub fn snap_id(&self) -> &str {
        &self.manifest.snap_id
    }

    pub fn set_state(&mut self, state: RunState) {
        self.manifest.state = state;
    }

    /// 放弃这份 spool：保留目录与已完成的数据供诊断，但不再参与续跑。
    pub fn abandon(&mut self, reason: &str) -> Result<(), SpoolError> {
        self.set_error(ErrorClass::Integrity, reason);
        self.set_state(RunState::Abandoned);
        self.save()
    }

    /// 这份 spool 已经放了多久（毫秒）。
    ///
    /// `created_at` 解析失败按「很久以前」处理：宁可不续跑，也不要把一份来路不明的
    /// spool 无限期地延续下去。
    pub fn age_ms(&self) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&self.manifest.created_at)
            .map(|created| {
                (crate::time::utc_now() - created.with_timezone(&chrono::Utc)).num_milliseconds()
            })
            .unwrap_or(i64::MAX)
    }

    /// 是否已经超过可续跑时限（`limit_ms == 0` 表示不限制）。
    pub fn resume_expired(&self, limit_ms: u64) -> bool {
        limit_ms > 0 && self.age_ms() > limit_ms as i64
    }

    pub fn set_error(&mut self, class: ErrorClass, message: &str) {
        self.manifest.error = Some(ManifestError {
            class,
            message: crate::redact::scrub(message),
            at: crate::time::rfc3339_utc(crate::time::utc_now()),
        });
    }

    pub fn is_complete(&self, item: &WorkItem) -> bool {
        self.manifest
            .item(item)
            .map(|r| r.completed)
            .unwrap_or(false)
    }

    /// 清空并准备写一个工作项。截断是续跑正确性的关键。
    pub fn begin_item(&mut self, item: &WorkItem) -> Result<(), SpoolError> {
        let path = self.dir.join(item_file(item));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        File::create(&path)?;
        let record = self.record_mut(item)?;
        record.rows = 0;
        record.pages = 0;
        record.request_ids.clear();
        record.expected_rows = None;
        record.retries = 0;
        record.duration_ms = 0;
        record.completed = false;
        record.error = None;
        record.fetched_at = None;
        self.save()
    }

    /// 取工作项对应的记账行；没有就是「计划与 spool 不一致」，不能静默丢数据。
    fn record_mut(&mut self, item: &WorkItem) -> Result<&mut ItemRecord, SpoolError> {
        let key = item.key();
        self.manifest
            .item_mut(item)
            .ok_or_else(|| SpoolError::new(format!("工作项 {key} 不在 spool 的计划里")))
    }

    /// 追加一页的行到 spool（NDJSON）。
    pub fn append_rows<T: Serialize>(
        &mut self,
        item: &WorkItem,
        rows: &[T],
    ) -> Result<(), SpoolError> {
        if rows.is_empty() {
            return Ok(());
        }
        let path = self.dir.join(item_file(item));
        let file = OpenOptions::new().append(true).create(true).open(&path)?;
        let mut writer = BufWriter::new(file);
        for row in rows {
            serde_json::to_writer(&mut writer, row)
                .map_err(|err| SpoolError::new(format!("spool 序列化失败: {err}")))?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        let written = rows.len() as u64;
        self.record_mut(item)?.rows += written;
        Ok(())
    }

    pub fn finish_item(&mut self, item: &WorkItem, stat: &FetchStat) -> Result<(), SpoolError> {
        let fetched_at = crate::time::rfc3339_utc(crate::time::utc_now());
        let record = self.record_mut(item)?;
        record.rows = stat.rows;
        record.pages = stat.pages;
        record.expected_rows = stat.expected_rows;
        record.retries = stat.retries;
        record.duration_ms = stat.duration_ms;
        record.request_ids = stat.request_ids.clone();
        record.fetched_at = Some(fetched_at);
        record.completed = true;
        record.error = None;
        self.save()
    }

    pub fn fail_item(&mut self, item: &WorkItem, err: &HarvestError) {
        let message = crate::redact::scrub(&err.message);
        if let Some(record) = self.manifest.item_mut(item) {
            record.completed = false;
            record.error = Some(message);
        }
    }

    /// 读一个工作项的全部行，**并核对文件本身**。
    ///
    /// 为什么不能只信 manifest：`record.rows` 是上次运行的记账，文件可能被清理、
    /// 回滚或只写了一半。如果文件丢了就返回空集合，最终会发出一条「这类资产 0 行」
    /// 且 `complete=true` 的快照——恰好是这套设计最想避免的事故。
    /// 所以这里对「文件存在」和「行数」都做硬校验，不匹配一律报错（→ `Aborted`）。
    pub fn read_rows<T: DeserializeOwned>(
        &self,
        record: &ItemRecord,
    ) -> Result<Vec<T>, SpoolError> {
        let path = self.dir.join(&record.file);
        let file = match File::open(&path) {
            Ok(file) => file,
            // 0 行的工作项允许没有文件（`append_rows` 空页不落盘）；
            // 但 manifest 记了行数就必须能读到，否则就是 spool 被清理/回滚了。
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && record.rows == 0 => {
                return Ok(Vec::new());
            }
            Err(err) => {
                return Err(SpoolError::new(format!(
                    "spool 文件 {} 不可读（manifest 记了 {} 行）: {err}",
                    record.file, record.rows
                )));
            }
        };
        let mut rows = Vec::new();
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let row = serde_json::from_str(&line).map_err(|err| {
                SpoolError::new(format!("{} 第 {} 行解析失败: {err}", record.file, idx + 1))
            })?;
            rows.push(row);
        }
        if rows.len() as u64 != record.rows {
            return Err(SpoolError::new(format!(
                "spool 文件 {} 实际 {} 行，manifest 记了 {} 行",
                record.file,
                rows.len(),
                record.rows
            )));
        }
        Ok(rows)
    }

    /// 原子落盘：写临时文件 + rename，避免断电留下半份 manifest。
    pub fn save(&mut self) -> Result<(), SpoolError> {
        self.manifest.updated_at = crate::time::rfc3339_utc(crate::time::utc_now());
        let raw = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|err| SpoolError::new(format!("manifest 序列化失败: {err}")))?;
        let tmp = self.dir.join("manifest.json.tmp");
        let final_path = self.dir.join("manifest.json");
        {
            let mut file = File::create(&tmp)?;
            file.write_all(&raw)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    }

    /// 按类型汇总行数，报告用。
    pub fn rows_by_type(&self) -> BTreeMap<AssetType, u64> {
        let mut out = BTreeMap::new();
        for asset_type in AssetType::ALL {
            out.insert(asset_type, self.manifest.rows_of(asset_type));
        }
        out
    }
}

#[derive(Debug)]
pub struct SpoolError {
    pub message: String,
}

impl SpoolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SpoolError {}

impl From<std::io::Error> for SpoolError {
    fn from(err: std::io::Error) -> Self {
        Self::new(format!("spool IO 错误: {err}"))
    }
}
