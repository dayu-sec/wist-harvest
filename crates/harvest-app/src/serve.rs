//! `run`：常驻调度。
//!
//! 三条语义：
//!
//! 1. **到点就跑**：下一轮定点由墙上时钟算，失败与重试都不推迟它
//!    （退避只安排"额外的重试"，且重试晚于定点就被丢弃）。
//! 2. **同一时刻只跑一轮**：在飞轮次未结束时到达的下一次触发直接跳过并记 warn；
//!    跨进程另有一层 flock 租约（`run_once` 内部），跳过不会变成双采。
//! 3. **优雅退出**：收到 SIGTERM/SIGINT 后通知在飞轮次取消（抓取中的轮次进
//!    `Aborted`，保持零帧语义），最多等 `shutdown_grace`；没等到就退出，
//!    spool 留着下次续跑。退出码恒为 0。

use std::time::Duration;

use chrono::{DateTime, Utc};
use harvest_core::redact;
use harvest_core::run::CancelFlag;
use harvest_core::schedule::{
    RoundResult, RoundTrigger, Scheduler, SchedulerConfig, WakeOutcome, jitter_sample,
};
use tokio::task::JoinHandle;

pub use crate::round::RoundOutcome;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub scheduler: SchedulerConfig,
    /// 是否续跑未完成的 spool。
    pub resume: bool,
    /// 收到退出信号后等一轮收尾的宽限期。
    pub shutdown_grace: Duration,
    /// 启动后立即跑一轮（不等第一个定点）。
    pub run_immediately: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            scheduler: SchedulerConfig::default(),
            resume: true,
            shutdown_grace: Duration::from_secs(30),
            run_immediately: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoundLog {
    pub trigger: RoundTrigger,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: RoundOutcome,
}

#[derive(Debug, Default)]
pub struct ServeStats {
    pub started: u32,
    pub skipped: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub rounds: Vec<RoundLog>,
}

impl ServeStats {
    pub fn to_json(&self, stopped_by: &str) -> serde_json::Value {
        serde_json::json!({
            "stopped_by": stopped_by,
            "started": self.started,
            "skipped": self.skipped,
            "succeeded": self.succeeded,
            "failed": self.failed,
            "rounds": self.rounds.iter().map(|round| serde_json::json!({
                "trigger": round.trigger.as_str(),
                "started_at": round.started_at,
                "finished_at": round.finished_at,
                "outcome": round.outcome.as_str(),
            })).collect::<Vec<_>>(),
        })
    }
}

struct Inflight {
    handle: JoinHandle<RoundOutcome>,
    cancel: CancelFlag,
    trigger: RoundTrigger,
    started_at: DateTime<Utc>,
}

fn next_seed(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *seed
}

/// 常驻调度主循环。
///
/// `run_round` 由调用方注入（真实实现走 [`crate::round::run_round`]，测试用假轮次），
/// `shutdown` 完成即视为收到退出信号。
pub async fn serve<F, Fut, S>(opts: ServeOptions, shutdown: S, mut run_round: F) -> ServeStats
where
    F: FnMut(CancelFlag) -> Fut,
    Fut: std::future::Future<Output = RoundOutcome> + Send + 'static,
    S: std::future::Future<Output = ()>,
{
    let mut seed: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0x5EED);

    let mut scheduler = Scheduler::new(
        opts.scheduler.clone(),
        Utc::now(),
        jitter_sample(opts.scheduler.jitter_max, next_seed(&mut seed)),
    );
    if opts.run_immediately {
        // 容器刚起来就把当天这轮补上；后续仍按定点走。
        scheduler.trigger_now(Utc::now());
    }
    redact::info(format!(
        "常驻调度启动：下一轮定点 {}",
        crate::serve::rfc3339(scheduler.next_scheduled())
    ));

    let mut stats = ServeStats::default();
    let mut inflight: Option<Inflight> = None;
    tokio::pin!(shutdown);

    loop {
        let now = Utc::now();
        let wake = scheduler.next_wake();
        let sleep_for = (wake - now)
            .to_std()
            .unwrap_or(Duration::ZERO)
            .max(Duration::from_millis(1));

        enum Event {
            Timer,
            RoundDone(Result<RoundOutcome, tokio::task::JoinError>),
            Shutdown,
        }

        let event = match inflight.as_mut() {
            Some(flight) => {
                let handle = &mut flight.handle;
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => Event::Timer,
                    result = handle => Event::RoundDone(result),
                    _ = &mut shutdown => Event::Shutdown,
                }
            }
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => Event::Timer,
                    _ = &mut shutdown => Event::Shutdown,
                }
            }
        };

        match event {
            Event::Timer => {
                let jitter = jitter_sample(opts.scheduler.jitter_max, next_seed(&mut seed));
                match scheduler.on_wake(Utc::now(), jitter, inflight.is_some()) {
                    WakeOutcome::Skipped { next } => {
                        stats.skipped += 1;
                        redact::warn(format!(
                            "上一轮仍未结束，跳过本轮触发（下一轮定点 {}）",
                            rfc3339(next)
                        ));
                    }
                    WakeOutcome::Start(trigger) => {
                        let cancel = CancelFlag::new();
                        let future = run_round(cancel.clone());
                        let started_at = Utc::now();
                        redact::info(format!("开始一轮（{}）", trigger.as_str()));
                        inflight = Some(Inflight {
                            handle: tokio::spawn(future),
                            cancel,
                            trigger,
                            started_at,
                        });
                        stats.started += 1;
                    }
                }
            }
            Event::RoundDone(result) => {
                let flight = inflight.take().expect("有在飞任务才会有完成事件");
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        redact::error(format!("轮次任务异常结束: {err}"));
                        RoundOutcome::Aborted
                    }
                };
                let finished_at = Utc::now();
                record_round(
                    &mut stats,
                    &mut scheduler,
                    flight.trigger,
                    flight.started_at,
                    outcome,
                    finished_at,
                );
            }
            Event::Shutdown => {
                redact::warn("收到退出信号，停止调度");
                if let Some(flight) = inflight.take() {
                    let trigger = flight.trigger;
                    let started_at = flight.started_at;
                    flight.cancel.cancel();
                    match tokio::time::timeout(opts.shutdown_grace, flight.handle).await {
                        Ok(Ok(outcome)) => {
                            record_round(
                                &mut stats,
                                &mut scheduler,
                                trigger,
                                started_at,
                                outcome,
                                Utc::now(),
                            );
                            if outcome.is_success() {
                                redact::info("在飞轮次已收尾，退出");
                            } else {
                                redact::warn("在飞轮次已中止（数据平面零帧），退出");
                            }
                        }
                        Ok(Err(err)) => {
                            redact::error(format!("轮次任务异常结束: {err}"));
                            stats.failed += 1;
                        }
                        Err(_) => {
                            stats.failed += 1;
                            redact::warn(format!(
                                "宽限期 {:?} 内未收尾；spool 保留，下次启动会续跑",
                                opts.shutdown_grace
                            ));
                        }
                    }
                }
                break;
            }
        }
    }

    redact::info(format!(
        "调度结束：started={} skipped={} succeeded={} failed={}",
        stats.started, stats.skipped, stats.succeeded, stats.failed
    ));
    stats
}

fn record_round(
    stats: &mut ServeStats,
    scheduler: &mut Scheduler,
    trigger: RoundTrigger,
    started_at: DateTime<Utc>,
    outcome: RoundOutcome,
    finished_at: DateTime<Utc>,
) {
    // 记一笔，再把结果喂回调度器（决定要不要退避重试）。
    scheduler.on_round_finished(
        finished_at,
        if outcome.is_success() {
            RoundResult::Success
        } else {
            RoundResult::Failure
        },
    );

    if outcome.is_success() {
        stats.succeeded += 1;
    } else {
        stats.failed += 1;
    }
    stats.rounds.push(RoundLog {
        trigger,
        started_at: rfc3339(started_at),
        finished_at: rfc3339(finished_at),
        outcome,
    });

    let retry = scheduler
        .retry_at()
        .map(|at| format!("，退避后 {} 重试", rfc3339(at)))
        .unwrap_or_default();
    redact::info(format!(
        "一轮结束：{}（连续失败 {}）{}",
        outcome.as_str(),
        scheduler.consecutive_failures(),
        retry
    ));
}

/// 报告与日志统一用 UTC RFC3339。
pub fn rfc3339(at: DateTime<Utc>) -> String {
    harvest_core::time::rfc3339_utc(at)
}

/// SIGTERM / SIGINT。容器里通常发 SIGTERM。
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                redact::warn(format!("无法监听 SIGTERM（{err}），退回 Ctrl-C"));
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let mut interrupt = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(err) => {
                redact::warn(format!("无法监听 SIGINT（{err}），退回 Ctrl-C"));
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = terminate.recv() => redact::warn("收到 SIGTERM"),
            _ = interrupt.recv() => redact::warn("收到 SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
