//! 编排：`Planned → Fetching → Spooled → Verified → Published → Done`。
//!
//! 唯一能决定"发不发"的地方就在这里，所以"`Aborted` 零帧"是本模块的硬不变量：
//! [`Sink::publish_snapshot`] 只在 `Verified` 之后调用一次。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::assemble;
use crate::config::ServiceConfig;
use crate::error::{ErrorClass, HarvestError};
use crate::lease;
use crate::model::{AssetType, Coverage};
use crate::report::{ErrorEntry, ItemReport, RunReport, ScopeReport, TypeReport};
use crate::scope::Scope;
use crate::source::{CompleteSnapshot, Sink, Source, WorkItem};
use crate::spool::Spool;
use crate::state::{RunState, StateMachine};

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 是否续跑同范围未完成的 spool。默认开启。
    pub resume: bool,
    /// 指定 snap_id（重放同一份快照用）。默认按范围 + 当前时间生成。
    pub snap_id: Option<String>,
    /// 退出信号。抓到一半收到信号时，本轮进 `Aborted`（数据平面零帧）。
    pub cancel: CancelFlag,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            resume: true,
            snap_id: None,
            cancel: CancelFlag::default(),
        }
    }
}

impl RunOptions {
    pub fn fresh() -> Self {
        Self {
            resume: false,
            ..Default::default()
        }
    }
}

/// 跨任务传递的取消标志。
///
/// 只在**工作项之间**检查：一个工作项写到一半被打断会留下半份 spool，
/// 而整项重抓才安全。
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Option<Arc<AtomicBool>>);

impl CancelFlag {
    pub fn new() -> Self {
        Self(Some(Arc::new(AtomicBool::new(false))))
    }

    pub fn cancel(&self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0
            .as_ref()
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
    }
}

#[derive(Debug)]
pub struct RunOutcome {
    pub state: RunState,
    pub snap_id: String,
    pub report: RunReport,
    pub report_path: Option<PathBuf>,
    pub published: bool,
}

impl RunOutcome {
    pub fn ok(&self) -> bool {
        self.state == RunState::Done
    }
}

struct RunTrace {
    machine: StateMachine,
    spool: Option<Spool>,
    published: Option<crate::source::PublishAck>,
    resumed: bool,
}

pub async fn run_once<S, K>(
    svc: &ServiceConfig,
    scope: &Scope,
    source: &S,
    sink: &K,
    opts: &RunOptions,
) -> RunOutcome
where
    S: Source + ?Sized,
    K: Sink + ?Sized,
{
    let started = crate::time::utc_now();
    let mut trace = RunTrace {
        machine: StateMachine::new(),
        spool: None,
        published: None,
        resumed: false,
    };

    let mut abort: Option<HarvestError> = None;
    if let Err(err) = execute(svc, scope, source, sink, opts, &mut trace).await {
        // 进入 Aborted：保留 spool + 原因，一帧不发。
        if let Some(spool) = trace.spool.as_mut() {
            spool.set_error(err.class, &err.message);
            spool.set_state(RunState::Aborted);
            if let Err(save_err) = spool.save() {
                crate::redact::error(format!("写入 Aborted manifest 失败: {save_err}"));
            }
        }
        if trace.machine.advance(RunState::Aborted).is_err() {
            crate::redact::warn("状态机无法进入 Aborted（已发布后失败？）");
        }
        crate::redact::error(format!("运行中止: {err}"));
        abort = Some(err);
    }

    let finished = crate::time::utc_now();
    let report = build_report(scope, &trace, started, finished, abort.as_ref());
    // 还没建 spool 就失败时没有 snap_id，用启动时间当文件名：不能让一次次的
    // 启动失败覆盖同一份 `-.json`，否则重启风暴会把排障记录冲干净。
    let report_name = match trace.spool.as_ref() {
        Some(_) => None,
        None => Some(format!("start-{}", crate::time::snap_timestamp(started))),
    };
    let report_path = match match &report_name {
        Some(name) => report.write_as(&svc.runs_dir(), name),
        None => report.write(&svc.runs_dir()),
    } {
        Ok(path) => Some(path),
        Err(err) => {
            crate::redact::error(format!("写入运行报告失败: {err}"));
            None
        }
    };

    let snap_id = trace
        .spool
        .as_ref()
        .map(|s| s.snap_id().to_string())
        .unwrap_or_else(|| "-".to_string());

    RunOutcome {
        state: trace.machine.state(),
        snap_id,
        report,
        report_path,
        published: trace.published.is_some(),
    }
}

async fn execute<S, K>(
    svc: &ServiceConfig,
    scope: &Scope,
    source: &S,
    sink: &K,
    opts: &RunOptions,
    trace: &mut RunTrace,
) -> Result<(), HarvestError>
where
    S: Source + ?Sized,
    K: Sink + ?Sized,
{
    let lease = lease::acquire(&svc.lease_dir(), scope)?;
    crate::redact::info(format!("租约已持有: {}", lease.path().display()));

    let plan = source.plan();
    if plan.is_empty() {
        return Err(HarvestError::integrity(
            "采集计划为空：所有资产类型都被关掉了？",
        ));
    }

    let base_snap_id = match &opts.snap_id {
        Some(id) => id.clone(),
        None => scope.new_snap_id(crate::time::utc_now()),
    };

    let resumable = if opts.resume {
        Spool::find_resumable(&svc.service.spool_dir, scope)?
    } else {
        None
    };

    let mut spool = match resumable {
        Some(dir) => {
            let mut existing = Spool::open_existing(&dir)?;
            // 两种“不能再续”的情况：计划变了（永远不可能变合法），或者放得太久了
            // （永久性的局部失败会把一份旧 spool 无限期地续下去，永不产出新快照）。
            let stale_reason = existing
                .plan_mismatch(&plan)
                .map(|(expected, actual)| {
                    format!(
                        "采集计划与 spool 不一致，放弃续跑（计划 {:?} vs spool {:?}）",
                        expected, actual
                    )
                })
                .or_else(|| {
                    existing
                        .resume_expired(svc.service.max_resume_age_ms)
                        .then(|| {
                            format!(
                                "spool 已存在 {}ms，超过可续跑时限 {}ms",
                                existing.age_ms(),
                                svc.service.max_resume_age_ms
                            )
                        })
                });

            match stale_reason {
                // 计划一致且未过期：正常续跑。
                None => {
                    crate::redact::info(format!("续跑既有 spool: {}", dir.display()));
                    trace.resumed = true;
                    existing
                }
                // 否则放弃它（保留目录供诊断），本轮开新 snap_id 重新采。
                // 不这么做的话，采集器会每轮撞在同一份旧 spool 上、永不发布。
                Some(reason) => {
                    crate::redact::warn(&reason);
                    existing.abandon(&reason)?;
                    let snap_id =
                        Spool::next_available_snap_id(&svc.service.spool_dir, scope, &base_snap_id);
                    Spool::create(&svc.service.spool_dir, scope, &snap_id, &plan)?
                }
            }
        }
        None => {
            let snap_id =
                Spool::next_available_snap_id(&svc.service.spool_dir, scope, &base_snap_id);
            Spool::create(&svc.service.spool_dir, scope, &snap_id, &plan)?
        }
    };
    crate::redact::info(format!(
        "snap_id={} 范围={} 工作项={}",
        spool.snap_id(),
        scope.producer(),
        plan.len()
    ));

    let result = fill_and_publish(
        &RoundContext {
            svc,
            scope,
            source,
            sink,
        },
        &plan,
        &mut spool,
        trace,
        &opts.cancel,
    )
    .await;
    trace.spool = Some(spool);
    drop(lease);
    result
}

/// 续跑必须保证计划没变：配置改了地域/类型就是另一个范围，不能混进同一帧。
/// 一轮需要的不可变输入，打包成结构，避免参数列表膨胀。
struct RoundContext<'a, S: ?Sized, K: ?Sized> {
    svc: &'a ServiceConfig,
    scope: &'a Scope,
    source: &'a S,
    sink: &'a K,
}

async fn fill_and_publish<S, K>(
    ctx: &RoundContext<'_, S, K>,
    plan: &[WorkItem],
    spool: &mut Spool,
    trace: &mut RunTrace,
    cancel: &CancelFlag,
) -> Result<(), HarvestError>
where
    S: Source + ?Sized,
    K: Sink + ?Sized,
{
    let source = ctx.source;
    let scope = ctx.scope;
    let svc = ctx.svc;
    let sink = ctx.sink;
    trace.machine.advance(RunState::Fetching)?;
    spool.set_state(RunState::Fetching);
    spool.save()?;

    for item in plan {
        if cancel.is_cancelled() {
            return Err(HarvestError::new(
                ErrorClass::Other,
                "收到退出信号，本轮中止（数据平面零帧；spool 保留供下次续跑）",
            ));
        }
        if spool.is_complete(item) {
            crate::redact::debug(format!("跳过已完成工作项 {}", item.key()));
            continue;
        }
        crate::redact::info(format!("抓取 {}", item.key()));
        spool.begin_item(item)?;
        match source.fetch(item, spool).await {
            Ok(stat) => {
                spool.finish_item(item, &stat)?;
                crate::redact::info(format!(
                    "完成 {}：{} 行 / {} 页 / {} 次重试",
                    item.key(),
                    stat.rows,
                    stat.pages,
                    stat.retries
                ));
            }
            Err(err) => {
                spool.fail_item(item, &err);
                return Err(err);
            }
        }
    }

    trace.machine.advance(RunState::Spooled)?;
    spool.set_state(RunState::Spooled);
    spool.save()?;

    // 完整性判定：不通过就 Aborted，绝不进入组装/发布。
    source.verify(spool.manifest())?;
    // `complete` 只能在这里置真：上面的 verify 已经挡掉了不完整的帧，
    // 能走到这一行就说明这是采集器自己校验过的完整枚举。
    //
    // `scope` 还要按**实际计划到的类型**再收敛一次：少采一类却声称账号级全集，
    // 会让下游把「这次没采」当成「已经不存在」（见 `Coverage::for_planned_types`）。
    let declared = source.coverage();
    let planned_types = spool.manifest().planned_types();
    let coverage = Coverage {
        complete: true,
        ..declared.clone()
    }
    .for_planned_types(&planned_types);
    if coverage.scope != declared.scope {
        crate::redact::warn(format!(
            "本帧只采了 {:?}，缺 {:?}：覆盖范围从 {:?} 降为 partial（下游只 upsert、不判死）",
            planned_types,
            Coverage::missing_asset_types(&planned_types),
            declared.scope
        ));
    }
    let assembled = assemble::build_snapshot(spool, scope, coverage)?;
    if assembled.duplicate_eip_rows > 0 {
        crate::redact::info(format!(
            "EIP 双路合并去重 {} 行（wanip 与 all 的重叠）",
            assembled.duplicate_eip_rows
        ));
    }

    trace.machine.advance(RunState::Verified)?;
    spool.set_state(RunState::Verified);
    spool.save()?;

    // 快照正文先序列化，再包成数据面记录（JSON 头 + DSNAP: + 正文，不带 NUL）。
    // 为什么要包：WPL 的规则是 `json(...), symbol(...), chars:body`，直接发裸 JSON
    // 结构对不上，记录会掉进兜底 sink 或被拒；而 NUL 会被引擎一起收进 body。
    let snapshot_json = serde_json::to_vec(&assembled.snapshot)
        .map_err(|err| HarvestError::data_format(format!("快照序列化失败: {err}")))?;
    let bytes = crate::wire::warp_record(
        &snapshot_json,
        crate::wire::WIRE_SCHEMA,
        &scope.source_code,
        &assembled.snapshot.collected_at,
        1,
    )?;
    if bytes.len() > svc.service.max_frame_bytes {
        return Err(HarvestError::integrity(format!(
            "帧体 {} 字节超过上限 {}；应推动下游支持分片组装，而不是拆 snap_id",
            bytes.len(),
            svc.service.max_frame_bytes
        )));
    }

    let mut per_type: BTreeMap<AssetType, u64> = BTreeMap::new();
    per_type.insert(
        AssetType::CloudHost,
        assembled.snapshot.stats.cloud_host.total,
    );
    per_type.insert(AssetType::Eip, assembled.snapshot.stats.eip.total);
    per_type.insert(AssetType::Vpc, assembled.snapshot.stats.vpc.total);
    per_type.insert(AssetType::SubNet, assembled.snapshot.stats.sub_net.total);
    let rows: u64 = per_type.values().sum();

    let complete = CompleteSnapshot {
        snap_id: assembled.snapshot.snap_id.clone(),
        bytes,
        rows,
        per_type,
    };

    // 唯一一次发送。
    let ack = sink.publish_snapshot(&complete).await?;
    crate::redact::info(format!("数据平面已确认：{}", ack.ack));
    trace.published = Some(ack);

    trace.machine.advance(RunState::Published)?;
    spool.set_state(RunState::Published);
    spool.save()?;

    trace.machine.advance(RunState::Done)?;
    spool.set_state(RunState::Done);
    spool.save()?;
    Ok(())
}

fn build_report(
    scope: &Scope,
    trace: &RunTrace,
    started: chrono::DateTime<chrono::Utc>,
    finished: chrono::DateTime<chrono::Utc>,
    abort: Option<&HarvestError>,
) -> RunReport {
    let scope_report = trace
        .spool
        .as_ref()
        .map(|spool| RunReport::from_manifest(spool.manifest()))
        .unwrap_or_else(|| ScopeReport::from_scope(scope));

    let snap_id = trace
        .spool
        .as_ref()
        .map(|spool| spool.snap_id().to_string())
        .unwrap_or_else(|| "-".to_string());

    let mut per_type: BTreeMap<String, TypeReport> = BTreeMap::new();
    let mut items: Vec<ItemReport> = Vec::new();

    if let Some(spool) = trace.spool.as_ref() {
        let manifest = spool.manifest();
        for asset_type in manifest.planned_types() {
            let records = manifest.items_of(asset_type);
            per_type.insert(
                asset_type.as_str().to_string(),
                TypeReport {
                    rows: records.iter().map(|r| r.rows).sum(),
                    pages: records.iter().map(|r| r.pages).sum(),
                    items: records.len(),
                },
            );
        }
        for record in &manifest.items {
            items.push(ItemReport {
                asset_type: record.asset_type.as_str().to_string(),
                region: record.region.clone(),
                lane: record.lane.clone(),
                completed: record.completed,
                rows: record.rows,
                pages: record.pages,
                expected_rows: record.expected_rows,
                completeness: record.completeness.clone(),
                retries: record.retries,
                duration_ms: record.duration_ms,
                request_ids: record.request_ids.clone(),
                error: record.error.as_ref().map(|e| crate::redact::scrub(e)),
            });
        }
    }

    let mut errors = Vec::new();
    if let Some(err) = abort {
        errors.push(ErrorEntry::from_error(err));
    } else if let Some(err) = trace
        .spool
        .as_ref()
        .and_then(|spool| spool.manifest().error.as_ref())
    {
        errors.push(ErrorEntry {
            class: err.class.as_str().to_string(),
            message: crate::redact::scrub(&err.message),
            action: None,
            region: None,
            request_id: None,
            code: None,
            at: err.at.clone(),
        });
    }

    RunReport {
        schema: crate::SCHEMA_RUN_REPORT.to_string(),
        snap_id,
        scope: scope_report,
        state: trace.machine.state(),
        state_trail: trace
            .machine
            .trail()
            .iter()
            .map(|s| s.as_str().to_string())
            .collect(),
        started_at: crate::time::rfc3339_utc(started),
        finished_at: crate::time::rfc3339_utc(finished),
        duration_ms: (finished - started).num_milliseconds().max(0) as u64,
        resumed: trace.resumed,
        per_type,
        items,
        sink: trace
            .published
            .as_ref()
            .map(|ack| crate::report::SinkReport {
                bytes: ack.bytes,
                ack: ack.ack.clone(),
                duration_ms: ack.duration_ms,
            }),
        errors,
        abort_reason: abort.map(|err| crate::redact::scrub(&err.to_string())),
    }
}

/// 供应用层把"错误类别"映射成退出码时使用。
pub fn exit_code(state: RunState, err: Option<&ErrorClass>) -> i32 {
    match state {
        RunState::Done => 0,
        RunState::Aborted => match err {
            Some(ErrorClass::InvalidParam) | Some(ErrorClass::Auth) => 3,
            _ => 2,
        },
        _ => 1,
    }
}
