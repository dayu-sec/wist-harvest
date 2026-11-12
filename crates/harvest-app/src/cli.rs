//! 命令行取值解析：`HH:MM` 与时长。
//!
//! 时长支持 `ms`/`s`/`m`/`h` 后缀，纯数字按秒解释；负值/零值按调用方语义处理。

use std::time::Duration;

use chrono::NaiveTime;
use harvest_core::schedule::{Backoff, ScheduleMode, SchedulerConfig};

/// 解析 `HH:MM`（24 小时制）。
pub fn parse_hhmm(text: &str) -> Result<NaiveTime, String> {
    let parsed = NaiveTime::parse_from_str(text.trim(), "%H:%M")
        .map_err(|_| format!("时间需要 HH:MM 形式（如 00:30），实际是 {text}"))?;
    Ok(parsed)
}

/// 解析时长：`500ms` / `30s` / `5m` / `2h` / 纯秒数。
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("时长不能为空".to_string());
    }
    let split_at = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split_at);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("时长需要数字开头（如 5m），实际是 {text}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("时长必须是非负数，实际是 {text}"));
    }
    let seconds = match unit.trim() {
        "" | "s" | "sec" | "secs" => value,
        "ms" => value / 1000.0,
        "m" | "min" | "mins" => value * 60.0,
        "h" | "hr" | "hrs" => value * 3600.0,
        other => return Err(format!("不认识的时长单位 {other}（支持 ms/s/m/h）")),
    };
    Ok(Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hhmm() {
        assert_eq!(parse_hhmm("00:30").unwrap().to_string(), "00:30:00");
        assert_eq!(parse_hhmm("22:05").unwrap().to_string(), "22:05:00");
        // 宽松接受不补零的写法，归一成同一时刻。
        assert_eq!(parse_hhmm("0:30").unwrap().to_string(), "00:30:00");
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("00:60").is_err());
        assert!(parse_hhmm("半小时").is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
    }

    #[test]
    fn rejects_bad_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-5m").is_err());
    }
}

/// 从命令行参数拼出调度配置。
///
/// 默认：每天 00:30（Asia/Shanghai）+ 0~5 分钟随机抖动。
pub fn build_scheduler(
    run_at: Option<&str>,
    interval: Option<&str>,
    jitter: Option<&str>,
    max_retries: Option<u32>,
    backoff_base: Option<&str>,
    backoff_max: Option<&str>,
) -> Result<SchedulerConfig, String> {
    if run_at.is_some() && interval.is_some() {
        return Err("--run-at 与 --interval 只能给一个".to_string());
    }
    let mode = match interval {
        Some(text) => {
            let every = parse_duration(text)?;
            if every.is_zero() {
                return Err("--interval 必须大于 0".to_string());
            }
            ScheduleMode::Interval { every }
        }
        None => ScheduleMode::Daily {
            at: parse_hhmm(run_at.unwrap_or("00:30"))?,
        },
    };

    let jitter_max = match jitter {
        Some(text) => parse_duration(text)?,
        None => Duration::from_secs(5 * 60),
    };

    let base = match backoff_base {
        Some(text) => parse_duration(text)?,
        None => Duration::from_secs(60),
    };
    let max = match backoff_max {
        Some(text) => parse_duration(text)?,
        None => Duration::from_secs(15 * 60),
    };
    if base.is_zero() {
        return Err("--backoff-base 必须大于 0".to_string());
    }
    if max < base {
        return Err("--backoff-max 不能小于 --backoff-base".to_string());
    }

    Ok(SchedulerConfig {
        mode,
        jitter_max,
        backoff: Backoff { base, max },
        max_retries_per_cycle: max_retries.unwrap_or(3),
    })
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;

    #[test]
    fn defaults_are_daily_0030_with_five_minute_jitter() {
        let cfg = build_scheduler(None, None, None, None, None, None).unwrap();
        assert_eq!(
            cfg.mode,
            ScheduleMode::Daily {
                at: NaiveTime::from_hms_opt(0, 30, 0).unwrap()
            }
        );
        assert_eq!(cfg.jitter_max, Duration::from_secs(300));
        assert_eq!(cfg.backoff.base, Duration::from_secs(60));
        assert_eq!(cfg.backoff.max, Duration::from_secs(900));
        assert_eq!(cfg.max_retries_per_cycle, 3);
    }

    #[test]
    fn interval_mode_overrides_run_at() {
        let cfg = build_scheduler(None, Some("2h"), Some("0"), Some(0), None, None).unwrap();
        assert_eq!(
            cfg.mode,
            ScheduleMode::Interval {
                every: Duration::from_secs(7200)
            }
        );
        assert_eq!(cfg.jitter_max, Duration::ZERO);
        assert_eq!(cfg.max_retries_per_cycle, 0);
    }

    #[test]
    fn run_at_and_interval_conflict() {
        assert!(build_scheduler(Some("00:30"), Some("1h"), None, None, None, None).is_err());
    }

    #[test]
    fn rejects_zero_interval_and_bad_backoff() {
        assert!(build_scheduler(None, Some("0s"), None, None, None, None).is_err());
        assert!(build_scheduler(None, None, None, None, Some("0s"), None).is_err());
        assert!(build_scheduler(None, None, None, None, Some("10m"), Some("1m")).is_err());
    }
}
