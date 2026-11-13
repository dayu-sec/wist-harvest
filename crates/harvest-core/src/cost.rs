//! 成本链路：分片 + manifest（契约 `harvest-cost-v1`，权威定义在 `wist-cost` 仓）。
//!
//! 成本**不进数据平面**（设计说明 §7.1）：账单是事实表，O(资源×天×18 个月)，
//! 需要按范围替换与月聚合，跟拓扑的对象/边是两套保留策略。所以这里只做两件事：
//!
//! 1. 把账单一行的标准化形状（[`CostDetail`]）写成 NDJSON 分片；
//! 2. 写一份 manifest，声明每片的 `sha256` / 行数 / 字节数。
//!
//! **分片不可变**：补采 = 新 `batch_id` + 新分片。已发布分片的字节不允许变，
//! `sha256` 就是它的身份。
//!
//! 三条不可协商的规则（契约 §2）在这里落地：
//! - 金额是十进制字符串，`null`（未知）与 `"0"`（确认为零）不可互换，退款为负；
//! - manifest 是唯一发布凭据，消费方只认 `sha256` 与行数；
//! - `CONSUMPTION`（按日）与 `CHARGE`（按月）分范围，**不相加**。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::HarvestError;
use crate::model::Vendor;

/// 明细行的 schema（契约常量，改了就是改契约）。
pub const COST_DETAIL_SCHEMA: &str = "wist.harvest.cost.detail.v1";
/// manifest 的 schema。
pub const COST_MANIFEST_SCHEMA: &str = "wist.harvest.cost.manifest.v1";
/// 单分片默认行数。跟 `wist-cost` 的 `MAX_RANGE_ROWS`（30 万）同一个量级思路：
/// 分片够大（少对象、少请求），又不至于大到一次读不进内存。
pub const DEFAULT_ROWS_PER_SHARD: u64 = 20_000;
/// 一个范围最多多少行（`wist-cost` `MAX_RANGE_ROWS` 的同一上限）。
pub const MAX_RANGE_ROWS: u64 = 300_000;

/// 计费口径。两个口径**分范围、分批次**，永不相加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostBasis {
    /// 按日消耗（`period_start == period_end`）。
    #[serde(rename = "CONSUMPTION")]
    Consumption,
    /// 整月账单（`period_start` 为该月首日，`period_end` 为该月末日）。
    #[serde(rename = "CHARGE")]
    Charge,
}

impl CostBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consumption => "CONSUMPTION",
            Self::Charge => "CHARGE",
        }
    }
}

/// `period_source`：重采（`cloud_api`）与迁移（`legacy_import`）的口径差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeriodSource {
    #[serde(rename = "cloud_api")]
    CloudApi,
    #[serde(rename = "legacy_import")]
    LegacyImport,
}

impl PeriodSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CloudApi => "cloud_api",
            Self::LegacyImport => "legacy_import",
        }
    }
}

/// 范围身份：七元组唯一确定一个范围（契约 §3）。
///
/// 同一七元组重复发布 = **替换**，不是累加 —— 这是下游「金额不可能翻倍」的依据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostRange {
    pub tenant: String,
    pub source_code: String,
    pub vendor: Vendor,
    pub billing_account_uin: String,
    pub cost_basis: CostBasis,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
}

impl CostRange {
    /// 自洽校验：两种口径对 `period_start` / `period_end` 的关系不同。
    pub fn validate(&self) -> Result<(), HarvestError> {
        if self.tenant.trim().is_empty() {
            return Err(self.invalid("tenant 不能为空"));
        }
        if self.source_code.trim().is_empty() {
            return Err(self.invalid("source_code 不能为空"));
        }
        if self.billing_account_uin.trim().is_empty() {
            return Err(self.invalid("billing_account_uin 不能为空"));
        }
        if self.period_end < self.period_start {
            return Err(self.invalid("period_end 早于 period_start"));
        }
        match self.cost_basis {
            CostBasis::Consumption => {
                if self.period_start != self.period_end {
                    return Err(self.invalid("CONSUMPTION 的 period_start 必须等于 period_end"));
                }
            }
            CostBasis::Charge => {
                let first = self.period_start.day();
                let last = self.period_end.day();
                let same_month = self.period_start.year() == self.period_end.year()
                    && self.period_start.month() == self.period_end.month();
                if first != 1 || !same_month {
                    return Err(self.invalid("CHARGE 的 period_start 必须是该月首日"));
                }
                let expect_end = self.period_start.succ_opt();
                let _ = expect_end;
                if last != days_in_month(self.period_start) {
                    return Err(self.invalid("CHARGE 的 period_end 必须是该月末日"));
                }
            }
        }
        Ok(())
    }

    /// 账期月份（`2026-10`），落对象键与 `bill_month` 都用它。
    pub fn month_label(&self) -> String {
        self.period_start.format("%Y-%m").to_string()
    }

    /// 对象键前缀（与 `wist-cost` 的 `ShardKeyLayout::DESIGN_TEMPLATE` 逐字一致）。
    ///
    /// `cost/{tenant}/{vendor}/{cost_basis}/{year}/{month}/{shard_id}.ndjson`
    pub fn object_prefix(&self) -> String {
        format!(
            "cost/{}/{}/{}/{}/{}",
            self.tenant,
            self.vendor.as_str(),
            self.cost_basis.as_str(),
            self.period_start.format("%Y"),
            self.period_start.format("%m"),
        )
    }

    fn invalid(&self, detail: impl Into<String>) -> HarvestError {
        HarvestError::new(crate::error::ErrorClass::Other, detail.into())
    }
}

fn days_in_month(date: NaiveDate) -> u32 {
    let (year, month) = (date.year(), date.month());
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("下个月一定存在");
    next.pred_opt().expect("每个月第一天都有前一天").day()
}

use chrono::Datelike;

/// 账单一行的**标准化**形状（契约 `detail.schema.json`）。
///
/// 只留标准化字段 + `components` 原文 + `raw` 原文：
/// 标准化字段给下游做聚合与匹配，`raw` 保证「以后想加字段不用回去重采」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostDetail {
    pub schema: String,
    /// `CONSUMPTION` 取查询日；`CHARGE` 取北京时间 `PayTime` 的日期。
    pub cost_date: String,
    pub bill_month: String,
    pub business_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub business_code_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_code_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// `business_code + ':' + resource_id`；`resource_id` 缺失时为 `...:UNALLOCATED`。
    pub resource_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer_uin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_uin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay_mode_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_begin_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_end_time: Option<String>,
    /// 云端明细主键（`Id`）：**同一条扣费事件的重采去重键**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_detail_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bill_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    pub currency: String,
    pub real_cost: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cash_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voucher_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incentive_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_amount: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<serde_json::Value>,
    pub raw: serde_json::Value,
}

/// `resource_key` 的统一拼法：没有 `resource_id` 时也必须有一个稳定键，
/// 否则「未归集的费用」在下游无处可挂 —— 而它恰好是成本治理里最有价值的那部分。
pub fn resource_key(business_code: &str, resource_id: Option<&str>) -> String {
    match resource_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => format!("{business_code}:{id}"),
        None => format!("{business_code}:UNALLOCATED"),
    }
}

/// 定标十进制累加器：金额一律按字符串进出，**全程不碰浮点**。
///
/// 云侧给 8 位小数，契约允许 18 位；这里统一按 18 位定标累加（`i128` 容得下
/// 10 亿元 × 1e18 这种量级）。`None` 与 `"0"` 严格区分：全是未知才输出 `null`，
/// 只要有一个分量是 0 就要输出 `"0"`。
#[derive(Debug, Clone, Copy, Default)]
pub struct DecimalSum {
    scaled: i128,
    /// 是否出现过任何非 `null` 的分量。
    known: bool,
}

const SCALE: i128 = 1_000_000_000_000_000_000; // 1e18
const SCALE_DIGITS: usize = 18;

impl DecimalSum {
    /// 累加一个金额分量。
    ///
    /// **解析失败必须报错，不能归零**：`"0"` 在契约里表示「确认为零」，`null` 才是未知。
    /// 把一只读不出来的金额当 0，会让账单少钱且无从发现 —— 这是钱链路上最坏的一种错。
    pub fn add(&mut self, value: Option<&str>) -> Result<(), HarvestError> {
        let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(());
        };
        let scaled = parse_scaled(raw)?;
        self.scaled = self
            .scaled
            .checked_add(scaled)
            .ok_or_else(|| HarvestError::data_format(format!("金额累加溢出 18 位定标：{raw}")))?;
        self.known = true;
        Ok(())
    }

    /// 输出契约要求的十进制字符串：`18` 位定标 → 去尾零；未知则 `None`。
    pub fn finish(self) -> Option<String> {
        if !self.known {
            return None;
        }
        Some(format_scaled(self.scaled))
    }
}

fn parse_scaled(raw: &str) -> Result<i128, HarvestError> {
    let (negative, body) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((int_part, frac)) => (int_part, frac),
        None => (body, ""),
    };
    // 只接受纯十进制：出现 `+`、科学计数法、千分位、全角字符等一律报错，
    // 而不是悄悄当成 0（宁可这轮不出，也不能出错的数）。
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(HarvestError::data_format(format!(
            "金额不是十进制字符串: {raw}"
        )));
    }
    // 超过 18 位小数就截断（云侧目前 8 位；截断比 panic 更安全，且不会放大金额）。
    let frac: String = frac_part.chars().take(SCALE_DIGITS).collect();
    let mut digits = String::with_capacity(int_part.len() + SCALE_DIGITS);
    digits.push_str(if int_part.is_empty() { "0" } else { int_part });
    digits.push_str(&frac);
    digits.extend(std::iter::repeat_n('0', SCALE_DIGITS - frac.len()));
    let value: i128 = digits
        .parse()
        .map_err(|_| HarvestError::data_format(format!("金额超出 18 位定标可表示的范围: {raw}")))?;
    Ok(if negative { -value } else { value })
}

fn format_scaled(scaled: i128) -> String {
    let negative = scaled < 0;
    let abs = scaled.unsigned_abs();
    let divisor = SCALE as u128;
    let int_part = abs / divisor;
    let frac_part = abs % divisor;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&int_part.to_string());
    if frac_part != 0 {
        let frac = format!("{frac_part:018}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out
}

/// manifest 里的一条分片记录（契约 `shard`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostShard {
    /// 四位序号（`0001` 起），拼进对象键。
    pub id: String,
    pub rows: u64,
    /// 分片文件字节的 sha256，小写十六进制。
    pub sha256: String,
    pub bytes: u64,
}

/// 一个范围的清单（契约 `manifest.schema.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostManifest {
    pub schema: String,
    /// 形如 `{source_code}:cost-{UTC yyyyMMddTHHmmssZ}`。
    pub batch_id: String,
    pub range: CostRange,
    pub currency: String,
    pub period_source: PeriodSource,
    pub fetched_at: String,
    /// 云 API `RequestId`，排障用（契约 §1 可选字段）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_ids: Vec<String>,
    pub shards: Vec<CostShard>,
    pub total_rows: u64,
}

/// 分片写入器：边抓边落盘，内存里只留一个分片的缓冲。
///
/// 为什么传 callback 而不是返回 `Vec`：一个月的账单是 4 万行起，18 个月翻十倍；
/// 全收进内存再写会把「采得下」变成「记得下」的问题。
pub struct CostShardWriter {
    root: PathBuf,
    range: CostRange,
    currency: String,
    period_source: PeriodSource,
    fetched_at: String,
    batch_id: String,
    request_ids: Vec<String>,
    rows_per_shard: u64,
    buffer: Vec<CostDetail>,
    shards: Vec<CostShard>,
    total_rows: u64,
}

impl CostShardWriter {
    pub fn new(
        root: impl Into<PathBuf>,
        range: CostRange,
        currency: impl Into<String>,
        period_source: PeriodSource,
        fetched_at: impl Into<String>,
        batch_id: impl Into<String>,
    ) -> Self {
        Self {
            root: root.into(),
            range,
            currency: currency.into(),
            period_source,
            fetched_at: fetched_at.into(),
            batch_id: batch_id.into(),
            request_ids: Vec::new(),
            rows_per_shard: DEFAULT_ROWS_PER_SHARD,
            buffer: Vec::new(),
            shards: Vec::new(),
            total_rows: 0,
        }
    }

    /// 云 API 的 `RequestId`，写进 manifest 供排障（不含凭据）。
    pub fn set_request_ids(&mut self, request_ids: Vec<String>) {
        self.request_ids = request_ids;
    }

    /// 本轮写入的**暂存目录**：分片先落这里，成功后才搬进最终对象键。
    ///
    /// 为什么必须暂存：最终键不含 `batch_id`（契约 §1 的布局），重采同一范围就是
    /// **覆写已发布的字节**。如果在抓取中途失败，旧 manifest 会指着一批新字节，
    /// 消费方 sha256 校验直接整批拒收。暂存把「失败」和「已发布」彻底隔开。
    ///
    /// 目录名带 batch_id，避免两次并发采集互相踩。
    fn staging_dir(&self) -> PathBuf {
        self.root.join(self.range.object_prefix()).join(format!(
            ".staging-{}",
            crate::scope::sanitize(&self.batch_id)
        ))
    }

    // 没有正常收尾就退出（抓取失败/超限/进程内放弃）时，暂存目录里是**从未发布**的字节，
    // 留着只会占盘并让下次排障多一份干扰。`finish` 成功时目录已经空了，这里是空操作。
    // 注意：暂存目录就在范围目录下（同一文件系统），`rename` 不会跨设备。

    pub fn with_rows_per_shard(mut self, rows: u64) -> Self {
        self.rows_per_shard = rows.max(1);
        self
    }

    /// 收一行。缓冲满了就落一片。
    pub fn push(&mut self, detail: CostDetail) -> Result<(), HarvestError> {
        self.total_rows += 1;
        if self.total_rows > MAX_RANGE_ROWS {
            return Err(HarvestError::new(
                crate::error::ErrorClass::Other,
                format!(
                    "范围 {} 超过 {MAX_RANGE_ROWS} 行上限（契约 §5.5：超限应转对象存储导出，不截断）",
                    self.range.month_label()
                ),
            ));
        }
        self.buffer.push(detail);
        if self.buffer.len() as u64 >= self.rows_per_shard {
            self.flush_shard()?;
        }
        Ok(())
    }

    fn flush_shard(&mut self) -> Result<(), HarvestError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let id = format!("{:04}", self.shards.len() + 1);
        let mut bytes = Vec::new();
        for row in &self.buffer {
            serde_json::to_writer(&mut bytes, row)
                .map_err(|err| HarvestError::data_format(format!("分片行序列化失败: {err}")))?;
            bytes.push(b'\n');
        }
        let dir = self.staging_dir();
        std::fs::create_dir_all(&dir).map_err(|err| {
            HarvestError::new(
                crate::error::ErrorClass::Other,
                format!("建分片暂存目录 {}: {err}", dir.display()),
            )
        })?;
        let path = dir.join(format!("{id}.ndjson"));
        std::fs::write(&path, &bytes).map_err(|err| {
            HarvestError::new(
                crate::error::ErrorClass::Other,
                format!("写分片 {}: {err}", path.display()),
            )
        })?;
        self.shards.push(CostShard {
            id,
            rows: self.buffer.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
        });
        self.buffer.clear();
        Ok(())
    }

    /// 收尾：把暂存分片搬进最终对象键 + 原子替换 manifest，返回 manifest 路径。
    ///
    /// 发布顺序（不可换）：
    /// 1. 先写 manifest 到同目录的临时文件并 `fsync`；
    /// 2. 再把分片从暂存目录 `rename` 到最终键；
    /// 3. 最后 `rename` manifest——它是唯一发布凭据，最后一步生效。
    ///
    /// 这样**抓取失败永远不会影响已发布的字节**（分片还在暂存区）。
    /// 同一范围重复发布就是替换：新 manifest 一生效，旧批次就不再是发布凭据。
    pub fn finish(mut self) -> Result<(PathBuf, CostManifest), HarvestError> {
        self.flush_shard()?;
        let manifest = CostManifest {
            schema: COST_MANIFEST_SCHEMA.to_string(),
            batch_id: self.batch_id.clone(),
            range: self.range.clone(),
            currency: self.currency.clone(),
            period_source: self.period_source,
            fetched_at: self.fetched_at.clone(),
            request_ids: self.request_ids.clone(),
            shards: self.shards.clone(),
            total_rows: self.total_rows,
        };

        let object_dir = self.root.join(self.range.object_prefix());
        std::fs::create_dir_all(&object_dir).map_err(|err| {
            HarvestError::new(
                crate::error::ErrorClass::Other,
                format!("建 manifest 目录 {}: {err}", object_dir.display()),
            )
        })?;
        let manifest_path = object_dir.join("manifest.json");
        let tmp = object_dir.join(format!(
            ".manifest-{}.tmp",
            crate::scope::sanitize(&self.batch_id)
        ));
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|err| HarvestError::data_format(format!("manifest 序列化失败: {err}")))?;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp).map_err(|err| {
                HarvestError::new(
                    crate::error::ErrorClass::Other,
                    format!("写 manifest 临时文件 {}: {err}", tmp.display()),
                )
            })?;
            file.write_all(&bytes).map_err(|err| {
                HarvestError::new(
                    crate::error::ErrorClass::Other,
                    format!("写 manifest 临时文件 {}: {err}", tmp.display()),
                )
            })?;
            file.sync_all().map_err(|err| {
                HarvestError::new(
                    crate::error::ErrorClass::Other,
                    format!("同步 manifest 临时文件 {}: {err}", tmp.display()),
                )
            })?;
        }

        // 入位：只有到这里才碰已发布的键。
        for shard in &manifest.shards {
            let from = self.staging_dir().join(format!("{}.ndjson", shard.id));
            let to = object_dir.join(format!("{}.ndjson", shard.id));
            std::fs::rename(&from, &to).map_err(|err| {
                HarvestError::new(
                    crate::error::ErrorClass::Other,
                    format!(
                        "发布分片 {} → {}: {err}（旧 manifest 未动）",
                        from.display(),
                        to.display()
                    ),
                )
            })?;
        }
        // manifest 最后生效：在此之前，消费方看到的仍是上一批。
        std::fs::rename(&tmp, &manifest_path).map_err(|err| {
            HarvestError::new(
                crate::error::ErrorClass::Other,
                format!("更换 manifest {}: {err}", manifest_path.display()),
            )
        })?;

        // 本批分片数变少时，旧的高序号分片会变成孤儿：manifest 是权威，删掉它们。
        prune_stale_shards(&object_dir, &manifest.shards);
        // 暂存目录里的分片已全部搬走；残留的空目录顺手清掉（失败不影响结果）。
        let _ = std::fs::remove_dir_all(self.staging_dir());

        Ok((manifest_path, manifest))
    }
}

/// 没有正常收尾就退出时清理暂存目录（成功发布后目录已空，是空操作）。
impl Drop for CostShardWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.staging_dir());
    }
}

/// 删除不再被 manifest 引用的分片文件（只动 `NNNN.ndjson` 形态的名字）。
fn prune_stale_shards(object_dir: &Path, shards: &[CostShard]) {
    let keep: std::collections::BTreeSet<&str> =
        shards.iter().map(|shard| shard.id.as_str()).collect();
    let Ok(entries) = std::fs::read_dir(object_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".ndjson") else {
            continue;
        };
        if id.len() != 4 || !id.chars().all(|c| c.is_ascii_digit()) || keep.contains(id) {
            continue;
        }
        if let Err(err) = std::fs::remove_file(entry.path()) {
            crate::redact::warn(format!(
                "清理孤儿分片 {} 失败: {err}",
                entry.path().display()
            ));
        }
    }
}

/// 分片里出现过的账期分布（给运行报告用）。
pub fn month_histogram(details: &[CostDetail]) -> BTreeMap<String, u64> {
    let mut hist = BTreeMap::new();
    for detail in details {
        *hist.entry(detail.bill_month.clone()).or_insert(0) += 1;
    }
    hist
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(basis: CostBasis, start: (i32, u32, u32), end: (i32, u32, u32)) -> CostRange {
        CostRange {
            tenant: "DEV-X4".into(),
            source_code: "tencent-cloud-prod".into(),
            vendor: Vendor::TencentCloud,
            billing_account_uin: "100041355715".into(),
            cost_basis: basis,
            period_start: NaiveDate::from_ymd_opt(start.0, start.1, start.2).unwrap(),
            period_end: NaiveDate::from_ymd_opt(end.0, end.1, end.2).unwrap(),
        }
    }

    #[test]
    fn consumption_range_is_a_single_day() {
        range(CostBasis::Consumption, (2026, 11, 10), (2026, 11, 10))
            .validate()
            .expect("单日");
        assert!(
            range(CostBasis::Consumption, (2026, 11, 10), (2026, 11, 11))
                .validate()
                .is_err(),
            "CONSUMPTION 跨天必须拒"
        );
    }

    #[test]
    fn charge_range_must_cover_the_whole_month() {
        range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31))
            .validate()
            .expect("整月");
        range(CostBasis::Charge, (2026, 2, 1), (2026, 2, 28))
            .validate()
            .expect("平年二月");
        assert!(
            range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 30))
                .validate()
                .is_err(),
            "少一天就是错的月"
        );
        assert!(
            range(CostBasis::Charge, (2026, 10, 2), (2026, 10, 31))
                .validate()
                .is_err(),
            "不是从首日开始"
        );
    }

    #[test]
    fn object_prefix_matches_the_cost_layout() {
        let range = range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31));
        assert_eq!(
            range.object_prefix(),
            "cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10"
        );
    }

    #[test]
    fn decimal_sum_keeps_null_zero_and_negative_apart() {
        // 全是未知 → null（不是 0）。
        assert_eq!(DecimalSum::default().finish(), None);

        // 一个已知的 0 → "0"。
        let mut zero = DecimalSum::default();
        zero.add(Some("0")).unwrap();
        assert_eq!(zero.finish().as_deref(), Some("0"));

        // 退款为负。
        let mut refund = DecimalSum::default();
        refund.add(Some("10.5")).unwrap();
        refund.add(Some("-3.25")).unwrap();
        assert_eq!(refund.finish().as_deref(), Some("7.25"));

        // 8 位小数的组件金额不能丢精度。
        let mut cents = DecimalSum::default();
        cents.add(Some("0.97000000")).unwrap();
        cents.add(Some("0.03000000")).unwrap();
        assert_eq!(cents.finish().as_deref(), Some("1"));
    }

    #[test]
    fn unparseable_amounts_are_errors_not_zero() {
        // 金额读不出来就是错误：静默当成 0 等于把「确认为零」写进了账单。
        for bad in ["+1.5", "1e3", "1,234.00", "１.５", "1.2.3"] {
            let mut sum = DecimalSum::default();
            assert!(sum.add(Some(bad)).is_err(), "{bad} 必须报错而不是归零");
            assert_eq!(sum.finish(), None, "报错后不能留下一个虚假的 0");
        }
        // 空串/空白仍然是「未知」，不报错也不记 known。
        let mut blank = DecimalSum::default();
        blank.add(Some("  ")).unwrap();
        assert_eq!(blank.finish(), None);
    }

    #[test]
    fn decimal_sum_handles_many_components_without_drift() {
        // 1000 个 0.00000001 → 0.00001；浮点累加这里会给出 1.0000002e-5 之类的脏值。
        let mut sum = DecimalSum::default();
        for _ in 0..1000 {
            sum.add(Some("0.00000001")).unwrap();
        }
        assert_eq!(sum.finish().as_deref(), Some("0.00001"));
    }

    #[test]
    fn resource_key_falls_back_to_unallocated() {
        assert_eq!(resource_key("p_cvm", Some("ins-1")), "p_cvm:ins-1");
        assert_eq!(resource_key("p_cvm", Some("  ")), "p_cvm:UNALLOCATED");
        assert_eq!(resource_key("p_cvm", None), "p_cvm:UNALLOCATED");
    }

    fn detail(cost: &str) -> CostDetail {
        CostDetail {
            schema: COST_DETAIL_SCHEMA.to_string(),
            cost_date: "2026-10-01".into(),
            bill_month: "2026-10".into(),
            business_code: "p_cvm".into(),
            business_code_name: None,
            product_code: None,
            product_code_name: None,
            resource_id: Some("ins-1".into()),
            resource_key: resource_key("p_cvm", Some("ins-1")),
            resource_name: None,
            payer_uin: None,
            owner_uin: None,
            pay_mode_name: None,
            action_type_name: None,
            pay_time: None,
            fee_begin_time: None,
            fee_end_time: None,
            provider_detail_id: None,
            bill_id: None,
            order_id: None,
            currency: "CNY".into(),
            real_cost: Some(cost.into()),
            cash_amount: None,
            voucher_amount: None,
            incentive_amount: None,
            transfer_amount: None,
            components: Vec::new(),
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn writer_shards_by_row_count_and_hashes_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let range = range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31));
        let mut writer = CostShardWriter::new(
            dir.path(),
            range.clone(),
            "CNY",
            PeriodSource::CloudApi,
            "2026-11-12T00:00:00+08:00",
            "tencent-cloud-prod:cost-20261112T000000Z",
        )
        .with_rows_per_shard(2);
        for _ in 0..5 {
            writer.push(detail("1.5")).unwrap();
        }
        let (manifest_path, manifest) = writer.finish().unwrap();

        assert_eq!(manifest.total_rows, 5);
        assert_eq!(
            manifest
                .shards
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
            vec!["0001", "0002", "0003"],
            "5 行 / 每片 2 行 = 3 片，最后一片是余量"
        );
        assert_eq!(manifest.shards[2].rows, 1);
        assert_eq!(manifest.schema, COST_MANIFEST_SCHEMA);

        // 分片落的路径必须与 wist-cost 的对象键布局逐字一致，否则消费方找不到。
        let shard_path = dir
            .path()
            .join("cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10/0001.ndjson");
        assert!(shard_path.exists(), "分片路径 {}", shard_path.display());
        let bytes = std::fs::read(&shard_path).unwrap();
        assert_eq!(
            manifest.shards[0].sha256,
            hex::encode(Sha256::digest(&bytes)),
            "manifest 里的 sha256 必须是实际字节的摘要"
        );
        assert_eq!(manifest.shards[0].bytes, bytes.len() as u64);
        assert!(bytes.ends_with(b"\n"), "NDJSON 每行以换行结尾");

        // manifest 也能读回来，且形状与写出的一致。
        let raw = std::fs::read(&manifest_path).unwrap();
        let back: CostManifest = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back, manifest);

        // 发布完成后暂存目录必须清干净（不留下一份重复的字节）。
        let staging: Vec<_> =
            std::fs::read_dir(dir.path().join("cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10"))
                .unwrap()
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(".staging-"))
                })
                .collect();
        assert!(staging.is_empty(), "暂存目录应在发布后清掉");
    }

    #[test]
    fn failed_batch_never_touches_the_published_shards() {
        let dir = tempfile::tempdir().unwrap();
        let range = range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31));
        let shard_path = dir
            .path()
            .join("cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10/0001.ndjson");
        let manifest_path = dir
            .path()
            .join("cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10/manifest.json");

        // 第一批：2 行，正常发布。
        let mut first = CostShardWriter::new(
            dir.path(),
            range.clone(),
            "CNY",
            PeriodSource::CloudApi,
            "t1",
            "batch-1",
        )
        .with_rows_per_shard(2);
        first.push(detail("1")).unwrap();
        first.push(detail("1")).unwrap();
        let (_, first_manifest) = first.finish().unwrap();
        let published = std::fs::read(&shard_path).unwrap();
        let published_manifest = std::fs::read(&manifest_path).unwrap();
        assert_eq!(first_manifest.shards[0].sha256, sha(&published));

        // 第二批：抓到一半失败（只 push、不 finish），已发布的那份必须一字未变。
        // 注意 rows_per_shard=1，push 就会立刻落暂存片，所以这不是「在内存里」的情况。
        let mut second = CostShardWriter::new(
            dir.path(),
            range,
            "CNY",
            PeriodSource::CloudApi,
            "t2",
            "batch-2",
        )
        .with_rows_per_shard(1);
        second.push(detail("999")).unwrap();
        drop(second);

        assert_eq!(
            std::fs::read(&shard_path).unwrap(),
            published,
            "抓取失败不能改写已发布分片的字节"
        );
        assert_eq!(
            std::fs::read(&manifest_path).unwrap(),
            published_manifest,
            "抓取失败不能改写已发布的 manifest"
        );
        let staging_left = std::fs::read_dir(shard_path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".staging-"))
            })
            .count();
        assert_eq!(staging_left, 0, "失败批次的暂存分片应当被清掉");
    }

    #[test]
    fn republishing_prunes_shards_the_new_manifest_does_not_list() {
        let dir = tempfile::tempdir().unwrap();
        let range = range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31));
        let month_dir = dir.path().join("cost/DEV-X4/TENCENT_CLOUD/CHARGE/2026/10");
        let writer = |batch: &str, rows: usize| {
            let mut writer = CostShardWriter::new(
                dir.path(),
                range.clone(),
                "CNY",
                PeriodSource::CloudApi,
                "t",
                batch,
            )
            .with_rows_per_shard(1);
            for _ in 0..rows {
                writer.push(detail("1")).unwrap();
            }
            writer
        };

        writer("batch-1", 3).finish().unwrap();
        assert!(month_dir.join("0003.ndjson").exists());

        let (_, manifest) = writer("batch-2", 1).finish().unwrap();
        assert_eq!(manifest.shards.len(), 1);
        assert!(month_dir.join("0001.ndjson").exists());
        assert!(
            !month_dir.join("0002.ndjson").exists() && !month_dir.join("0003.ndjson").exists(),
            "旧批次多出来的分片不能留在范围目录里"
        );
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn an_empty_range_still_writes_a_manifest() {
        // 空范围是合法事实（这个月确实没花钱），不能被当成失败。
        let dir = tempfile::tempdir().unwrap();
        let writer = CostShardWriter::new(
            dir.path(),
            range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
            PeriodSource::CloudApi,
            "2026-11-12T00:00:00+08:00",
            "b",
        );
        let (_, manifest) = writer.finish().unwrap();
        assert_eq!(manifest.total_rows, 0);
        assert!(manifest.shards.is_empty());
    }

    #[test]
    fn exceeding_the_range_limit_is_an_error_not_a_truncation() {
        // 契约 §5.5：超限必须失败（转对象存储导出），不能悄悄截断。
        let dir = tempfile::tempdir().unwrap();
        let mut writer = CostShardWriter::new(
            dir.path(),
            range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
            PeriodSource::CloudApi,
            "t",
            "b",
        )
        .with_rows_per_shard(MAX_RANGE_ROWS);
        for i in 0..MAX_RANGE_ROWS {
            writer
                .push(detail("0"))
                .unwrap_or_else(|err| panic!("第 {i} 行不该失败: {err}"));
        }
        let err = writer.push(detail("0")).expect_err("超限必须拒");
        assert!(format!("{err}").contains("上限"), "实际：{err}");
    }
}
