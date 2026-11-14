//! `wist-harvest`：外部事实采集服务。
//!
//! 子命令：
//!
//! - `once`：跑一轮采集（租约 → 抓取 → 校验 → 发布）；
//! - `run`：常驻调度，按 `--run-at`（默认 00:30 Asia/Shanghai）或 `--interval` 反复跑；
//! - `dry-run`：只打印生效配置与采集计划，不联网、不写 spool；
//! - `cost`：抓一个账期范围的账单，落分片 + manifest（契约 `harvest-cost-v1`，不进数据平面）；
//! - `diagnose`：容器 healthcheck，离线分项自检。

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use harvest_app::cli;

#[derive(Parser)]
#[command(
    name = "wist-harvest",
    version,
    about = "云账号事实采集：拉取 → 归一 → spool → 完整性校验 → 数据平面发布",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 跑一轮采集。任一工作项失败即 Aborted，数据平面零帧。
    Once {
        /// 服务配置 `/etc/wist-harvest/harvest.toml`。
        #[arg(long)]
        config: PathBuf,
        /// 数据源定义 `/etc/wist-harvest/sources.d/<source_code>.toml`。
        #[arg(long)]
        source: PathBuf,
        /// 忽略可续跑的 spool，强制开新 snap_id。
        #[arg(long)]
        fresh: bool,
        /// 指定 snap_id（重放同一份快照用；下游按幂等键去重）。
        #[arg(long)]
        snap_id: Option<String>,
    },
    /// 常驻调度：按定点（默认 00:30 Asia/Shanghai）反复跑，单轮并发。
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        source: PathBuf,
        /// 每天几点跑（Asia/Shanghai），默认 00:30。与 --interval 互斥。
        #[arg(long, value_name = "HH:MM")]
        run_at: Option<String>,
        /// 固定间隔（如 30m / 1h）。给了就不看 --run-at。
        #[arg(long, value_name = "DUR")]
        interval: Option<String>,
        /// 抖动上限，默认 5m：在定点上再加一个随机偏移，避免多站点同时打云 API。
        #[arg(long, value_name = "DUR")]
        jitter: Option<String>,
        /// 同一调度周期内最多额外重试几次，默认 3。
        #[arg(long, value_name = "N")]
        max_retries: Option<u32>,
        /// 失败退避的起始间隔，默认 60s。
        #[arg(long, value_name = "DUR")]
        backoff_base: Option<String>,
        /// 失败退避的上限，默认 15m。
        #[arg(long, value_name = "DUR")]
        backoff_max: Option<String>,
        /// 收到 SIGTERM/SIGINT 后等在飞轮次收尾的宽限期，默认 30s。
        #[arg(long, value_name = "DUR")]
        shutdown_grace: Option<String>,
        /// 忽略可续跑的 spool。
        #[arg(long)]
        fresh: bool,
        /// 启动后立即跑一轮，不等第一个定点。
        #[arg(long)]
        now: bool,
    },
    /// 打印生效配置与采集计划，不联网、不写盘。
    DryRun {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        source: PathBuf,
    },
    /// 抓一个账期范围的账单：分片 + manifest（成本不进数据平面）。
    Cost {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        source: PathBuf,
        /// 整月账单（`CHARGE`），格式 `YYYY-MM`。默认上一个自然月。
        #[arg(long, value_name = "YYYY-MM", conflicts_with = "day")]
        month: Option<String>,
        /// 单日消耗（`CONSUMPTION`），格式 `YYYY-MM-DD`。
        #[arg(long, value_name = "YYYY-MM-DD")]
        day: Option<String>,
        /// 分片根目录（默认数据源 `[cost].output_dir`，再退回 `<state_dir>/cost`）。
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// 自检（容器 healthcheck 可用）：离线、非交互、不输出凭据。
    Diagnose {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        source: PathBuf,
        /// 只输出失败项。
        #[arg(long)]
        quiet: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("无法创建异步运行时: {err}");
            std::process::exit(1);
        }
    };

    let code = runtime.block_on(async move {
        match cli.command {
            Command::Once {
                config,
                source,
                fresh,
                snap_id,
            } => harvest_app::once::run(config, source, fresh, snap_id).await,
            Command::Run {
                config,
                source,
                run_at,
                interval,
                jitter,
                max_retries,
                backoff_base,
                backoff_max,
                shutdown_grace,
                fresh,
                now,
            } => {
                let scheduler = match cli::build_scheduler(
                    run_at.as_deref(),
                    interval.as_deref(),
                    jitter.as_deref(),
                    max_retries,
                    backoff_base.as_deref(),
                    backoff_max.as_deref(),
                ) {
                    Ok(scheduler) => scheduler,
                    Err(message) => {
                        eprintln!("调度参数不可用: {message}");
                        return 3;
                    }
                };
                let grace = match shutdown_grace.as_deref() {
                    Some(text) => match cli::parse_duration(text) {
                        Ok(duration) => duration,
                        Err(message) => {
                            eprintln!("--shutdown-grace 不可用: {message}");
                            return 3;
                        }
                    },
                    None => Duration::from_secs(30),
                };

                // `enabled` / `[control]` 是运行时开关（中心关采集时进程常驻但不调度）。
                // 这里单独读一次配置：读不出来要在启动期就明确失败，而不是等到第一轮。
                let service = match harvest_core::config::ServiceConfig::load(&config) {
                    Ok(service) => service,
                    Err(err) => {
                        eprintln!("采集配置不可用：{err}");
                        return 3;
                    }
                };
                let enabled = service.service.enabled;
                let control = if service.control.is_enabled() {
                    // token 可选（内网端点可能不校验）；读到就注册进脱敏器，永不落日志。
                    let token = match service.control.credential.get("bearer") {
                        Ok(secret) => {
                            harvest_core::redact::register_secret(secret.expose());
                            Some(secret.expose().to_string())
                        }
                        Err(_) => None,
                    };
                    Some(harvest_app::serve::ControlPlan {
                        url: service.control.url.trim().to_string(),
                        token,
                        poll_interval: service.control.poll_interval(),
                    })
                } else {
                    None
                };

                let opts = harvest_app::serve::ServeOptions {
                    enabled,
                    control,
                    scheduler,
                    resume: !fresh,
                    shutdown_grace: grace,
                    run_immediately: now,
                };
                let round_config = config.clone();
                let round_source = source.clone();
                let resume = !fresh;

                let stats = harvest_app::serve::serve(
                    opts,
                    harvest_app::serve::shutdown_signal(),
                    move |cancel| {
                        let config = round_config.clone();
                        let source = round_source.clone();
                        async move {
                            harvest_app::round::run_round(&config, &source, resume, cancel).await
                        }
                    },
                )
                .await;

                println!(
                    "{}",
                    serde_json::to_string_pretty(&stats.to_json("signal")).unwrap_or_default()
                );
                // 优雅退出：无论最后一轮是 Done 还是 Aborted，退出码都是 0。
                0
            }
            Command::Cost {
                config,
                source,
                month,
                day,
                output_dir,
            } => harvest_app::cost::run(config, source, month, day, output_dir).await,
            Command::DryRun { config, source } => harvest_app::dryrun::run(&config, &source),
            Command::Diagnose {
                config,
                source,
                quiet,
            } => harvest_app::diagnose::run(&config, &source, quiet),
        }
    });
    std::process::exit(code);
}
