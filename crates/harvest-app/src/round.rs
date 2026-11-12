//! 一轮采集：`once` 与常驻 `run` 共用同一份实现。
//!
//! 区分两种失败：
//!
//! - `Ok(summary)` 且 `state == Aborted`：**跑了但数据不完整**（有 spool 与报告）；
//! - `Err`：**本轮还没开始就失败**（配置/凭据/端点/初始化）。
//!
//! 调度侧两种都按失败处理，但日志与报告位置不同。

use std::path::{Path, PathBuf};

use harvest_core::config::{ServiceConfig, SourceConfig};
use harvest_core::error::HarvestError;
use harvest_core::redact;
use harvest_core::report::RunReport;
use harvest_core::run::{CancelFlag, RunOptions, run_once};
use harvest_core::state::RunState;
use harvest_tencent::TencentSource;
use harvest_warp::WarpSink;

pub struct RoundConfig {
    pub service: ServiceConfig,
    pub source: SourceConfig,
}

impl RoundConfig {
    pub fn load(config_path: &Path, source_path: &Path) -> Result<Self, String> {
        let service =
            ServiceConfig::load(config_path).map_err(|err| format!("服务配置不可用: {err}"))?;
        let source =
            SourceConfig::load(source_path).map_err(|err| format!("数据源配置不可用: {err}"))?;
        Ok(Self { service, source })
    }
}

pub struct RoundSummary {
    pub state: RunState,
    pub snap_id: String,
    pub published: bool,
    pub report_path: Option<PathBuf>,
    pub report: RunReport,
}

impl RoundSummary {
    pub fn is_success(&self) -> bool {
        self.state == RunState::Done
    }
}

/// 执行一轮：建 sink → 解析数据源（含凭据）→ 走一遍 `run_once`。
pub async fn execute(
    cfg: &RoundConfig,
    resume: bool,
    cancel: CancelFlag,
) -> Result<RoundSummary, HarvestError> {
    execute_with(
        cfg,
        RunOptions {
            resume,
            snap_id: None,
            cancel,
        },
    )
    .await
}

/// 同上，但允许指定 `snap_id`（`once --snap-id` 用）。
pub async fn execute_with(
    cfg: &RoundConfig,
    opts: RunOptions,
) -> Result<RoundSummary, HarvestError> {
    // 先建 sink：端点/证书有问题就在联网抓取前失败。
    let sink = WarpSink::from_settings(&cfg.service.sink)?;
    let source = TencentSource::init(cfg.source.clone(), &cfg.service).await?;
    let scope = source.scope();

    let outcome = run_once(&cfg.service, &scope, &source, &sink, &opts).await;
    Ok(RoundSummary {
        state: outcome.state,
        snap_id: outcome.snap_id,
        published: outcome.published,
        report_path: outcome.report_path,
        report: outcome.report,
    })
}

/// 一轮的终态，给调度器记账用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    /// 完整并已发布。
    Done,
    /// 跑了但中止（数据平面零帧）。
    Aborted,
    /// 还没开始就失败（配置/凭据/初始化）。
    StartFailed,
}

impl RoundOutcome {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Done)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Aborted => "Aborted",
            Self::StartFailed => "StartFailed",
        }
    }
}

/// 调度器调用的一轮：任何失败都收敛成 [`RoundOutcome`]，不向上抛。
pub async fn run_round(
    config_path: &Path,
    source_path: &Path,
    resume: bool,
    cancel: CancelFlag,
) -> RoundOutcome {
    let cfg = match RoundConfig::load(config_path, source_path) {
        Ok(cfg) => cfg,
        Err(message) => {
            redact::error(message);
            return RoundOutcome::StartFailed;
        }
    };
    match execute(&cfg, resume, cancel).await {
        Ok(summary) => {
            if summary.is_success() {
                redact::info(format!(
                    "本轮完成：snap_id={} 共 {} 类 / {} 行",
                    summary.snap_id,
                    summary.report.per_type.len(),
                    summary
                        .report
                        .per_type
                        .values()
                        .map(|t| t.rows)
                        .sum::<u64>()
                ));
                RoundOutcome::Done
            } else {
                redact::error(format!(
                    "本轮 Aborted：snap_id={} 原因={}",
                    summary.snap_id,
                    summary
                        .report
                        .abort_reason
                        .clone()
                        .unwrap_or_else(|| "未知".to_string())
                ));
                RoundOutcome::Aborted
            }
        }
        Err(err) => {
            redact::error(format!("本轮无法开始: {err}"));
            RoundOutcome::StartFailed
        }
    }
}
