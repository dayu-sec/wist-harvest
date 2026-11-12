//! 不变量测试（设计说明 §5）：
//!
//! 1. `Aborted` 状态下数据平面零帧；
//! 2. 分页总数不一致 → `Aborted`；
//! 3. 续跑不重抓已完成工作项；
//! 4. 同一 `snap_id` 重放，帧体逐字节一致（下游按幂等键去重）。

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use harvest_core::config::ServiceConfig;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::model::{AssetType, CloudHost, Vendor};
use harvest_core::run::{RunOptions, run_once};
use harvest_core::scope::Scope;
use harvest_core::source::{
    CompleteSnapshot, FetchStat, LANE_DEFAULT, PublishAck, Sink, Source, WorkItem,
};
use harvest_core::spool::Spool;
use harvest_core::state::RunState;

/// 一个可编程的假数据源：每个地域产 `rows` 行，可指定在某个工作项上失败，
/// 也可以故意把 `expected_rows` 报错，用来验证完整性判定。
struct FakeSource {
    plan: Vec<WorkItem>,
    rows: usize,
    fail_on: Mutex<Option<String>>,
    expected_rows: Option<u64>,
    calls: Mutex<Vec<String>>,
}

impl FakeSource {
    fn new(regions: &[&str]) -> Self {
        Self {
            plan: regions
                .iter()
                .map(|region| WorkItem::new(AssetType::CloudHost, *region, LANE_DEFAULT))
                .collect(),
            rows: 2,
            fail_on: Mutex::new(None),
            expected_rows: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn fail_on(&self, key: impl Into<String>) {
        *self.fail_on.lock().unwrap() = Some(key.into());
    }

    fn clear_failures(&self) {
        *self.fail_on.lock().unwrap() = None;
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Source for FakeSource {
    fn vendor(&self) -> Vendor {
        Vendor::TencentCloud
    }

    fn plan(&self) -> Vec<WorkItem> {
        self.plan.clone()
    }

    async fn fetch(&self, item: &WorkItem, spool: &mut Spool) -> Result<FetchStat, HarvestError> {
        self.calls.lock().unwrap().push(item.key());
        if self.fail_on.lock().unwrap().as_deref() == Some(item.key().as_str()) {
            return Err(
                HarvestError::new(ErrorClass::ServerError, "模拟云端 5xx（重试后仍失败）")
                    .with_region(&item.region),
            );
        }

        let rows: Vec<CloudHost> = (0..self.rows)
            .map(|idx| CloudHost {
                host_id: format!("ins-{}-{idx}", item.region),
                host_name: format!("host-{}-{idx}", item.region),
                source_asset_id: format!("ins-{}-{idx}", item.region),
                region: item.region.clone(),
                ..Default::default()
            })
            .collect();
        spool.append_rows(item, &rows)?;

        Ok(FetchStat {
            rows: rows.len() as u64,
            pages: 1,
            expected_rows: Some(self.expected_rows.unwrap_or(rows.len() as u64)),
            retries: 0,
            request_ids: vec![format!("req-{}", item.key())],
            duration_ms: 1,
        })
    }

    fn verify(&self, manifest: &harvest_core::spool::Manifest) -> Result<(), HarvestError> {
        harvest_core::integrity::verify_manifest(manifest, false).map(|_| ())
    }
}

#[derive(Default)]
struct RecordingSink {
    frames: Mutex<Vec<CompleteSnapshot>>,
}

impl RecordingSink {
    fn frames(&self) -> Vec<CompleteSnapshot> {
        self.frames.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sink for RecordingSink {
    async fn publish_snapshot(&self, snap: &CompleteSnapshot) -> Result<PublishAck, HarvestError> {
        self.frames.lock().unwrap().push(snap.clone());
        Ok(PublishAck {
            bytes: snap.bytes.len() as u64,
            ack: "ok".to_string(),
            duration_ms: 1,
        })
    }
}

fn service_config(spool_dir: &Path, state_dir: &Path) -> ServiceConfig {
    ServiceConfig::parse(&format!(
        r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"
page_size = 100

[sink]
endpoint = "127.0.0.1:1"
allow_plaintext = true
"#,
        spool = spool_dir.display(),
        state = state_dir.display(),
    ))
    .expect("测试配置应可解析")
}

fn scope() -> Scope {
    Scope::new(
        "tenant-example",
        "tencent-cloud-prod",
        "inventory",
        "prod",
        Vendor::TencentCloud,
        vec!["ap-guangzhou".into(), "ap-shanghai".into()],
    )
}

fn spool_dir_for(spool_root: &Path, scope: &Scope, snap_id: &str) -> std::path::PathBuf {
    spool_root.join(scope.spool_prefix()).join(snap_id)
}

#[tokio::test]
async fn aborted_run_sends_zero_frames() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    source.fail_on("CLOUD_HOST/ap-shanghai.default");
    let sink = RecordingSink::default();

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(outcome.state, RunState::Aborted);
    assert!(!outcome.published);
    assert_eq!(sink.frames().len(), 0, "Aborted 必须零帧");

    // spool 与失败原因都要保留，供排障。
    let dir_path = spool_dir_for(&svc.service.spool_dir, &scope, &outcome.snap_id);
    let spool = Spool::open_existing(&dir_path).expect("Aborted 也要保留 manifest");
    assert_eq!(spool.manifest().state, RunState::Aborted);
    let error = spool.manifest().error.as_ref().expect("必须记录失败原因");
    assert_eq!(error.class, ErrorClass::ServerError);
    assert!(error.message.contains("模拟云端"), "{}", error.message);
}

#[tokio::test]
async fn page_total_mismatch_aborts_with_zero_frames() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let mut source = FakeSource::new(&["ap-guangzhou"]);
    source.rows = 2;
    // 云端说 3 条，实收 2 条 —— 分页没抓完，绝不能当成"只有 2 台"发出去。
    source.expected_rows = Some(3);
    let sink = RecordingSink::default();

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(outcome.state, RunState::Aborted);
    assert_eq!(sink.frames().len(), 0);
    let errors = &outcome.report.errors;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].class, "integrity");
    assert!(
        errors[0].message.contains("分页总数不一致"),
        "{}",
        errors[0].message
    );
}

#[tokio::test]
async fn resume_skips_completed_items() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    source.fail_on("CLOUD_HOST/ap-shanghai.default");
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(first.state, RunState::Aborted);
    assert_eq!(
        source.calls(),
        vec![
            "CLOUD_HOST/ap-guangzhou.default",
            "CLOUD_HOST/ap-shanghai.default"
        ]
    );

    source.clear_failures();
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(second.state, RunState::Done);
    assert!(second.report.resumed, "第二轮应续跑同一 spool");
    assert_eq!(second.snap_id, first.snap_id, "续跑沿用同一 snap_id");
    assert_eq!(
        source.calls(),
        vec![
            "CLOUD_HOST/ap-guangzhou.default",
            "CLOUD_HOST/ap-shanghai.default",
            // 只有失败的那项被重抓
            "CLOUD_HOST/ap-shanghai.default",
        ]
    );
    assert_eq!(sink.frames().len(), 1);
}

#[tokio::test]
async fn replay_same_snap_id_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(first.state, RunState::Done);

    // 模拟"发布超时、按未确认处理"：把状态退回 Verified，再跑一轮。
    let snap_dir = spool_dir_for(&svc.service.spool_dir, &scope, &first.snap_id);
    let mut spool = Spool::open_existing(&snap_dir).unwrap();
    spool.set_state(RunState::Verified);
    spool.save().unwrap();

    let calls_before = source.calls().len();
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(second.state, RunState::Done);
    assert_eq!(source.calls().len(), calls_before, "重放不应再抓数据");

    let frames = sink.frames();
    assert_eq!(frames.len(), 2, "同一 snap_id 发了两次");
    assert_eq!(frames[0].snap_id, frames[1].snap_id);
    assert_eq!(
        frames[0].bytes, frames[1].bytes,
        "同一 spool 重放必须逐字节一致（含 collected_at）"
    );

    let payload = wire_body(&frames[0].bytes);
    assert_eq!(payload["snap_id"], serde_json::json!(first.snap_id));
    assert_eq!(payload["assets"]["CLOUD_HOST"].as_array().unwrap().len(), 4);
}

/// 从线上记录里取快照正文：`<JSON 头>CLOUDINV:<正文>\0`。
fn wire_body(bytes: &[u8]) -> serde_json::Value {
    let symbol = harvest_core::wire::WIRE_SYMBOL.as_bytes();
    let at = bytes
        .windows(symbol.len())
        .position(|w| w == symbol)
        .expect("记录里必须有 CLOUDINV: 符号");
    let body = &bytes[at + symbol.len()..];
    serde_json::from_slice(body).expect("正文应是合法 JSON")
}

#[tokio::test]
async fn fresh_run_gets_a_new_snap_id_and_does_not_resume() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = Scope::new(
        "tenant-example",
        "tencent-cloud-prod",
        "inventory",
        "prod",
        Vendor::TencentCloud,
        vec!["ap-guangzhou".into()],
    );
    let source = FakeSource::new(&["ap-guangzhou"]);
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::fresh()).await;
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::fresh()).await;
    assert_eq!(first.state, RunState::Done);
    assert_eq!(second.state, RunState::Done);
    assert_ne!(
        first.report.snap_id, second.report.snap_id,
        "新一轮必须是新 snap_id（同一秒内也不能撞车）"
    );
}
