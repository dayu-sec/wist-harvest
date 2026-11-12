//! 常驻调度：定点触发 + 抖动 + 失败退避 + 上一轮未结束则跳过。
//!
//! 两个语义必须同时成立，容易写错：
//!
//! - **到点就跑**：下一轮定点由墙上时钟算出来，失败和重试都**不推迟**它；
//! - **失败退避**：上一轮失败后按指数退避安排重试，但重试只是"额外的补救"，
//!   一旦重试时间晚于下一轮定点就直接丢掉——定点会跑，没必要再补。
//!
//! 另外"定点"用的是 Asia/Shanghai 的挂钟时间。该时区 1991 年后没有夏令时，
//! 固定 `+08:00` 就够了，不需要引入时区数据库。

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, NaiveTime, TimeZone, Utc};

/// Asia/Shanghai 的固定偏移。
pub const CN_OFFSET_SECONDS: i32 = 8 * 3600;

fn to_chrono(duration: Duration) -> ChronoDuration {
    ChronoDuration::from_std(duration).unwrap_or_else(|_| ChronoDuration::zero())
}

/// 调度模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleMode {
    /// 每天 `HH:MM`（Asia/Shanghai）触发。
    Daily { at: NaiveTime },
    /// 固定间隔触发。间隔从**上一轮结束**起算，所以慢轮次不会堆积。
    Interval { every: Duration },
}

impl ScheduleMode {
    /// 下一次触发时刻（已叠加抖动），严格晚于 `now`。
    pub fn next_after(&self, now: DateTime<Utc>, jitter: Duration) -> DateTime<Utc> {
        match self {
            Self::Interval { every } => now + to_chrono(*every + jitter),
            Self::Daily { at } => next_daily(now, *at, jitter),
        }
    }
}

fn next_daily(now: DateTime<Utc>, at: NaiveTime, jitter: Duration) -> DateTime<Utc> {
    let offset = FixedOffset::east_opt(CN_OFFSET_SECONDS).expect("+08:00 是合法偏移");
    let local = now.with_timezone(&offset);
    let mut date = local.date_naive();
    for _ in 0..3 {
        let naive = date.and_time(at);
        let candidate = offset
            .from_local_datetime(&naive)
            .single()
            .expect("+08:00 不存在夏令时歧义")
            .to_utc()
            + to_chrono(jitter);
        if candidate > now {
            return candidate;
        }
        match date.succ_opt() {
            Some(next) => date = next,
            None => break,
        }
    }
    now + to_chrono(Duration::from_secs(86_400))
}

/// 从 `seed` 派生 `[0, max]` 内的抖动。splitmix64：够均匀，且不引入随机数依赖。
pub fn jitter_sample(max: Duration, seed: u64) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let span = u64::try_from(max.as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let nanos = x % span;
    Duration::from_nanos(nanos)
}

/// 指数退避：`base * 2^(n-1)`，上限 `max`。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(60),
            max: Duration::from_secs(15 * 60),
        }
    }
}

impl Backoff {
    pub fn delay(&self, consecutive_failures: u32) -> Duration {
        if consecutive_failures == 0 {
            return Duration::ZERO;
        }
        let shift = (consecutive_failures - 1).min(20);
        let factor = 1u64 << shift;
        let base_ms = u64::try_from(self.base.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let max_ms = u64::try_from(self.max.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        Duration::from_millis(base_ms.saturating_mul(factor).min(max_ms))
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub mode: ScheduleMode,
    /// 抖动上限：在定点上再加一个 `[0, jitter_max]` 的随机偏移，
    /// 避免多站点同一秒打云 API。
    pub jitter_max: Duration,
    pub backoff: Backoff,
    /// 同一调度周期内最多额外重试几次（0 = 只跑定点，不重试）。
    pub max_retries_per_cycle: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            mode: ScheduleMode::Daily {
                at: NaiveTime::from_hms_opt(0, 30, 0).expect("00:30 合法"),
            },
            jitter_max: Duration::from_secs(5 * 60),
            backoff: Backoff::default(),
            max_retries_per_cycle: 3,
        }
    }
}

/// 本轮是定点触发还是失败重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundTrigger {
    Scheduled,
    Retry,
}

impl RoundTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Retry => "retry",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundResult {
    Success,
    Failure,
}

/// 到点之后该做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    Start(RoundTrigger),
    /// 上一轮还没结束：本轮跳过，并推进到下一个定点（不能原地忙等）。
    Skipped {
        next: DateTime<Utc>,
    },
}

/// 调度状态机。纯逻辑：时间与抖动都由调用方注入，方便测试。
#[derive(Debug, Clone)]
pub struct Scheduler {
    cfg: SchedulerConfig,
    next_scheduled: DateTime<Utc>,
    retry_at: Option<DateTime<Utc>>,
    consecutive_failures: u32,
    retries_this_cycle: u32,
    pending: Option<RoundTrigger>,
}

impl Scheduler {
    pub fn new(cfg: SchedulerConfig, now: DateTime<Utc>, jitter: Duration) -> Self {
        let next_scheduled = cfg.mode.next_after(now, jitter);
        Self {
            cfg,
            next_scheduled,
            retry_at: None,
            consecutive_failures: 0,
            retries_this_cycle: 0,
            pending: None,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    /// 让下一轮立刻触发（`run --now` 与测试用）。后续仍按 `mode` 推进。
    ///
    /// 注意：这只是把「下一个定点」拉到当下，失败退避的规则不变。
    pub fn trigger_now(&mut self, now: DateTime<Utc>) {
        self.next_scheduled = now;
    }

    /// 下一个**定点**。失败不会改它——这就是"到点就跑"。
    pub fn next_scheduled(&self) -> DateTime<Utc> {
        self.next_scheduled
    }

    /// 下一次失败重试的时刻（如果有）。
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        self.retry_at
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn next_wake(&self) -> DateTime<Utc> {
        match self.retry_at {
            Some(retry) if retry < self.next_scheduled => retry,
            _ => self.next_scheduled,
        }
    }

    /// 醒来：决定开始一轮还是跳过。
    ///
    /// `previous_round_running` 由调用方观察在飞任务得出。跳过时不改失败计数，
    /// 但必须推进定点，否则下一轮醒来还是同一时刻，会变成忙等。
    pub fn on_wake(
        &mut self,
        now: DateTime<Utc>,
        jitter: Duration,
        previous_round_running: bool,
    ) -> WakeOutcome {
        if previous_round_running {
            self.retry_at = None;
            self.retries_this_cycle = 0;
            self.next_scheduled = self.cfg.mode.next_after(now, jitter);
            return WakeOutcome::Skipped {
                next: self.next_scheduled,
            };
        }

        let trigger = match self.retry_at {
            Some(retry) if retry < self.next_scheduled => {
                self.retry_at = None;
                RoundTrigger::Retry
            }
            _ => {
                self.retry_at = None;
                self.retries_this_cycle = 0;
                self.next_scheduled = self.cfg.mode.next_after(now, jitter);
                RoundTrigger::Scheduled
            }
        };
        self.pending = Some(trigger);
        WakeOutcome::Start(trigger)
    }

    /// 本轮结束：更新失败计数与重试计划。
    pub fn on_round_finished(&mut self, now: DateTime<Utc>, result: RoundResult) {
        let trigger = self.pending.take().unwrap_or(RoundTrigger::Scheduled);
        match result {
            RoundResult::Success => {
                self.consecutive_failures = 0;
                self.retries_this_cycle = 0;
                self.retry_at = None;
            }
            RoundResult::Failure => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if trigger == RoundTrigger::Retry {
                    self.retries_this_cycle = self.retries_this_cycle.saturating_add(1);
                }
                self.retry_at = None;
                if self.retries_this_cycle < self.cfg.max_retries_per_cycle {
                    let at = now + to_chrono(self.cfg.backoff.delay(self.consecutive_failures));
                    // 重试晚于下一个定点就丢掉：定点本来就会跑。
                    if at < self.next_scheduled {
                        self.retry_at = Some(at);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("测试用固定时间")
            .to_utc()
    }

    fn daily(hh: u32, mm: u32, jitter_secs: u64) -> SchedulerConfig {
        SchedulerConfig {
            mode: ScheduleMode::Daily {
                at: NaiveTime::from_hms_opt(hh, mm, 0).unwrap(),
            },
            jitter_max: Duration::from_secs(jitter_secs),
            ..Default::default()
        }
    }

    // ---- 定点 + 时区 + 跨日 ----

    #[test]
    fn next_run_at_uses_shanghai_wall_clock() {
        // 2026-11-12T15:00:00Z = 11-12 23:00 +08:00，当天 00:30 已过 → 次日 00:30。
        let cfg = daily(0, 30, 0);
        let next = cfg
            .mode
            .next_after(utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        assert_eq!(next, utc("2026-11-12T16:30:00Z"), "00:30 +08:00 == 16:30Z");
    }

    #[test]
    fn next_run_at_crosses_day_after_trigger() {
        // 2026-11-12T16:31:00Z = 11-13 00:31 +08:00，刚过 00:30 → 11-14 的 00:30。
        let cfg = daily(0, 30, 0);
        let next = cfg
            .mode
            .next_after(utc("2026-11-12T16:31:00Z"), Duration::ZERO);
        assert_eq!(next, utc("2026-11-13T16:30:00Z"));
    }

    #[test]
    fn next_run_at_is_strictly_in_the_future() {
        let cfg = daily(0, 30, 0);
        // 正好在触发点上：应给下一天，而不是原地返回。
        let at = utc("2026-11-12T16:30:00Z");
        assert_eq!(
            cfg.mode.next_after(at, Duration::ZERO),
            utc("2026-11-13T16:30:00Z")
        );
    }

    #[test]
    fn evening_run_at_lands_on_the_same_utc_day() {
        // 22:30 +08:00 == 14:30Z；UTC 与本地日期不同的场景。
        let cfg = daily(22, 30, 0);
        let next = cfg
            .mode
            .next_after(utc("2026-11-12T00:00:00Z"), Duration::ZERO);
        assert_eq!(next, utc("2026-11-12T14:30:00Z"));
    }

    // ---- 抖动 ----

    #[test]
    fn jitter_is_added_on_top_of_the_daily_time() {
        let cfg = daily(0, 30, 300);
        let base = utc("2026-11-12T16:30:00Z");
        assert_eq!(
            cfg.mode
                .next_after(utc("2026-11-12T15:00:00Z"), Duration::from_secs(300)),
            base + ChronoDuration::seconds(300)
        );
    }

    #[test]
    fn jitter_sample_stays_within_range() {
        let max = Duration::from_secs(300);
        let mut seen_big = false;
        for seed in 0..500u64 {
            let sample = jitter_sample(max, seed);
            assert!(sample <= max, "seed={seed} sample={sample:?}");
            if sample > Duration::from_secs(250) {
                seen_big = true;
            }
        }
        assert!(seen_big, "抖动应该真的分散，而不是恒为 0");
        assert_eq!(jitter_sample(Duration::ZERO, 42), Duration::ZERO);
    }

    #[test]
    fn jittered_trigger_never_lands_before_the_base_time() {
        let cfg = daily(0, 30, 300);
        let now = utc("2026-11-12T15:00:00Z");
        let base = utc("2026-11-12T16:30:00Z");
        for seed in 0..200u64 {
            let next = cfg
                .mode
                .next_after(now, jitter_sample(Duration::from_secs(300), seed));
            assert!(next >= base, "抖动不能把触发时间提前");
            assert!(next <= base + ChronoDuration::seconds(300));
        }
    }

    // ---- 退避 ----

    #[test]
    fn backoff_grows_then_caps() {
        let backoff = Backoff {
            base: Duration::from_secs(60),
            max: Duration::from_secs(15 * 60),
        };
        let seq: Vec<u64> = (1..=7).map(|n| backoff.delay(n).as_secs()).collect();
        assert_eq!(seq, vec![60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(backoff.delay(0), Duration::ZERO);
    }

    // ---- 跳过与失败重试共存 ----

    #[test]
    fn skipping_when_previous_round_runs_advances_the_schedule() {
        let cfg = daily(0, 30, 0);
        let mut sched = Scheduler::new(cfg, utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        assert_eq!(sched.next_scheduled(), utc("2026-11-12T16:30:00Z"));

        let wake = sched.on_wake(utc("2026-11-12T16:30:00Z"), Duration::ZERO, true);
        assert_eq!(
            wake,
            WakeOutcome::Skipped {
                next: utc("2026-11-13T16:30:00Z")
            }
        );
        assert_eq!(sched.next_scheduled(), utc("2026-11-13T16:30:00Z"));
        assert_eq!(sched.consecutive_failures(), 0, "跳过不算失败");
        assert_eq!(sched.retry_at(), None);
    }

    #[test]
    fn failure_schedules_a_retry_without_moving_the_next_scheduled_run() {
        let cfg = daily(0, 30, 0);
        let mut sched = Scheduler::new(cfg, utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        let fire_at = utc("2026-11-12T16:30:00Z");

        assert_eq!(
            sched.on_wake(fire_at, Duration::ZERO, false),
            WakeOutcome::Start(RoundTrigger::Scheduled)
        );
        sched.on_round_finished(fire_at + ChronoDuration::seconds(5), RoundResult::Failure);

        // 定点仍然是次日 00:30，失败没有把它推后。
        assert_eq!(sched.next_scheduled(), utc("2026-11-13T16:30:00Z"));
        // 第一次失败 → base 60s 后重试。
        assert_eq!(
            sched.retry_at(),
            Some(fire_at + ChronoDuration::seconds(65))
        );
        assert_eq!(
            sched.next_wake(),
            fire_at + ChronoDuration::seconds(65),
            "重试早于定点，所以下一次醒来是重试"
        );
    }

    #[test]
    fn retries_are_bounded_per_cycle_and_back_off_exponentially() {
        let mut cfg = daily(0, 30, 0);
        cfg.max_retries_per_cycle = 2;
        cfg.backoff = Backoff {
            base: Duration::from_secs(60),
            max: Duration::from_secs(15 * 60),
        };
        let mut sched = Scheduler::new(cfg, utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        let mut now = utc("2026-11-12T16:30:00Z");

        assert_eq!(
            sched.on_wake(now, Duration::ZERO, false),
            WakeOutcome::Start(RoundTrigger::Scheduled)
        );
        sched.on_round_finished(now, RoundResult::Failure);
        let first_retry = sched.retry_at().expect("第一次失败应安排重试");
        assert_eq!(first_retry, now + ChronoDuration::seconds(60));

        now = first_retry;
        assert_eq!(
            sched.on_wake(now, Duration::ZERO, false),
            WakeOutcome::Start(RoundTrigger::Retry)
        );
        sched.on_round_finished(now, RoundResult::Failure);
        let second_retry = sched.retry_at().expect("第二次失败应安排重试");
        assert_eq!(second_retry, now + ChronoDuration::seconds(120), "指数退避");

        now = second_retry;
        assert_eq!(
            sched.on_wake(now, Duration::ZERO, false),
            WakeOutcome::Start(RoundTrigger::Retry)
        );
        sched.on_round_finished(now, RoundResult::Failure);
        assert_eq!(sched.retry_at(), None, "本轮重试额度用完，等下一个定点");
        assert_eq!(sched.next_wake(), utc("2026-11-13T16:30:00Z"));
    }

    #[test]
    fn success_resets_backoff() {
        let mut cfg = daily(0, 30, 0);
        cfg.max_retries_per_cycle = 5;
        let mut sched = Scheduler::new(cfg, utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        let mut now = utc("2026-11-12T16:30:00Z");

        sched.on_wake(now, Duration::ZERO, false);
        sched.on_round_finished(now, RoundResult::Failure);
        now = sched.retry_at().unwrap();
        sched.on_wake(now, Duration::ZERO, false);
        assert_eq!(sched.consecutive_failures(), 1);
        sched.on_round_finished(now, RoundResult::Success);

        assert_eq!(sched.consecutive_failures(), 0);
        assert_eq!(sched.retry_at(), None);
        assert_eq!(sched.next_wake(), utc("2026-11-13T16:30:00Z"));
    }

    #[test]
    fn retry_that_would_land_after_the_next_run_is_dropped() {
        let mut cfg = daily(0, 30, 0);
        cfg.max_retries_per_cycle = 5;
        cfg.backoff = Backoff {
            base: Duration::from_secs(60 * 60),
            max: Duration::from_secs(60 * 60),
        };
        // 23:59 +08:00 触发，定点下一次是 00:30，只差 31 分钟 < 1 小时退避。
        let mut sched = Scheduler::new(cfg, utc("2026-11-12T15:00:00Z"), Duration::ZERO);
        let fire_at = utc("2026-11-12T15:59:00Z");
        sched.on_wake(fire_at, Duration::ZERO, false);
        sched.on_round_finished(fire_at, RoundResult::Failure);
        assert_eq!(sched.retry_at(), None, "重试会晚于定点，直接丢掉");
        assert_eq!(sched.next_wake(), utc("2026-11-12T16:30:00Z"));
    }

    #[test]
    fn interval_mode_spaces_rounds_from_completion() {
        let cfg = SchedulerConfig {
            mode: ScheduleMode::Interval {
                every: Duration::from_secs(3600),
            },
            jitter_max: Duration::ZERO,
            ..Default::default()
        };
        let now = utc("2026-11-12T00:00:00Z");
        assert_eq!(
            cfg.mode.next_after(now, Duration::ZERO),
            now + ChronoDuration::seconds(3600)
        );
    }
}
