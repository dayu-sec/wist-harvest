//! 常驻调度的行为测试：单轮并发、失败退避、优雅退出。
//!
//! 时间和轮次都由测试注入，不去真的等到 00:30。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use harvest_app::round::RoundOutcome;
use harvest_app::serve::{ServeOptions, serve};
use harvest_core::schedule::{Backoff, ScheduleMode, SchedulerConfig};

fn interval_scheduler(
    every: Duration,
    max_retries: u32,
    base: Duration,
    max: Duration,
) -> SchedulerConfig {
    SchedulerConfig {
        mode: ScheduleMode::Interval { every },
        jitter_max: Duration::ZERO,
        backoff: Backoff { base, max },
        max_retries_per_cycle: max_retries,
    }
}

fn schedule_shutdown(after: Duration) -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let _ = tx.send(());
    });
    rx
}

#[tokio::test]
async fn skips_trigger_while_previous_round_is_running() {
    // 上一轮一直不结束（闸门永不开），期间每次到点都必须跳过。
    let gate = Arc::new(AtomicBool::new(false));
    let gate_round = Arc::clone(&gate);
    let shutdown = schedule_shutdown(Duration::from_millis(150));

    let stats = serve(
        ServeOptions {
            enabled: true,
            control: None,
            scheduler: interval_scheduler(
                Duration::from_millis(20),
                0,
                Duration::from_millis(10),
                Duration::from_millis(20),
            ),
            resume: true,
            shutdown_grace: Duration::from_millis(50),
            run_immediately: false,
        },
        async move {
            let _ = shutdown.await;
        },
        move |_cancel| {
            let gate = Arc::clone(&gate_round);
            async move {
                // 刻意不响应取消：用来验证"宽限期内没收尾"的分支。
                while !gate.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                RoundOutcome::Done
            }
        },
    )
    .await;

    assert_eq!(stats.started, 1, "上一轮没结束就不该开第二轮: {stats:?}");
    assert!(
        stats.skipped >= 3,
        "到点但上一轮在跑时应跳过若干次: {stats:?}"
    );
    assert_eq!(stats.succeeded, 0);
    assert_eq!(stats.failed, 1, "宽限期内没收尾 → 记一次失败: {stats:?}");
}

#[tokio::test]
async fn failures_are_retried_with_backoff_within_one_cycle() {
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&attempts);
    let shutdown = schedule_shutdown(Duration::from_millis(250));

    let stats = serve(
        ServeOptions {
            enabled: true,
            control: None,
            // 定点很远（60s），所以这一轮只会被失败重试驱动。
            scheduler: interval_scheduler(
                Duration::from_secs(60),
                3,
                Duration::from_millis(20),
                Duration::from_millis(40),
            ),
            resume: true,
            run_immediately: true,
            shutdown_grace: Duration::from_millis(100),
        },
        async move {
            let _ = shutdown.await;
        },
        move |_cancel| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RoundOutcome::Aborted
            }
        },
    )
    .await;

    // 1 次定点 + 3 次退避重试（20ms → 40ms → 40ms，被 max 截断），之后等下一个定点。
    assert_eq!(stats.started, 4, "{stats:?}");
    assert_eq!(stats.failed, 4, "{stats:?}");
    assert_eq!(stats.succeeded, 0);
    assert_eq!(stats.skipped, 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 4);

    let triggers: Vec<&'static str> = stats.rounds.iter().map(|r| r.trigger.as_str()).collect();
    assert_eq!(triggers, vec!["scheduled", "retry", "retry", "retry"]);
}

#[tokio::test]
async fn success_stops_the_retry_chain() {
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&attempts);
    let shutdown = schedule_shutdown(Duration::from_millis(250));

    let stats = serve(
        ServeOptions {
            enabled: true,
            control: None,
            scheduler: interval_scheduler(
                Duration::from_secs(60),
                5,
                Duration::from_millis(20),
                Duration::from_millis(40),
            ),
            resume: true,
            run_immediately: true,
            shutdown_grace: Duration::from_millis(100),
        },
        async move {
            let _ = shutdown.await;
        },
        move |_cancel| {
            let counter = Arc::clone(&counter);
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                // 第一次失败，第二次成功。
                if attempt == 0 {
                    RoundOutcome::Aborted
                } else {
                    RoundOutcome::Done
                }
            }
        },
    )
    .await;

    assert_eq!(stats.started, 2, "成功后不应再重试: {stats:?}");
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.succeeded, 1);
}

#[tokio::test]
async fn shutdown_cancels_the_inflight_round() {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let watcher = Arc::clone(&saw_cancel);
    let shutdown = schedule_shutdown(Duration::from_millis(80));

    let stats = serve(
        ServeOptions {
            enabled: true,
            control: None,
            scheduler: interval_scheduler(
                Duration::from_secs(60),
                0,
                Duration::from_millis(10),
                Duration::from_millis(20),
            ),
            resume: true,
            run_immediately: true,
            shutdown_grace: Duration::from_millis(500),
        },
        async move {
            let _ = shutdown.await;
        },
        move |cancel| {
            let watcher = Arc::clone(&watcher);
            async move {
                for _ in 0..200 {
                    if cancel.is_cancelled() {
                        watcher.store(true, Ordering::SeqCst);
                        return RoundOutcome::Aborted;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                RoundOutcome::Done
            }
        },
    )
    .await;

    assert!(
        saw_cancel.load(Ordering::SeqCst),
        "收到退出信号后必须通知在飞轮次取消"
    );
    assert_eq!(stats.started, 1);
    assert_eq!(stats.failed, 1, "被取消的轮次按 Aborted 记: {stats:?}");
}

#[tokio::test]
async fn no_round_started_when_shutdown_arrives_before_the_trigger() {
    let started = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&started);
    let shutdown = schedule_shutdown(Duration::from_millis(50));

    let stats = serve(
        ServeOptions {
            enabled: true,
            control: None,
            scheduler: interval_scheduler(
                Duration::from_secs(3600),
                3,
                Duration::from_millis(10),
                Duration::from_millis(20),
            ),
            resume: true,
            shutdown_grace: Duration::from_millis(50),
            run_immediately: false,
        },
        async move {
            let _ = shutdown.await;
        },
        move |_cancel| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RoundOutcome::Done
            }
        },
    )
    .await;

    assert_eq!(stats.started, 0, "还没到点就退出，不应开轮");
    assert_eq!(started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disabled_service_stays_idle_and_runs_no_round() {
    // 关闭态：进程常驻等退出信号，**一轮都不跑**（管理面关采集时容器不必销毁）。
    let rounds = Arc::new(AtomicBool::new(false));
    let rounds_round = Arc::clone(&rounds);

    let stats = serve(
        ServeOptions {
            enabled: false,
            control: None,
            scheduler: interval_scheduler(
                Duration::from_millis(20),
                0,
                Duration::from_millis(10),
                Duration::from_millis(20),
            ),
            resume: true,
            shutdown_grace: Duration::from_millis(50),
            run_immediately: true,
        },
        async { tokio::time::sleep(Duration::from_millis(120)).await },
        move |_cancel| {
            let rounds = Arc::clone(&rounds_round);
            async move {
                rounds.store(true, Ordering::SeqCst);
                RoundOutcome::Done
            }
        },
    )
    .await;

    assert!(
        !rounds.load(Ordering::SeqCst),
        "关闭态不应跑任何一轮（连 run_immediately 也不跑）"
    );
    assert_eq!(stats.started, 0);
    assert_eq!(stats.succeeded, 0);
}
