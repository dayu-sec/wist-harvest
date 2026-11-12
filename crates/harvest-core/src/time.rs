//! 时间格式约定。
//!
//! - `snap_id` 的时间部分固定为 UTC `yyyyMMddTHHmmssZ`；
//! - 运行报告里的时间用 UTC RFC3339（毫秒）；
//! - 帧体里的 `collected_at` 沿用交接文档样例的 `+08:00` 形态。

use chrono::{DateTime, FixedOffset, SecondsFormat, TimeZone, Utc};

/// snap_id 的时间后缀，例如 `20261112T100000Z`。
pub fn snap_timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// 报告/清单里的时间：UTC RFC3339，毫秒精度。
pub fn rfc3339_utc(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 帧体 `collected_at`：固定 +08:00 的 RFC3339。
pub fn rfc3339_cn(at: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00 是合法偏移");
    at.with_timezone(&offset)
        .to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// 从 UNIX 时间戳重建 UTC 时间（TC3 签名测试与报告解析用）。
pub fn from_unix(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_timestamp_is_compact_utc() {
        let at = from_unix(1_760_000_000);
        assert_eq!(snap_timestamp(at), "20251009T085320Z");
    }

    #[test]
    fn collected_at_uses_plus_eight() {
        let at = from_unix(1_760_000_000);
        assert_eq!(rfc3339_cn(at), "2025-10-09T16:53:20+08:00");
    }
}
