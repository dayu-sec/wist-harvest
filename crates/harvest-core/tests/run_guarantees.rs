//! 不变量测试（设计说明 §5）：
//!
//! 1. `Aborted` 状态下数据平面零帧；
//! 2. 分页总数不一致 → `Aborted`；
//! 3. 续跑不重抓已完成工作项；
//! 4. 同一 `snap_id` 重放，帧体逐字节一致（下游按幂等键去重）。

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use harvest_core::config::{AllowEmpty, ServiceConfig};
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::model::{AssetType, CloudHost, Vendor};
use harvest_core::run::{RunOptions, run_once};
use harvest_core::scope::Scope;
use harvest_core::source::{
    CompleteSnapshot, Completeness, FetchStat, LANE_DEFAULT, PublishAck, Sink, Source, WorkItem,
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
    allow_empty: AllowEmpty,
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
            allow_empty: AllowEmpty::All(false),
        }
    }

    /// 改采集计划（模拟改了配置后重启）：多一条分页道，地域集合不变。
    fn add_lane(&mut self, region: &str, lane: &str) {
        self.plan
            .push(WorkItem::new(AssetType::CloudHost, region, lane));
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

    fn scope(&self) -> Scope {
        let mut regions: Vec<String> = self.plan.iter().map(|item| item.region.clone()).collect();
        regions.sort();
        regions.dedup();
        Scope::new(
            "tenant-example",
            "tencent-cloud-prod",
            "inventory",
            "prod",
            Vendor::TencentCloud,
            regions,
        )
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
                host_id: format!("ins-{}-{}-{idx}", item.region, item.lane),
                host_name: format!("host-{}-{idx}", item.region),
                source_asset_id: format!("ins-{}-{}-{idx}", item.region, item.lane),
                region: item.region.clone(),
                ..Default::default()
            })
            .collect();
        spool.append_rows(item, &rows)?;

        let total = self.expected_rows.unwrap_or(rows.len() as u64);
        Ok(FetchStat {
            rows: rows.len() as u64,
            pages: 1,
            completeness: Some(Completeness::Total { total }),
            expected_rows: Some(total),
            retries: 0,
            request_ids: vec![format!("req-{}", item.key())],
            duration_ms: 1,
        })
    }

    fn verify(&self, manifest: &harvest_core::spool::Manifest) -> Result<(), HarvestError> {
        harvest_core::integrity::verify_manifest(manifest, &self.allow_empty).map(|_| ())
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
    service_config_with(spool_dir, state_dir, "")
}

/// 同上，额外给一段 `[service]` 覆写（例如 `max_resume_age_ms`）。
fn service_config_with(spool_dir: &Path, state_dir: &Path, extra: &str) -> ServiceConfig {
    ServiceConfig::parse(&format!(
        r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"
page_size = 100
{extra}
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

/// 从线上记录里取快照正文：`<JSON 头>CLOUDINV:<正文>`（无 NUL）。
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

#[tokio::test]
async fn plan_change_abandons_the_old_spool_instead_of_deadlocking() {
    // 改了采集计划（例如 `[assets]` 开关）后，旧 spool 永远不可能变合法。
    // 若它继续可续跑，采集器会每轮撞在同一个校验错误上，永不发布。
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let mut source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    source.fail_on("CLOUD_HOST/ap-shanghai.default");
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(first.state, RunState::Aborted);
    let old_dir = spool_dir_for(&svc.service.spool_dir, &scope, &first.snap_id);
    assert_eq!(
        Spool::open_existing(&old_dir).unwrap().manifest().state,
        RunState::Aborted
    );

    // 计划多了一条分页道（相当于现场改了 sources.d）；地域集合没变。
    source.clear_failures();
    source.add_lane("ap-guangzhou", "extra");
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(second.state, RunState::Done, "{:?}", second.report.errors);
    assert!(!second.report.resumed, "不能续跑计划已变的旧 spool");
    assert_ne!(second.snap_id, first.snap_id, "必须是新的 snap_id");
    assert_eq!(
        Spool::open_existing(&old_dir).unwrap().manifest().state,
        RunState::Abandoned,
        "旧 spool 保留供诊断，但不再可续跑"
    );

    // 第三轮不应再挑到那份 Abandoned 的 spool。
    let third = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(third.state, RunState::Done);
    assert!(!third.report.resumed, "Abandoned 的 spool 不能被续跑");
    assert_ne!(third.snap_id, second.snap_id);
}

#[tokio::test]
async fn a_missing_spool_file_aborts_instead_of_publishing_zero_rows() {
    // manifest 说「这些工作项有 N 行」，但文件被清理/回滚了。
    // 只信 manifest 就会发出一条 0 行且 complete=true 的快照（图上的资产被误删）。
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(first.state, RunState::Done);
    assert_eq!(sink.frames().len(), 1);

    // 删掉 spool 里的数据文件，把状态退回 Verified（模拟「重发前发现文件没了」）。
    let snap_dir = spool_dir_for(&svc.service.spool_dir, &scope, &first.snap_id);
    let mut spool = Spool::open_existing(&snap_dir).unwrap();
    for record in &spool.manifest().items {
        std::fs::remove_file(snap_dir.join(&record.file)).unwrap();
    }
    spool.set_state(RunState::Verified);
    spool.save().unwrap();
    let calls_before = source.calls().len();

    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(second.state, RunState::Aborted);
    assert_eq!(second.snap_id, first.snap_id, "续跑的是同一份 spool");
    assert_eq!(sink.frames().len(), 1, "不能多发一条 0 行的快照");
    assert_eq!(source.calls().len(), calls_before, "已完成的工作项不应重抓");
    assert!(
        second
            .report
            .abort_reason
            .as_deref()
            .unwrap_or_default()
            .contains("不可读"),
        "{:?}",
        second.report.abort_reason
    );
}

#[tokio::test]
async fn an_expired_spool_is_abandoned_instead_of_resumed_forever() {
    // 永久性的局部失败（没权限的地域、不开放的产物）会让旧 spool 永远续不完。
    // 超过时限就必须放弃，否则采集器永远不产出新快照，`collected_at` 也永远停在过去。
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config_with(
        &dir.path().join("spool"),
        &dir.path().join("state"),
        "max_resume_age_ms = 1",
    );
    let scope = scope();
    let source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    source.fail_on("CLOUD_HOST/ap-shanghai.default");
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(first.state, RunState::Aborted);
    let old_dir = spool_dir_for(&svc.service.spool_dir, &scope, &first.snap_id);

    // 把创建时间推到很早（不必真等）。
    let mut spool = Spool::open_existing(&old_dir).unwrap();
    spool.manifest_mut().created_at = "2020-01-01T00:00:00.000Z".to_string();
    spool.save().unwrap();

    source.clear_failures();
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(second.state, RunState::Done, "{:?}", second.report.errors);
    assert!(!second.report.resumed, "过期的 spool 不能续跑");
    assert_ne!(second.snap_id, first.snap_id);
    assert_eq!(
        Spool::open_existing(&old_dir).unwrap().manifest().state,
        RunState::Abandoned
    );
}

#[tokio::test]
async fn an_empty_type_needs_an_explicit_allowance() {
    // 0 行默认是可疑的（空 ≠ 失败）。
    let dir = tempfile::tempdir().unwrap();
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let scope = scope();
    let mut source = FakeSource::new(&["ap-guangzhou", "ap-shanghai"]);
    source.rows = 0;
    let sink = RecordingSink::default();

    let first = run_once(&svc, &scope, &source, &sink, &RunOptions::fresh()).await;
    assert_eq!(first.state, RunState::Aborted);
    assert_eq!(sink.frames().len(), 0, "0 行不能发");

    // 只给这一类型显式放行：仍然要能发出去（确实是空的账号）。
    source.allow_empty = AllowEmpty::Types(vec![AssetType::CloudHost]);
    let second = run_once(&svc, &scope, &source, &sink, &RunOptions::fresh()).await;
    assert_eq!(second.state, RunState::Done, "{:?}", second.report.errors);
    assert_eq!(sink.frames().len(), 1);
    let payload = wire_body(&sink.frames()[0].bytes);
    assert_eq!(
        payload["stats"]["CLOUD_HOST"]["total"],
        serde_json::json!(0)
    );
}
