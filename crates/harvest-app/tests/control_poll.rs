//! 中心配置轮询的端到端行为：harvest **自己**拉 `harvest.enabled`。
//!
//! 三个必须钉住的行为（都是「中心说关」时最容易错的地方）：
//!
//! 1. 中心说关闭 → **一轮都不跑**（哪怕本地 `enabled=true`、哪怕 `--now`）；
//! 2. 中心说开启 → 正常按调度跑；
//! 3. 中心不可达 / 返回 500 → 沿用本地值（首拍失败则**不采**，fail-closed）。
//!
//! 用本地 TCP 假服务，不联网。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harvest_app::round::RoundOutcome;
use harvest_app::serve::{ControlPlan, ServeOptions, serve};
use harvest_core::schedule::{Backoff, ScheduleMode, SchedulerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 起一个只会回固定响应的假中心。返回它的 URL。
async fn stub_center(status: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let status = status.to_string();
            let body = body.to_string();
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}/internal/harvest")
}

fn fast_scheduler() -> SchedulerConfig {
    SchedulerConfig {
        mode: ScheduleMode::Interval {
            every: Duration::from_millis(20),
        },
        jitter_max: Duration::ZERO,
        backoff: Backoff {
            base: Duration::from_millis(10),
            max: Duration::from_millis(20),
        },
        max_retries_per_cycle: 0,
    }
}

async fn run_with_control(url: String, window: Duration) -> (usize, usize) {
    let rounds = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&rounds);
    let stats = serve(
        ServeOptions {
            enabled: true,
            control: Some(ControlPlan {
                url,
                token: None,
                poll_interval: Duration::from_millis(30),
            }),
            scheduler: fast_scheduler(),
            resume: true,
            shutdown_grace: Duration::from_millis(50),
            run_immediately: true,
        },
        async move { tokio::time::sleep(window).await },
        move |_cancel| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RoundOutcome::Done
            }
        },
    )
    .await;
    (rounds.load(Ordering::SeqCst), stats.started as usize)
}

#[tokio::test]
async fn center_disabled_blocks_every_round() {
    let url = stub_center("200 OK", r#"{"enabled":false}"#).await;
    let (rounds, started) = run_with_control(url, Duration::from_millis(400)).await;
    assert_eq!(rounds, 0, "中心关闭时一轮都不该跑（连 --now 也不跑）");
    assert_eq!(started, 0);
}

#[tokio::test]
async fn center_enabled_runs_on_schedule() {
    let url = stub_center("200 OK", r#"{"enabled":true,"revision":3}"#).await;
    let (rounds, _) = run_with_control(url, Duration::from_millis(400)).await;
    assert!(rounds >= 1, "中心开启时应按调度正常跑，实际 {rounds} 轮");
}

#[tokio::test]
async fn unreachable_center_is_fail_closed() {
    // 首拍就拿不到中心配置 → 不采（宁可停采，也不要在「中心说关」时偷偷采一轮）。
    let (rounds, started) = run_with_control(
        "http://127.0.0.1:9/internal/harvest".to_string(),
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(rounds, 0);
    assert_eq!(started, 0);
}

#[tokio::test]
async fn center_server_error_is_fail_closed() {
    let url = stub_center("500 Internal Server Error", r#"{"error":"boom"}"#).await;
    let (rounds, _) = run_with_control(url, Duration::from_millis(300)).await;
    assert_eq!(rounds, 0);
}
