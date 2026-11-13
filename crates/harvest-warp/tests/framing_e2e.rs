//! `WarpSink` 端到端：长度前缀帧 + ACK，以及"`Aborted` 零帧"。
//!
//! 这里的 sink 是**真的** `WarpSink`（不是 mock trait），只是对端换成本地
//! TCP 服务器，所以除了 TLS 之外的收发路径都被覆盖了。

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harvest_core::config::{ServiceConfig, SinkSettings};
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::model::{AssetType, CloudHost, Vendor};
use harvest_core::run::{RunOptions, run_once};
use harvest_core::scope::Scope;
use harvest_core::source::{CompleteSnapshot, FetchStat, LANE_DEFAULT, Sink, Source, WorkItem};
use harvest_core::spool::{Manifest, Spool};
use harvest_core::state::RunState;
use harvest_warp::{WarpSink, frame};
use serde_json::{Value, json};
use tokio::net::TcpListener;

struct MockDataPlane {
    endpoint: String,
    frames: Arc<Mutex<Vec<Vec<u8>>>>,
    connections: Arc<AtomicUsize>,
}

impl MockDataPlane {
    async fn spawn(ack: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let endpoint = listener.local_addr().unwrap().to_string();
        let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));

        let frames_bg = Arc::clone(&frames);
        let connections_bg = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                connections_bg.fetch_add(1, Ordering::SeqCst);
                let frames = Arc::clone(&frames_bg);
                let ack = ack.clone();
                tokio::spawn(async move {
                    if let Ok(payload) = frame::read_frame(&mut socket, 64 * 1024 * 1024).await {
                        frames.lock().unwrap().push(payload);
                        let ack_bytes = serde_json::to_vec(&ack).unwrap();
                        let _ = frame::write_frame(&mut socket, &ack_bytes).await;
                    }
                });
            }
        });

        Self {
            endpoint,
            frames,
            connections,
        }
    }

    /// 只看帧、**不回应答** —— 模拟 warp-parse 的 `tcp_src`。
    async fn spawn_silent() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let endpoint = listener.local_addr().unwrap().to_string();
        let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));

        let frames_bg = Arc::clone(&frames);
        let connections_bg = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                connections_bg.fetch_add(1, Ordering::SeqCst);
                let frames = Arc::clone(&frames_bg);
                tokio::spawn(async move {
                    if let Ok(payload) = frame::read_frame(&mut socket, 64 * 1024 * 1024).await {
                        frames.lock().unwrap().push(payload);
                    }
                });
            }
        });

        Self {
            endpoint,
            frames,
            connections,
        }
    }

    fn frames(&self) -> Vec<Vec<u8>> {
        self.frames.lock().unwrap().clone()
    }

    /// 等帧到达（最多约 2s）。
    ///
    /// `expect_ack = false` 时发布不再串行于服务端读帧，断言前必须等 —— 否则是竞态，
    /// 本地偶尔幸过、CI 上必挂。
    async fn wait_frames(&self, expected: usize) -> Vec<Vec<u8>> {
        for _ in 0..200 {
            let frames = self.frames();
            if frames.len() >= expected {
                return frames;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.frames()
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

fn plaintext_sink(endpoint: &str) -> WarpSink {
    WarpSink::from_settings(&SinkSettings {
        endpoint: endpoint.to_string(),
        allow_plaintext: true,
        ..Default::default()
    })
    .expect("明文 sink 应可构建")
}

/// 只给「真的会回应答」的测试用；默认（`expect_ack = false`）不等 ACK。
fn plaintext_sink_expecting_ack(endpoint: &str) -> WarpSink {
    WarpSink::from_settings(&SinkSettings {
        endpoint: endpoint.to_string(),
        allow_plaintext: true,
        expect_ack: true,
        ..Default::default()
    })
    .expect("明文 sink 应可构建")
}

fn snapshot() -> CompleteSnapshot {
    CompleteSnapshot {
        snap_id: "snap-1".to_string(),
        bytes: br#"{"schema_native":"wist.harvest.snapshot.v1"}"#.to_vec(),
        rows: 0,
        per_type: Default::default(),
    }
}

#[tokio::test]
async fn publishes_one_length_prefixed_frame_and_reads_ack() {
    let server = MockDataPlane::spawn(json!({"status": "ok"})).await;
    let sink = plaintext_sink_expecting_ack(&server.endpoint);

    let ack = sink
        .publish_snapshot(&snapshot())
        .await
        .expect("发布应成功");
    assert_eq!(ack.ack, "ok");
    assert_eq!(ack.bytes, snapshot().bytes.len() as u64);

    let frames = server.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0],
        snapshot().bytes,
        "线上的 payload 必须与帧体逐字节一致"
    );
}

#[tokio::test]
async fn rejection_is_reported_not_swallowed() {
    let server = MockDataPlane::spawn(json!({"status": "error", "message": "bad schema"})).await;
    let sink = plaintext_sink_expecting_ack(&server.endpoint);

    let err = sink.publish_snapshot(&snapshot()).await.unwrap_err();
    assert!(
        err.message.contains("拒绝接收"),
        "必须把拒绝原因带出来: {}",
        err.message
    );
}

// ---------------------------------------------------------------------------
// Aborted → 零帧（用真的 WarpSink）
// ---------------------------------------------------------------------------

struct FailingSource {
    plan: Vec<WorkItem>,
}

#[async_trait]
impl Source for FailingSource {
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
        if item.region == "ap-shanghai" {
            return Err(HarvestError::new(
                ErrorClass::RateLimited,
                "模拟限流耗尽重试",
            ));
        }
        let rows: Vec<CloudHost> = vec![CloudHost {
            source_asset_id: "ins-1".into(),
            region: item.region.clone(),
            ..Default::default()
        }];
        spool.append_rows(item, &rows)?;
        Ok(FetchStat {
            rows: 1,
            pages: 1,
            completeness: Some(harvest_core::source::Completeness::Total { total: 1 }),
            expected_rows: Some(1),
            ..Default::default()
        })
    }

    fn verify(&self, manifest: &Manifest) -> Result<(), HarvestError> {
        harvest_core::integrity::verify_manifest(
            manifest,
            &harvest_core::config::AllowEmpty::All(false),
        )
        .map(|_| ())
    }
}

fn service_config(spool_dir: &Path, state_dir: &Path, endpoint: &str) -> ServiceConfig {
    ServiceConfig::parse(&format!(
        r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"
page_size = 100

[sink]
endpoint = "{endpoint}"
allow_plaintext = true
"#,
        spool = spool_dir.display(),
        state = state_dir.display(),
    ))
    .unwrap()
}

#[tokio::test]
async fn aborted_run_sends_zero_frames_over_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockDataPlane::spawn(json!({"status": "ok"})).await;
    let svc = service_config(
        &dir.path().join("spool"),
        &dir.path().join("state"),
        &server.endpoint,
    );
    let sink = plaintext_sink(&server.endpoint);

    let scope = Scope::new(
        "tenant-example",
        "tencent-cloud-prod",
        "inventory",
        "prod",
        Vendor::TencentCloud,
        vec!["ap-guangzhou".into(), "ap-shanghai".into()],
    );
    let source = FailingSource {
        plan: vec![
            WorkItem::new(AssetType::CloudHost, "ap-guangzhou", LANE_DEFAULT),
            WorkItem::new(AssetType::CloudHost, "ap-shanghai", LANE_DEFAULT),
        ],
    };

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(outcome.state, RunState::Aborted);
    assert_eq!(server.frames().len(), 0, "Aborted 必须在线上也是零帧");
    assert_eq!(
        server.connections(),
        0,
        "Aborted 连连接都不该建立（发送只在 Verified 之后）"
    );
}

/// 保证 `Sink` trait 的实现确实被用到了（否则上面那个测试会假通过）。
#[tokio::test]
async fn successful_run_sends_exactly_one_frame() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockDataPlane::spawn(json!({"status": "ok"})).await;
    let svc = service_config(
        &dir.path().join("spool"),
        &dir.path().join("state"),
        &server.endpoint,
    );
    let sink = plaintext_sink(&server.endpoint);
    let scope = Scope::new(
        "tenant-example",
        "tencent-cloud-prod",
        "inventory",
        "prod",
        Vendor::TencentCloud,
        vec!["ap-guangzhou".into()],
    );
    let source = FailingSource {
        plan: vec![WorkItem::new(
            AssetType::CloudHost,
            "ap-guangzhou",
            LANE_DEFAULT,
        )],
    };

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done);
    // 不等 ACK 时「发布成功」不再串行于服务端读帧，所以要等一下（最多约 2s）。
    let frames = server.wait_frames(1).await;
    assert_eq!(frames.len(), 1);

    // 线上记录 = `<JSON 头>CLOUDINV:<正文>\0`；快照在正文里。
    let symbol = harvest_core::wire::WIRE_SYMBOL.as_bytes();
    let at = frames[0]
        .windows(symbol.len())
        .position(|w| w == symbol)
        .expect("记录里必须有 CLOUDINV: 符号");
    let payload: Value = serde_json::from_slice(&frames[0][at + symbol.len()..]).unwrap();
    assert_eq!(payload["snap_id"], json!(outcome.snap_id));
}

#[tokio::test]
async fn publishes_without_ack_when_data_plane_stays_silent() {
    // warp-parse 的 tcp_src 不回 ACK：默认 expect_ack=false，写完即成功。
    // 不能因为「没等到应答」把已经送达的帧记成 Aborted —— 那会导致 spool 永不清理。
    let server = MockDataPlane::spawn_silent().await;
    let sink = plaintext_sink(&server.endpoint);

    let ack = sink
        .publish_snapshot(&snapshot())
        .await
        .expect("无应答也应视为已送达");
    assert_eq!(ack.ack, "none");
    let frames = server.wait_frames(1).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0], snapshot().bytes);
}
