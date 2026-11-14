//! 中心配置轮询：**harvest 自己去拉** `harvest.enabled`（不经 gwlinkd）。
//!
//! ## 为什么
//!
//! 「采集开不开」是中心控制逻辑，但执行不必经过本机代理：采集器定时问一句中心就行。
//! 好处是 gwlinkd 不需要认识 harvest（否则每加一个可开关的服务都要改一次它），
//! 而且开关**不需要重启容器**——下一拍就生效。
//!
//! ## 契约（v1）
//!
//! ```text
//! GET <control.url>
//! Authorization: Bearer <token>          # token 来自 .wist/sec-value.yml
//!
//! 200 {"enabled": true}                  # 只认 enabled；revision 可选，仅用于日志
//! ```
//!
//! ## 失败语义
//!
//! **保持上一个已知值**（不因为中心抖一下就停采 / 停不下来）：
//!
//! - 从没拉到过 → 用本地 `[service] enabled`；
//! - 拉失败 → 沿用上一拍的值，并**只在状态翻转时**打日志（避免每 30s 一条噪声）。
//!
//! 本地 `[service] enabled = false` 是**硬关**：远端说 true 也不会采（现场要停就得停）。

use std::time::Duration;

/// 中心返回的配置（只取我们关心的字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteConfig {
    pub enabled: bool,
    pub revision: Option<u64>,
}

/// 解析中心响应体。`enabled` 缺失或不是布尔值 = 契约不符，明确报错（不猜）。
pub fn parse_state(body: &[u8]) -> Result<RemoteConfig, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|err| format!("响应不是 JSON：{err}"))?;
    let enabled = value
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "响应缺少布尔字段 `enabled`".to_string())?;
    let revision = value.get("revision").and_then(serde_json::Value::as_u64);
    Ok(RemoteConfig { enabled, revision })
}

/// 拉一次中心配置。
pub async fn fetch_state(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Result<RemoteConfig, String> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("请求中心配置失败：{err}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("读取中心配置响应失败：{err}"))?;
    if !status.is_success() {
        // 401 是最常见的配置错误（token 不匹配）：把状态码带出来，别让人只看到 "failed"。
        return Err(format!(
            "中心配置返回 {}：{}",
            status.as_u16(),
            String::from_utf8_lossy(&body[..body.len().min(200)])
        ));
    }
    parse_state(&body)
}

pub fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("构建 HTTP 客户端失败：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_and_optional_revision() {
        let state = parse_state(br#"{"enabled":false}"#).unwrap();
        assert!(!state.enabled);
        assert_eq!(state.revision, None);

        let state = parse_state(br#"{"enabled":true,"revision":7}"#).unwrap();
        assert!(state.enabled);
        assert_eq!(state.revision, Some(7));
    }

    #[test]
    fn rejects_missing_or_wrong_typed_enabled() {
        // 契约不符就报错，不猜 —— 猜错等于替中心做决定。
        assert!(parse_state(br#"{"revision":1}"#).is_err());
        assert!(parse_state(br#"{"enabled":"true"}"#).is_err());
        assert!(parse_state(b"not json").is_err());
    }
}
