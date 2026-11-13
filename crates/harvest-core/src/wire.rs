//! 数据面的**线上记录格式**。
//!
//! 平台已有的生产者（`wist-agentd`）在 `framing = "len"` 的 tcp source 上发的不是纯 JSON，
//! 而是：
//!
//! ```text
//! {"schema":..,"agent":..,"ts":..,"seq":..} DSNAP: <正文>
//! ```
//!
//! 对应 `models/wpl/cloud_inventory/parse.wpl` 里的
//! `json(chars@schema, chars@agent, chars@ts, digit@seq), symbol(CLOUDINV:), chars:body`，
//! 符号两侧各有一个空格。
//!
//! 这里照搬同一形态，只把符号换成 ` CLOUDINV: `。**不要**把快照直接当 JSON 发出去：
//! 那样 WPL 的 `json(...), symbol(...)` 结构对不上，记录会落进兜底 sink 或被拒。
//! 也**不要**在正文末尾补 NUL：实际 WPL 是 `chars:body`，引擎会把 NUL 一起收进 body。
//!
//! 字段取值都来自快照本身（`collected_at` 而不是 `now()`），这样**同一 `snap_id` 重放
//! 的帧字节完全一致**，幂等可以直接比对。

use crate::error::HarvestError;

/// 记录的 `schema` 值。属于**线上外壳**，与快照内部的 `schema_native` 是两件事：
/// 外壳给 wparse 路由/规则用，`schema_native` 给下游解析正文用。
pub const WIRE_SCHEMA: &str = "cloud_inventory/1";

/// WPL 规则里的符号，与 `models/wpl` 中 `symbol(CLOUDINV:)` 必须逐字一致。
pub const WIRE_SYMBOL: &str = " CLOUDINV: ";

/// 把快照正文包成数据面记录。
pub fn warp_record(
    snapshot_json: &[u8],
    schema: &str,
    agent: &str,
    ts: &str,
    seq: u64,
) -> Result<Vec<u8>, HarvestError> {
    let header = serde_json::json!({
        "schema": schema,
        "agent": agent,
        "ts": ts,
        "seq": seq,
    });
    let mut out = serde_json::to_vec(&header)
        .map_err(|err| HarvestError::data_format(format!("记录头序列化失败: {err}")))?;
    out.extend_from_slice(WIRE_SYMBOL.as_bytes());
    // **不加 NUL**：本引擎的 `chars:body\0` 会把 NUL 一起收进 body，
    // 下游拿到 `{...}\x00` 就解不开 JSON。帧边界由长度前缀决定，不需要终结符。
    out.extend_from_slice(snapshot_json);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_matches_the_agent_uplink_wire_shape() {
        let body = br#"{"schema_native":"wist.harvest.snapshot.v1"}"#;
        let wire = warp_record(
            body,
            WIRE_SCHEMA,
            "tencent-cloud-prod",
            "2026-11-12T10:00:00+08:00",
            1,
        )
        .unwrap();

        // 正文原样跟在符号之后，末尾**不带**终结符。
        assert_ne!(wire.last(), Some(&0u8), "不能带 NUL：它会被收进 body");
        let symbol_at = wire
            .windows(WIRE_SYMBOL.len())
            .position(|w| w == WIRE_SYMBOL.as_bytes())
            .expect("必须带 CLOUDINV: 符号");
        assert_eq!(&wire[symbol_at + WIRE_SYMBOL.len()..], body);

        // 头是合法 JSON，字段与 WPL 的 json(...) 声明一一对应。
        let header: serde_json::Value = serde_json::from_slice(&wire[..symbol_at]).unwrap();
        assert_eq!(header["schema"], serde_json::json!(WIRE_SCHEMA));
        assert_eq!(header["agent"], serde_json::json!("tencent-cloud-prod"));
        assert_eq!(header["ts"], serde_json::json!("2026-11-12T10:00:00+08:00"));
        assert_eq!(header["seq"], serde_json::json!(1));
    }

    #[test]
    fn replay_is_byte_identical() {
        let body = br#"{"a":1}"#;
        let a = warp_record(body, WIRE_SCHEMA, "s", "t", 1).unwrap();
        let b = warp_record(body, WIRE_SCHEMA, "s", "t", 1).unwrap();
        assert_eq!(a, b);
    }
}
