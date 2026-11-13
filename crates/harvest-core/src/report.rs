//! 运行报告：每次运行一份，落 `state/runs/<snap_id>.json`。
//!
//! 内容按设计说明 §8：范围身份、终态、分类错误、每类型/每地域统计、
//! 云 API `RequestId`、数据平面 ACK。**不含凭据**（写入前统一过脱敏）。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HarvestError;
use crate::model::Vendor;
use crate::scope::Scope;
use crate::source::PublishAck;
use crate::spool::{Manifest, SpoolError};
use crate::state::RunState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub schema: String,
    pub snap_id: String,
    pub scope: ScopeReport,
    pub state: RunState,
    pub state_trail: Vec<String>,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    /// 本次是否沿用了上一轮未完成的 spool。
    pub resumed: bool,
    pub per_type: BTreeMap<String, TypeReport>,
    pub items: Vec<ItemReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sink: Option<SinkReport>,
    pub errors: Vec<ErrorEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeReport {
    pub source_code: String,
    pub producer: String,
    pub group: String,
    pub env_ref: String,
    pub vendor: Vendor,
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypeReport {
    pub rows: u64,
    pub pages: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemReport {
    pub asset_type: String,
    pub region: String,
    pub lane: String,
    pub completed: bool,
    pub rows: u64,
    pub pages: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rows: Option<u64>,
    /// 完整性证据（`total` / `exhausted`），排障时能看出这轮靠什么判完整性。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completeness: Option<crate::source::Completeness>,
    pub retries: u64,
    pub duration_ms: u64,
    pub request_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkReport {
    pub bytes: u64,
    pub ack: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEntry {
    pub class: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub at: String,
}

impl ErrorEntry {
    pub fn from_error(err: &HarvestError) -> Self {
        Self {
            class: err.class.as_str().to_string(),
            message: crate::redact::scrub(&err.message),
            action: err.action.as_deref().map(str::to_string),
            region: err.region.as_deref().map(str::to_string),
            request_id: err.request_id.as_deref().map(str::to_string),
            code: err.code.as_deref().map(str::to_string),
            at: crate::time::rfc3339_utc(crate::time::utc_now()),
        }
    }
}

impl ScopeReport {
    pub fn from_scope(scope: &Scope) -> Self {
        Self {
            source_code: scope.source_code.clone(),
            producer: scope.producer(),
            group: scope.group.clone(),
            env_ref: scope.env_ref.clone(),
            vendor: scope.vendor,
            regions: scope.regions.clone(),
        }
    }
}

impl RunReport {
    pub fn from_manifest(manifest: &Manifest) -> ScopeReport {
        ScopeReport {
            source_code: manifest.source_code.clone(),
            producer: manifest.producer.clone(),
            group: manifest.group.clone(),
            env_ref: manifest.env_ref.clone(),
            vendor: manifest.vendor,
            regions: manifest.regions.clone(),
        }
    }

    pub fn sink_from_ack(ack: &PublishAck) -> SinkReport {
        SinkReport {
            bytes: ack.bytes,
            ack: ack.ack.clone(),
            duration_ms: ack.duration_ms,
        }
    }

    /// 原子写盘：`<runs_dir>/<snap_id>.json`。
    pub fn write(&self, runs_dir: &Path) -> Result<PathBuf, SpoolError> {
        self.write_as(runs_dir, &self.snap_id)
    }

    /// 同上，但指定文件名。
    ///
    /// 用于「轮次还没开始就失败」的情形：那时没有 `snap_id`，不能所有启动失败
    /// 都覆盖同一份 `-.json`，否则重启风暴会把排障记录冲干净。
    pub fn write_as(&self, runs_dir: &Path, name: &str) -> Result<PathBuf, SpoolError> {
        std::fs::create_dir_all(runs_dir).map_err(SpoolError::from)?;
        let path = runs_dir.join(format!("{}.json", crate::scope::sanitize(name)));
        let tmp = path.with_extension("json.tmp");
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|err| SpoolError::new(format!("报告序列化失败: {err}")))?;
        {
            let mut file = std::fs::File::create(&tmp).map_err(SpoolError::from)?;
            file.write_all(&raw).map_err(SpoolError::from)?;
            file.sync_all().map_err(SpoolError::from)?;
        }
        std::fs::rename(&tmp, &path).map_err(SpoolError::from)?;
        Ok(path)
    }
}
