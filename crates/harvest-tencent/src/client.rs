//! 云 API 调用：签名 + 限速 + 错误分类 + 重试。
//!
//! 三条纪律：
//!
//! 1. **HTTP 200 里的 `Response.Error` 也是失败**，不能只看状态码。
//! 2. **重试只看 [`ErrorClass::retryable`]**：限流 / 5xx / 网络可重试；
//!    鉴权 / 参数 / 响应超限重试无用，立即失败。
//! 3. **错误体只保留 `Code`/`Message`/`RequestId`** 并裁剪，凭据不进日志。

use std::sync::Arc;
use std::time::Duration;

use harvest_core::config::SecretString;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::ratelimit::RateLimiter;
use harvest_core::redact;
use serde_json::Value;

use crate::tc3;

/// 一个云 API 动作的固定元数据。
#[derive(Debug, Clone, Copy)]
pub struct ApiAction {
    pub name: &'static str,
    pub host: &'static str,
    pub version: &'static str,
    /// TC3 签名用的服务名。
    pub service: &'static str,
}

#[derive(Debug, Clone)]
pub struct RetrySpec {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetrySpec {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub retry: RetrySpec,
    /// 测试钩子：把请求指向同一个 base URL（如 `http://127.0.0.1:PORT`）。
    pub endpoint_override: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_response_bytes: 16 * 1024 * 1024,
            retry: RetrySpec::default(),
            endpoint_override: None,
        }
    }
}

pub struct TencentClient {
    http: reqwest::Client,
    secret_id: SecretString,
    secret_key: SecretString,
    limiter: Arc<RateLimiter>,
    options: ClientOptions,
}

#[derive(Debug, Clone)]
pub struct CallOutcome {
    pub body: Value,
    pub retries: u32,
}

impl TencentClient {
    pub fn new(
        secret_id: SecretString,
        secret_key: SecretString,
        limiter: Arc<RateLimiter>,
        options: ClientOptions,
    ) -> Result<Self, HarvestError> {
        let http = reqwest::Client::builder()
            .timeout(options.timeout)
            .build()
            .map_err(|err| HarvestError::other(format!("构建 HTTP 客户端失败: {err}")))?;
        Ok(Self {
            http,
            secret_id,
            secret_key,
            limiter,
            options,
        })
    }

    /// 调用一个 action，返回 `Response` 对象（已剥掉外层 `Response` 键）。
    pub async fn call(
        &self,
        action: &ApiAction,
        region: Option<&str>,
        payload: &Value,
    ) -> Result<CallOutcome, HarvestError> {
        let body = serde_json::to_vec(payload)
            .map_err(|err| HarvestError::data_format(format!("请求体序列化失败: {err}")))?;

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.limiter.acquire(action.name).await;

            match self.attempt(action, region, &body).await {
                Ok(body) => {
                    return Ok(CallOutcome {
                        body,
                        retries: attempt - 1,
                    });
                }
                Err(err) if err.retryable() && attempt < self.options.retry.max_attempts => {
                    let delay = self.backoff(attempt);
                    redact::warn(format!(
                        "{} 第 {attempt} 次失败（{}），{:?} 后重试: {err}",
                        action.name, err.class, delay
                    ));
                    tokio::time::sleep(delay).await;
                }
                Err(err) => {
                    return Err(err
                        .with_action(action.name)
                        .with_region(region.unwrap_or("")));
                }
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32 << (attempt.saturating_sub(1).min(6));
        let delay = self.options.retry.base_delay * factor;
        delay.min(self.options.retry.max_delay)
    }

    async fn attempt(
        &self,
        action: &ApiAction,
        region: Option<&str>,
        payload: &[u8],
    ) -> Result<Value, HarvestError> {
        let (url, host) = self.endpoint(action);
        let timestamp = chrono::Utc::now().timestamp();
        let signed = tc3::sign(&tc3::Tc3Request {
            secret_id: self.secret_id.expose(),
            secret_key: self.secret_key.expose(),
            service: action.service,
            host: &host,
            action: action.name,
            timestamp,
            payload,
        });

        let mut request = self
            .http
            .post(&url)
            .header("Content-Type", tc3::CONTENT_TYPE)
            .header("X-TC-Action", action.name)
            .header("X-TC-Version", action.version)
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("Authorization", signed.authorization)
            .body(payload.to_vec());
        if let Some(region) = region {
            request = request.header("X-TC-Region", region);
        }

        let response = request.send().await.map_err(|err| {
            let class = if err.is_timeout() {
                ErrorClass::Timeout
            } else {
                ErrorClass::Network
            };
            HarvestError::new(class, format!("请求失败: {err}"))
        })?;

        let status = response.status();
        if let Some(len) = response.content_length()
            && len as usize > self.options.max_response_bytes
        {
            return Err(HarvestError::new(
                ErrorClass::ResponseTooLarge,
                format!(
                    "响应 {len} 字节超过上限 {}",
                    self.options.max_response_bytes
                ),
            ));
        }

        let bytes = response.bytes().await.map_err(|err| {
            HarvestError::new(ErrorClass::Network, format!("读取响应失败: {err}"))
        })?;
        if bytes.len() > self.options.max_response_bytes {
            return Err(HarvestError::new(
                ErrorClass::ResponseTooLarge,
                format!(
                    "响应 {} 字节超过上限 {}",
                    bytes.len(),
                    self.options.max_response_bytes
                ),
            ));
        }

        if !status.is_success() {
            let class = classify_status(status.as_u16());
            let snippet = redact::trim_cloud_message(&String::from_utf8_lossy(&bytes), 512);
            return Err(HarvestError::new(
                class,
                format!("HTTP {}: {snippet}", status.as_u16()),
            ));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|err| HarvestError::data_format(format!("响应不是合法 JSON: {err}")))?;

        let response = parsed
            .get("Response")
            .ok_or_else(|| HarvestError::data_format("响应缺少 Response 字段"))?;

        // HTTP 200 也可能是失败：云 API 把错误放在 Response.Error 里。
        if let Some(error) = response.get("Error") {
            let code = error
                .get("Code")
                .and_then(|v| v.as_str())
                .unwrap_or("UnknownError")
                .to_string();
            let message = redact::trim_cloud_message(
                error.get("Message").and_then(|v| v.as_str()).unwrap_or(""),
                512,
            );
            let request_id = response
                .get("RequestId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return Err(
                HarvestError::new(classify_api_error(&code), format!("{code}: {message}"))
                    .with_code(code)
                    .with_request_id(request_id),
            );
        }

        Ok(response.clone())
    }

    /// 实际请求的 URL 与签名用的 host。
    fn endpoint(&self, action: &ApiAction) -> (String, String) {
        match &self.options.endpoint_override {
            Some(base) => {
                let base = base.trim_end_matches('/');
                let authority = base
                    .split_once("://")
                    .map(|(_, rest)| rest)
                    .unwrap_or(base)
                    .split('/')
                    .next()
                    .unwrap_or(base)
                    .to_string();
                (format!("{base}/"), authority)
            }
            None => (format!("https://{}/", action.host), action.host.to_string()),
        }
    }
}

pub fn classify_status(status: u16) -> ErrorClass {
    match status {
        429 => ErrorClass::RateLimited,
        401 | 403 => ErrorClass::Auth,
        500..=599 => ErrorClass::ServerError,
        413 => ErrorClass::ResponseTooLarge,
        400..=499 => ErrorClass::InvalidParam,
        _ => ErrorClass::Other,
    }
}

pub fn classify_api_error(code: &str) -> ErrorClass {
    if code.starts_with("RequestLimitExceeded") || code == "TooManyRequests" {
        return ErrorClass::RateLimited;
    }
    if code.starts_with("InternalError")
        || code.starts_with("ServiceUnavailable")
        || code.starts_with("ResourceUnavailable")
    {
        return ErrorClass::ServerError;
    }
    if code.starts_with("AuthFailure")
        || code.starts_with("UnauthorizedOperation")
        || code.starts_with("InvalidCredential")
        || code.contains("TokenExpired")
    {
        return ErrorClass::Auth;
    }
    if code.contains("InvalidParameter")
        || code.starts_with("MissingParameter")
        || code.starts_with("LimitExceeded")
    {
        return ErrorClass::InvalidParam;
    }
    ErrorClass::Other
}

/// 从响应里取 `RequestId`（排障用，不含凭据）。
pub fn request_id(response: &Value) -> Option<String> {
    response
        .get("RequestId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_classification() {
        assert_eq!(
            classify_api_error("RequestLimitExceeded"),
            ErrorClass::RateLimited
        );
        assert_eq!(
            classify_api_error("InternalError.System"),
            ErrorClass::ServerError
        );
        assert_eq!(
            classify_api_error("AuthFailure.SignatureExpire"),
            ErrorClass::Auth
        );
        assert_eq!(
            classify_api_error("InvalidParameterValue"),
            ErrorClass::InvalidParam
        );
        assert_eq!(classify_api_error("SomethingNew"), ErrorClass::Other);
        assert!(!classify_api_error("AuthFailure.SecretIdNotFound").retryable());
        assert!(classify_api_error("RequestLimitExceeded").retryable());
    }

    #[test]
    fn http_status_classification() {
        assert_eq!(classify_status(429), ErrorClass::RateLimited);
        assert_eq!(classify_status(503), ErrorClass::ServerError);
        assert_eq!(classify_status(403), ErrorClass::Auth);
        assert_eq!(classify_status(400), ErrorClass::InvalidParam);
    }

    #[test]
    fn endpoint_override_keeps_authority() {
        let client = TencentClient::new(
            SecretString::new("id"),
            SecretString::new("key"),
            Arc::new(RateLimiter::new(
                harvest_core::ratelimit::RateSpec::default(),
                Default::default(),
            )),
            ClientOptions {
                endpoint_override: Some("http://127.0.0.1:9999".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let (url, host) = client.endpoint(&ApiAction {
            name: "DescribeInstances",
            host: "cvm.tencentcloudapi.com",
            version: "2017-03-12",
            service: "cvm",
        });
        assert_eq!(url, "http://127.0.0.1:9999/");
        assert_eq!(host, "127.0.0.1:9999");
    }
}
