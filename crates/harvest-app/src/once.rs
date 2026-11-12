//! `once`：跑一轮就退出。

use std::path::PathBuf;

use harvest_core::run::{CancelFlag, RunOptions};
use harvest_core::state::RunState;
use serde_json::json;

use crate::round;

pub async fn run(
    config_path: PathBuf,
    source_path: PathBuf,
    fresh: bool,
    snap_id: Option<String>,
) -> i32 {
    let cfg = match round::RoundConfig::load(&config_path, &source_path) {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("{message}");
            return 3;
        }
    };

    let opts = RunOptions {
        resume: !fresh,
        snap_id,
        cancel: CancelFlag::default(),
    };

    match round::execute_with(&cfg, opts).await {
        Ok(summary) => {
            let out = json!({
                "snap_id": summary.snap_id,
                "state": summary.state.as_str(),
                "published": summary.published,
                "report": summary.report_path.as_ref().map(|p| p.display().to_string()),
                "per_type": summary.report.per_type,
                "abort_reason": summary.report.abort_reason,
                "errors": summary.report.errors,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            if summary.state == RunState::Done {
                0
            } else {
                2
            }
        }
        Err(err) => {
            eprintln!("本轮无法开始: {err}");
            3
        }
    }
}
