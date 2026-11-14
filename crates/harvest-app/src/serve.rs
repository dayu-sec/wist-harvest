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
    RoundResult, RoundTrigger, ScheduleMode, Scheduler, SchedulerConfig, WakeOutcome, jitter_sample,
};
use tokio::task::JoinHandle;
use wist_api::harvest_work::HarvestCadence;

pub use crate::round::RoundOutcome;

/// 连续这么多次轮询失败就重建连接（重查证书有效期 / 重新接入：都是后台动作，不该等人）。
const RECONNECT_AFTER: u32 = 6;

/// 就算一切正常，也隔这么久重建一次连接。
///
/// 为什么需要：**证书续期只发生在 `connect()` 里**（建连前读 `notAfter`）。进程能长跑几十天，
/// 若启动时那次续期恰好失败，不主动重连就再也没机会重试 → 证书到期后只能人工删 identity。
/// 6 小时一次在 7 天续期窗口面前有充裕余量，代价是每天多 4 次 TLS 握手。
const RECONNECT_EVERY: Duration = Duration::from_secs(6 * 3600);

/// 网关工作授权轮询计划（`[gateway]` 有 endpoint 时才建）。
///
/// 手里拿的是**连接器**而不是连接：证书/接入券/网关就绪都可能晚于容器启动，
/// 失败就退避重试，不要求人工重启。
#[derive(Clone)]
pub struct WorkPlan {
    pub gateway: crate::gateway::GatewayConnector,
}

/// 网关下发的一份「本机该怎么采」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCadence {
    Daily { at: chrono::NaiveTime },
    Interval { every: Duration },
    OnDemand,
}

/// 网关下发的一份「本机该怎么采」。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteWork {
    /// 授权生效（standing active）。
    pub active: bool,
    /// `None` = 没有授权 / 已撤销 / 按需模式（不排定时）。
    pub cadence: Option<RemoteCadence>,
    pub work_id: Option<String>,
    pub plan_version: i64,
    /// 未了结的一次性工作（页面「立即采集」）；每次变化全量下发。
    pub one_shot: Vec<RemoteOneShot>,
}

/// 一件待执行的一次性工作（已从契约报文中拍平）。
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteOneShot {
    pub work_id: String,
    pub action: String,
    /// 忽略可续跑 spool（`inventory.rescan` 一律为真）。
    pub fresh: bool,
    /// 绝对截止；解析不出来就不判超期（宁可跑，也别因报文格式丢掉一件活）。
    pub deadline: Option<DateTime<Utc>>,
}

impl RemoteOneShot {
    /// 契约 → 本地形态。**动作语义在这里收口**：`rescan` 就是 fresh，不看调用方写什么。
    pub fn from_contract(work: &wist_api::harvest_work::OneShotHarvestWork) -> Self {
        Self {
            work_id: work.work_id.clone(),
            action: work.action.clone(),
            fresh: work.action == "inventory.rescan" || work.spec.fresh.unwrap_or(false),
            deadline: DateTime::parse_from_rfc3339(&work.deadline_at)
                .ok()
                .map(|at| at.with_timezone(&Utc)),
        }
    }

    pub fn is_overdue(&self, now: DateTime<Utc>) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }
}

pub struct ServeOptions {
    /// 本地采集开关：`false` 时保持常驻但不调度（**硬关**，网关授权也不采）。
    pub enabled: bool,
    /// 网关工作授权轮询；`None` = 不轮询（离线模式：用本机 `--run-at/--interval`）。
    pub work: Option<WorkPlan>,
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
            enabled: true,
            work: None,
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
    /// 这一轮是在执行哪件一次性工作（定时轮次为 `None`）：结束后要上报结果。
    one_shot: Option<String>,
}

/// 交给 `run_round` 的一轮参数。
///
/// 为什么不再直接传 `CancelFlag`：一次性工作可能要求 `fresh`（忽略可续跑 spool），
/// 而定时轮次的这个值来自 `ServeOptions.resume` —— 两侧不能共用一个全局开关。
pub struct RoundRequest {
    pub cancel: CancelFlag,
    /// `true` = 续跑未完成的 spool；`false` = 强制开新 `snap_id`。
    pub resume: bool,
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
    F: FnMut(RoundRequest) -> Fut,
    Fut: std::future::Future<Output = RoundOutcome> + Send + 'static,
    S: std::future::Future<Output = ()>,
{
    // 关闭态：**不跑任何一轮**，但进程不退出、网关照旧轮询（心跳与证书续期要继续）：
    // 容器留着，管理面改回 true 后重启即恢复，不用重新部署；本地硬关 37 天后证书也不会裸过期。
    if !opts.enabled {
        redact::info("采集已关闭（service.enabled=false）：保持常驻、不调度");
    }

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

    // 远端授权：本地 `enabled` 是硬关；配了 `[gateway]` 时**先按未授权处理**，
    // 等第一拍 poll 答复再放行（否则容器刚起来那一轮可能抢在第一次轮询之前跑掉）。
    let work_configured = opts.work.is_some();
    let effective = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
        opts.enabled && !work_configured,
    ));
    // 是否按定时调度（`on_demand` / 未授权时为 false）。
    let mut scheduled = !work_configured;
    let (work_tx, mut work_rx) = tokio::sync::mpsc::unbounded_channel::<RemoteWork>();
    // 结果上报走同一条连接：主循环把「开始/结束」交给轮询任务去发，失败在下一拍重试。
    let (report_tx, mut report_rx) = tokio::sync::mpsc::unbounded_channel::<ReportRequest>();
    // 保留一个发送端：否则没有轮询任务时 `recv()` 立刻返回 None，主循环会空转。
    let _work_keepalive = work_tx.clone();
    if let Some(plan) = opts.work.clone() {
        let effective = std::sync::Arc::clone(&effective);
        let local_enabled = opts.enabled;
        let tx = work_tx.clone();
        tokio::spawn(async move {
            let mut last_seq = 0_i64;
            let mut last_active: Option<bool> = None;
            let mut applied: Option<(String, i64)> = None;
            // 上一份成功的 cadence：cadence 解析失败时沿用它（不能因为网关报文坏了就停采）。
            let mut cadence: Option<RemoteCadence> = None;
            let mut failing = false;
            // 连接本身也是可重试的：没证书 / 券没落盘 / 网关没起都不该要求重启容器。
            let mut known: Option<crate::gateway::GatewayClient> = None;
            let mut connect_failing = false;
            let mut poll_failures = 0_u32;
            let mut connected_at: Option<std::time::Instant> = None;
            let mut pending_reports: std::collections::VecDeque<ReportRequest> =
                std::collections::VecDeque::new();
            loop {
                // 定期重连：让「续期 / 重新读 CA / 重新验名」不依赖启动时机（见 RECONNECT_EVERY）。
                if connected_at.is_some_and(|at| at.elapsed() >= RECONNECT_EVERY) {
                    known = None;
                }
                if known.is_none() {
                    match plan.gateway.connect().await {
                        Ok(gateway) => {
                            if connect_failing {
                                redact::info(format!(
                                    "已接入网关：{}（{}）",
                                    plan.gateway.worker_id(),
                                    gateway.source_code()
                                ));
                            }
                            connect_failing = false;
                            poll_failures = 0;
                            failing = false;
                            connected_at = Some(std::time::Instant::now());
                            known = Some(gateway);
                        }
                        Err(err) => {
                            poll_failures += 1;
                            if !connect_failing {
                                redact::warn(format!(
                                    "{err}（{}s 后重试；进程常驻，恢复后自动开始轮询）",
                                    plan.gateway.poll_interval().as_secs()
                                ));
                                connect_failing = true;
                            }
                            // fail-closed：长时间接不上就别再按上一拍的授权采（
                            // 证书过期 / 被吊销在这里同样是“读不到授权”）。
                            if poll_failures == RECONNECT_AFTER {
                                redact::warn("连续多次接不上网关：停用上一份授权（恢复自动继续）");
                            }
                            if poll_failures >= RECONNECT_AFTER {
                                effective.store(false, std::sync::atomic::Ordering::SeqCst);
                            }
                            tokio::time::sleep(plan.gateway.poll_interval()).await;
                            continue;
                        }
                    }
                }
                let gateway = known.as_ref().expect("刚连上");
                match gateway.poll(last_seq).await {
                    Ok(grant) => {
                        if failing {
                            redact::info("网关工作授权已恢复");
                            failing = false;
                        }
                        poll_failures = 0;
                        last_seq = grant.sequence;
                        let active = grant
                            .standing
                            .as_ref()
                            .map(|standing| standing.is_active())
                            .unwrap_or(false);
                        effective
                            .store(local_enabled && active, std::sync::atomic::Ordering::SeqCst);
                        if last_active != Some(active) {
                            redact::info(format!(
                                "网关授权：active={active}（work={:?} plan_version={:?}）",
                                grant.standing.as_ref().map(|standing| &standing.work_id),
                                grant
                                    .standing
                                    .as_ref()
                                    .map(|standing| standing.plan_version)
                            ));
                            last_active = Some(active);
                        }
                        // cadence 解析失败就沿用上一份（只吼一句，不上报 ack：页面能看出漂移）。
                        if let Some(standing) = grant.standing.as_ref() {
                            let key = (standing.work_id.clone(), standing.plan_version);
                            if applied.as_ref() != Some(&key) {
                                match remote_cadence(&standing.spec.cadence) {
                                    Ok(parsed) => {
                                        cadence = if active { Some(parsed) } else { None }
                                    }
                                    Err(err) => {
                                        redact::warn(format!("standing 参数不可用：{err}"));
                                    }
                                }
                                if let Err(err) =
                                    gateway.ack(&standing.work_id, standing.plan_version).await
                                {
                                    // 不记 applied：下一拍重试（页面会显示未确认/漂移）。
                                    redact::warn(format!("工作确认失败（下一拍重试）：{err}"));
                                } else {
                                    applied = Some(key);
                                }
                            }
                        } else {
                            applied = None;
                            cadence = None;
                        }
                        // 每拍都下发一份完整快照；主循环自己按「cadence 变没变 / 这件活开过没」去重
                        // （在发送侧做 diff 会让“同拍既有新 spec 又有一件一次性工作”难以表达）。
                        let update = RemoteWork {
                            active,
                            cadence,
                            work_id: grant.standing.as_ref().map(|s| s.work_id.clone()),
                            plan_version: grant
                                .standing
                                .as_ref()
                                .map(|s| s.plan_version)
                                .unwrap_or(0),
                            one_shot: grant
                                .one_shot
                                .iter()
                                .map(RemoteOneShot::from_contract)
                                .collect(),
                        };
                        if tx.send(update).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        poll_failures += 1;
                        // 只在第一次失败时说一句：网关抖一下不该把日志刷满，也不改本机行为。
                        if !failing {
                            redact::warn(format!("{err}（沿用上一拍的值）"));
                            failing = true;
                        }
                        if poll_failures >= RECONNECT_AFTER {
                            // 重建连接会重查证书（到期就续期）并重新走接入判定，
                            // 不用等人工重启。
                            redact::warn(format!(
                                "连续 {poll_failures} 次轮询失败：重建连接并重查证书有效期"
                            ));
                            // fail-closed：拿不到授权就不再按上一拍的授权继续采（
                            // 吊销/过期在这里都是“读不到授权”，不能当“仍然可采”）。
                            effective.store(false, std::sync::atomic::Ordering::SeqCst);
                            known = None;
                            connect_failing = false;
                            poll_failures = 0;
                            // 连接已丢：这一拍不再尝试上报（重连后下一拍重试）。
                            tokio::time::sleep(plan.gateway.poll_interval()).await;
                            continue;
                        }
                    }
                }

                // 结果上报：先发积压的，再在等下一拍的窗口里即时发新到的。
                while let Ok(report) = report_rx.try_recv() {
                    pending_reports.push_back(report);
                }
                flush_reports(gateway, &mut pending_reports).await;
                let wait = tokio::time::sleep(plan.gateway.poll_interval());
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        _ = &mut wait => break,
                        incoming = report_rx.recv() => {
                            let Some(report) = incoming else { break };
                            pending_reports.push_back(report);
                            flush_reports(gateway, &mut pending_reports).await;
                        }
                    }
                }
            }
        });
    }

    let mut stats = ServeStats::default();
    let mut inflight: Option<Inflight> = None;
    let mut disabled_logged = false;
    // 当前调度器是按哪份 cadence 建的：每拍都下发快照，但**只有变了才重建**
    // （每拍重建会把下一轮定点一直往后推）。
    let mut applied_cadence: Option<RemoteCadence> = None;
    // 待执行的一次性工作（保持下发顺序）与「开过的」集合（同一 work_id 只跑一次：
    // 工作行在客户端上报结果之前仍会出现在快照里）。
    let mut pending_one_shot: std::collections::VecDeque<RemoteOneShot> =
        std::collections::VecDeque::new();
    let mut started_one_shot: std::collections::HashSet<String> = std::collections::HashSet::new();
    tokio::pin!(shutdown);

    loop {
        // 一次性工作优先：只要没在跑，就立刻开队首那件（不比定点、不看 on_demand）。
        if inflight.is_none() && !pending_one_shot.is_empty() {
            let now = Utc::now();
            while let Some(task) = pending_one_shot.pop_front() {
                if !opts.enabled {
                    // 本地硬关 = 急停开关：手动触发也越不过它。必须上报失败，
                    // 否则页面会一直停在「已下发」，运维会以为是采集器卡住了。
                    redact::warn(format!(
                        "本地硬关（service.enabled=false）：拒绝执行一次性工作 {}",
                        task.work_id
                    ));
                    stats.skipped += 1;
                    let _ = report_tx.send(ReportRequest::blocked(&task.work_id));
                    continue;
                }
                if task.is_overdue(now) {
                    redact::warn(format!(
                        "一次性工作 {} 已过 deadline：不再执行，直接上报失败",
                        task.work_id
                    ));
                    stats.skipped += 1;
                    let _ = report_tx.send(ReportRequest::overdue(&task.work_id));
                    continue;
                }
                let cancel = CancelFlag::new();
                let request = RoundRequest {
                    cancel: cancel.clone(),
                    resume: !task.fresh,
                };
                let future = run_round(request);
                redact::info(format!(
                    "开始一次性工作：{}（{}{}）",
                    task.work_id,
                    task.action,
                    if task.fresh { "，强制新快照" } else { "" }
                ));
                // 先记“开过了”：下一拍的快照里它仍未了结（结果还没上报），
                // 不记就会在它跑完之后再被排一次。
                started_one_shot.insert(task.work_id.clone());
                // 先报 running：页面不用等到跑完才知道“它开始动了”。
                let _ = report_tx.send(ReportRequest::running(&task.work_id));
                inflight = Some(Inflight {
                    handle: tokio::spawn(future),
                    cancel,
                    trigger: RoundTrigger::Manual,
                    started_at: Utc::now(),
                    one_shot: Some(task.work_id),
                });
                stats.started += 1;
                break;
            }
        }

        let now = Utc::now();
        let wake = scheduler.next_wake();
        let sleep_for = (wake - now)
            .to_std()
            .unwrap_or(Duration::ZERO)
            .max(Duration::from_millis(1));

        enum Event {
            Timer,
            Work(RemoteWork),
            RoundDone(Result<RoundOutcome, tokio::task::JoinError>),
            Shutdown,
        }

        let event = match inflight.as_mut() {
            Some(flight) => {
                let handle = &mut flight.handle;
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => Event::Timer,
                    Some(update) = work_rx.recv() => Event::Work(update),
                    result = handle => Event::RoundDone(result),
                    _ = &mut shutdown => Event::Shutdown,
                }
            }
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => Event::Timer,
                    Some(update) = work_rx.recv() => Event::Work(update),
                    _ = &mut shutdown => Event::Shutdown,
                }
            }
        };

        match event {
            Event::Work(update) => {
                // ① 一次性工作：全量替换待办（网关只下发未了结的），已开过的过滤掉。
                pending_one_shot = update
                    .one_shot
                    .into_iter()
                    .filter(|task| !started_one_shot.contains(&task.work_id))
                    .collect();
                // ② cadence：只有变了才重建调度器。
                if update.cadence != applied_cadence {
                    applied_cadence = update.cadence;
                    match update.cadence {
                        Some(cadence) => match scheduler_config(cadence) {
                            Some(config) => {
                                let jitter = jitter_sample(config.jitter_max, next_seed(&mut seed));
                                scheduler = Scheduler::new(config, Utc::now(), jitter);
                                scheduled = true;
                                disabled_logged = false;
                                redact::info(format!(
                                    "应用网关授权：{} plan_version={}（{}）",
                                    update.work_id.as_deref().unwrap_or("-"),
                                    update.plan_version,
                                    cadence_label(cadence)
                                ));
                            }
                            None => {
                                scheduled = false;
                                redact::info("网关授权为 on_demand：不排定时，只响应一次性工作");
                            }
                        },
                        None => {
                            scheduled = false;
                            redact::info(format!(
                                "网关未授权采集（plan_version={}）：停止定时调度",
                                update.plan_version
                            ));
                        }
                    }
                }
            }
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
                        if !effective.load(std::sync::atomic::Ordering::SeqCst) || !scheduled {
                            // 未授权 / 已暂停 / on_demand：照常推进调度，但不真跑。只喊一次。
                            stats.skipped += 1;
                            if !disabled_logged {
                                redact::warn(
                                    "当前没有可执行的采集授权（本地硬关 / 未授权 / 已暂停 / on_demand）：跳过定时触发",
                                );
                                disabled_logged = true;
                            }
                            continue;
                        }
                        disabled_logged = false;
                        let cancel = CancelFlag::new();
                        let request = RoundRequest {
                            cancel: cancel.clone(),
                            resume: opts.resume,
                        };
                        let future = run_round(request);
                        let started_at = Utc::now();
                        redact::info(format!("开始一轮（{}）", trigger.as_str()));
                        inflight = Some(Inflight {
                            handle: tokio::spawn(future),
                            cancel,
                            trigger,
                            started_at,
                            one_shot: None,
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
                if let Some(work_id) = flight.one_shot {
                    // 一次性工作：结果就是回执（不再单独 ack）。
                    let _ = report_tx.send(ReportRequest::finished(&work_id, outcome));
                }
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
                            if let Some(work_id) = &flight.one_shot {
                                // 退出前也要把结果交出去（轮询任务可能已经不在了，那就算了）。
                                let _ = report_tx.send(ReportRequest::finished(work_id, outcome));
                            }
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

/// 主循环 → 轮询任务的一条结果上报。
///
/// 为什么绕一圈而不是主循环直接发：**一条连接只能有一个主**。让持有连接的轮询任务去发，
/// 失败重试、重连、证书续期全都不用在这里重复一遍，也不会与 poll 抢同一个客户端状态。
#[derive(Debug, Clone)]
struct ReportRequest {
    work_id: String,
    status: &'static str,
    detail: String,
}

impl ReportRequest {
    fn running(work_id: &str) -> Self {
        Self {
            work_id: work_id.to_string(),
            status: "running",
            detail: "已开始执行".to_string(),
        }
    }

    fn finished(work_id: &str, outcome: RoundOutcome) -> Self {
        Self {
            work_id: work_id.to_string(),
            status: if outcome.is_success() {
                "succeeded"
            } else {
                "failed"
            },
            detail: concise_detail(outcome).to_string(),
        }
    }

    fn overdue(work_id: &str) -> Self {
        Self {
            work_id: work_id.to_string(),
            status: "failed",
            detail: "排队超期：已过 deadline_at".to_string(),
        }
    }

    fn blocked(work_id: &str) -> Self {
        Self {
            work_id: work_id.to_string(),
            status: "failed",
            detail: "本地硬关（service.enabled=false）：未执行".to_string(),
        }
    }
}

/// 给页面看的一句话（别把内部枚举名丢给运维）。
fn concise_detail(outcome: RoundOutcome) -> &'static str {
    match outcome {
        RoundOutcome::Done => "",
        RoundOutcome::Aborted => "本轮中止（数据平面零帧），spool 保留待续跑",
        RoundOutcome::StartFailed => "启动失败（配置 / 凭据 / 初始化）",
    }
}

/// 把积压的结果上报发出去；失败就留在队首，下一拍重试。
async fn flush_reports(
    gateway: &crate::gateway::GatewayClient,
    pending: &mut std::collections::VecDeque<ReportRequest>,
) -> bool {
    while let Some(report) = pending.front() {
        match gateway
            .report(&report.work_id, report.status, &report.detail)
            .await
        {
            Ok(accepted) => {
                if accepted.status == "unknown" {
                    // 网关不认识这件活（被删 / 换了网关）：再发也是徒劳，丢弃但要说出来。
                    redact::warn(format!(
                        "结果上报被网关拒绝（unknown，丢弃）：{}",
                        report.work_id
                    ));
                }
                pending.pop_front();
            }
            Err(err) => {
                redact::warn(format!("结果上报失败（下一拍重试）：{err}"));
                return false;
            }
        }
    }
    true
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
    //
    // 一次性工作（`Manual`）**不反哺调度器**：它是一次临时动作，失败不应该把站点当天
    // 那轮定时采集往后拖，成功也不应该让调度器以为“这轮已经跑过了”。
    if trigger != RoundTrigger::Manual {
        scheduler.on_round_finished(
            finished_at,
            if outcome.is_success() {
                RoundResult::Success
            } else {
                RoundResult::Failure
            },
        );
    }

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
/// 网关下发的 `cadence` → 本地调度模式。
///
/// 参数坏时不猜：返回错误，采集器不 ack（页面显示漂移），而不是静默用旧节奏。
fn remote_cadence(cadence: &HarvestCadence) -> Result<RemoteCadence, String> {
    match cadence.mode.as_str() {
        "on_demand" => Ok(RemoteCadence::OnDemand),
        "daily" => {
            let at = cadence
                .at
                .as_deref()
                .ok_or_else(|| "cadence.mode=daily 缺 at".to_string())?;
            let at = chrono::NaiveTime::parse_from_str(at.trim(), "%H:%M")
                .map_err(|err| format!("cadence.at 不可用：{err}"))?;
            Ok(RemoteCadence::Daily { at })
        }
        "interval" => {
            let seconds = cadence
                .seconds
                .ok_or_else(|| "cadence.mode=interval 缺 seconds".to_string())?;
            if seconds == 0 {
                return Err("cadence.seconds 不能为 0".to_string());
            }
            Ok(RemoteCadence::Interval {
                every: Duration::from_secs(seconds),
            })
        }
        other => Err(format!("未知 cadence.mode {other}")),
    }
}

fn scheduler_config(cadence: RemoteCadence) -> Option<SchedulerConfig> {
    match cadence {
        RemoteCadence::OnDemand => None,
        RemoteCadence::Daily { at } => Some(SchedulerConfig {
            mode: ScheduleMode::Daily { at },
            ..Default::default()
        }),
        RemoteCadence::Interval { every } => Some(SchedulerConfig {
            mode: ScheduleMode::Interval { every },
            ..Default::default()
        }),
    }
}

fn cadence_label(cadence: RemoteCadence) -> String {
    match cadence {
        RemoteCadence::Daily { at } => format!("daily {}", at.format("%H:%M")),
        RemoteCadence::Interval { every } => format!("interval {}s", every.as_secs()),
        RemoteCadence::OnDemand => "on_demand".to_string(),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_interval() -> SchedulerConfig {
        SchedulerConfig {
            mode: ScheduleMode::Interval {
                every: Duration::from_millis(10),
            },
            jitter_max: Duration::ZERO,
            ..Default::default()
        }
    }

    fn unreachable_work(state_dir: &std::path::Path) -> WorkPlan {
        // 有端点但没证书、没券：`connect()` 立刻失败（不发任何请求）。
        WorkPlan {
            gateway: crate::gateway::GatewayConnector::new(
                &harvest_core::config::GatewaySettings {
                    endpoint: "https://gw.example.com:3000".to_string(),
                    source_code: "demo".to_string(),
                    ..Default::default()
                },
                state_dir,
                None,
            ),
        }
    }

    /// 网关连不上时的契约：**进程常驻、一轮不跑、能正常退出**。
    /// 这是「不需要人工重启」的底线 —— 一旦这里变成 panic 或退出，容器就会无限重启。
    #[tokio::test]
    async fn unreachable_gateway_stays_resident_and_runs_nothing() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let rounds = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rounds_round = std::sync::Arc::clone(&rounds);

        let stats = serve(
            ServeOptions {
                enabled: true,
                work: Some(unreachable_work(state_dir.path())),
                scheduler: fast_interval(),
                resume: true,
                shutdown_grace: Duration::from_millis(50),
                run_immediately: true,
            },
            async { tokio::time::sleep(Duration::from_millis(150)).await },
            move |_cancel| {
                let rounds = std::sync::Arc::clone(&rounds_round);
                async move {
                    rounds.store(true, std::sync::atomic::Ordering::SeqCst);
                    RoundOutcome::Done
                }
            },
        )
        .await;

        assert!(
            !rounds.load(std::sync::atomic::Ordering::SeqCst),
            "没有授权就不能采（fail-closed）"
        );
        assert_eq!(stats.started, 0);
        assert_eq!(stats.succeeded, 0);
    }

    /// 本地硬关 + 配了网关：照样不跑（本地开关比远端授权更强），但进程常驻。
    #[tokio::test]
    async fn locally_disabled_beats_a_remote_grant() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let rounds = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rounds_round = std::sync::Arc::clone(&rounds);

        let stats = serve(
            ServeOptions {
                enabled: false,
                work: Some(unreachable_work(state_dir.path())),
                scheduler: fast_interval(),
                resume: true,
                shutdown_grace: Duration::from_millis(50),
                run_immediately: true,
            },
            async { tokio::time::sleep(Duration::from_millis(120)).await },
            move |_cancel| {
                let rounds = std::sync::Arc::clone(&rounds_round);
                async move {
                    rounds.store(true, std::sync::atomic::Ordering::SeqCst);
                    RoundOutcome::Done
                }
            },
        )
        .await;

        assert!(!rounds.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(stats.started, 0);
    }

    #[test]
    fn maps_gateway_cadence_to_local_schedule() {
        assert_eq!(
            remote_cadence(&HarvestCadence::daily("00:30", 300)).expect("daily"),
            RemoteCadence::Daily {
                at: chrono::NaiveTime::from_hms_opt(0, 30, 0).expect("valid time")
            }
        );
        assert_eq!(
            remote_cadence(&HarvestCadence::interval(3600)).expect("interval"),
            RemoteCadence::Interval {
                every: Duration::from_secs(3600)
            }
        );
        assert_eq!(
            remote_cadence(&HarvestCadence::on_demand()).expect("on demand"),
            RemoteCadence::OnDemand
        );
        assert!(scheduler_config(RemoteCadence::OnDemand).is_none());
    }

    #[test]
    fn rejects_bad_cadence_instead_of_guessing() {
        let mut missing_at = HarvestCadence::daily("00:30", 0);
        missing_at.at = None;
        assert!(remote_cadence(&missing_at).is_err());

        let mut unknown = HarvestCadence::on_demand();
        unknown.mode = "weekly".to_string();
        assert!(remote_cadence(&unknown).is_err());

        let mut zero = HarvestCadence::interval(0);
        zero.seconds = Some(0);
        assert!(remote_cadence(&zero).is_err());
    }

    fn one_shot_contract(
        action: &str,
        fresh: Option<bool>,
        deadline_at: &str,
    ) -> wist_api::harvest_work::OneShotHarvestWork {
        wist_api::harvest_work::OneShotHarvestWork {
            work_id: "hos:harvest:demo:inventory.run:20261114T083000Z:abcd1234".to_string(),
            action: action.to_string(),
            spec: wist_api::harvest_work::OneShotHarvestSpec {
                assets: None,
                regions: None,
                fresh,
            },
            scheduled_at: "2026-11-14T08:30:00Z".to_string(),
            deadline_at: deadline_at.to_string(),
            timeout_seconds: 3600,
            interruptible: true,
            status: "dispatched".to_string(),
            attempt: 0,
            issued_by: "admin".to_string(),
            issued_at: "2026-11-14T08:30:00Z".to_string(),
        }
    }

    /// 动作语义在客户端收口：`inventory.rescan` 永远 fresh，不管网关报文里写什么。
    #[test]
    fn one_shot_action_decides_the_snapshot_mode() {
        let run = RemoteOneShot::from_contract(&one_shot_contract(
            "inventory.run",
            None,
            "2026-11-14T09:30:00Z",
        ));
        assert!(!run.fresh);

        let informed = RemoteOneShot::from_contract(&one_shot_contract(
            "inventory.run",
            Some(true),
            "2026-11-14T09:30:00Z",
        ));
        assert!(informed.fresh, "spec.fresh=true 也要生效");

        let rescan = RemoteOneShot::from_contract(&one_shot_contract(
            "inventory.rescan",
            Some(false),
            "2026-11-14T09:30:00Z",
        ));
        assert!(rescan.fresh, "重扫就是 fresh，调用方写 false 也不听");
    }

    /// 截止时间是**绝对**的：过了就过了，不能因为排队就顺延。
    /// 解析不出来时宁可不判超期（网关报文格式问题不该让一件活白丢）。
    #[test]
    fn one_shot_deadline_is_absolute_and_unparsable_deadlines_do_not_block() {
        let task = RemoteOneShot::from_contract(&one_shot_contract(
            "inventory.run",
            None,
            "2026-11-14T09:30:00Z",
        ));
        let deadline = task.deadline.expect("deadline");
        assert!(!task.is_overdue(deadline - chrono::Duration::seconds(1)));
        assert!(task.is_overdue(deadline), "到点就算超期");
        assert!(task.is_overdue(deadline + chrono::Duration::seconds(1)));

        let broken = RemoteOneShot::from_contract(&one_shot_contract(
            "inventory.run",
            None,
            "not-a-timestamp",
        ));
        assert!(broken.deadline.is_none());
        assert!(!broken.is_overdue(Utc::now()));
    }

    /// 上报给页面的文案：成功不说废话，失败说清楚“数据面有没有发出去”。
    #[test]
    fn result_reports_map_outcomes_to_honest_statuses() {
        let ok = ReportRequest::finished("w", RoundOutcome::Done);
        assert_eq!(ok.status, "succeeded");
        assert_eq!(ok.detail, "");

        let aborted = ReportRequest::finished("w", RoundOutcome::Aborted);
        assert_eq!(aborted.status, "failed");
        assert!(aborted.detail.contains("零帧"), "{}", aborted.detail);

        let start_failed = ReportRequest::finished("w", RoundOutcome::StartFailed);
        assert_eq!(start_failed.status, "failed");
        assert!(start_failed.detail.contains("启动失败"));

        let overdue = ReportRequest::overdue("w");
        assert_eq!(overdue.status, "failed");
        assert!(overdue.detail.contains("deadline_at"));
    }
}
