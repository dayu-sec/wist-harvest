//! 云 API 调用：RPC 签名 + 限速 + 错误分类 + 重试。
//!
//! 与腾讯云客户端的纪律一致：
//!
//! 1. **HTTP 200 里的业务失败也是失败**（BSS OpenAPI 用 `Success=false` 表达）；
//! 2. **重试只看 [`ErrorClass::retryable`]**：限流 / 5xx / 网络可重试；
//!    鉴权 / 参数 / 响应超限重试无用，立即失败；
//! 3. **错误体只保留 `Code`/`Message`/`RequestId`** 并裁剪，凭据不进日志。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use harvest_core::config::SecretString;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::ratelimit::RateLimiter;
use harvest_core::redact;
use serde_json::Value;

use crate::rpc;

/// 一个 RPC 动作的固定元数据。
#[derive(Debug, Clone, Copy)]
pub struct ApiAction {
    pub name: &'static str,
    pub host: &'static str,
    pub version: &'static str,
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

pub struct AliyunClient {
    http: reqwest::Client,
    access_key_id: SecretString,
    access_key_secret: SecretString,
    limiter: Arc<RateLimiter>,
    options: ClientOptions,
}

#[derive(Debug, Clone)]
pub struct CallOutcome {
    pub body: Value,
    pub retries: u32,
}

impl AliyunClient {
    pub fn new(
        access_key_id: SecretString,
        access_key_secret: SecretString,
        limiter: Arc<RateLimiter>,
        options: ClientOptions,
    ) -> Result<Self, HarvestError> {
        let http = reqwest::Client::builder()
            .timeout(options.timeout)
            .build()
            .map_err(|err| HarvestError::other(format!("构建 HTTP 客户端失败: {err}")))?;
        Ok(Self {
            http,
            access_key_id,
            access_key_secret,
            limiter,
            options,
        })
    }

    /// 调用一个 RPC 动作，返回完整响应体。
    ///
    /// `region` 会以 `RegionId` 公共参数发出去；全局接口（账单 / STS）传 `None`。
    pub async fn call(
        &self,
        action: &ApiAction,
        region: Option<&str>,
        params: &[(&str, String)],
    ) -> Result<CallOutcome, HarvestError> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.limiter.acquire(action.name).await;

            match self.attempt(action, region, params).await {
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
        params: &[(&str, String)],
    ) -> Result<Value, HarvestError> {
        let mut query: BTreeMap<String, String> = BTreeMap::new();
        query.insert("Action".into(), action.name.into());
        query.insert("Version".into(), action.version.into());
        query.insert("Format".into(), "JSON".into());
        query.insert(
            "AccessKeyId".into(),
            self.access_key_id.expose().to_string(),
        );
        query.insert("SignatureMethod".into(), rpc::SIGNATURE_METHOD.into());
        query.insert("SignatureVersion".into(), rpc::SIGNATURE_VERSION.into());
        query.insert("SignatureNonce".into(), nonce());
        query.insert(
            "Timestamp".into(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
        if let Some(region) = region {
            query.insert("RegionId".into(), region.to_string());
        }
        for (key, value) in params {
            query.insert((*key).to_string(), value.clone());
        }

        let signed = rpc::signed_query(self.access_key_secret.expose(), &query);
        let url = self.endpoint(action, &signed);

        let response = self.http.get(&url).send().await.map_err(|err| {
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
            return Err(api_error_from_body(
                &bytes,
                classify_status(status.as_u16()),
            ));
        }

        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|err| HarvestError::data_format(format!("响应不是合法 JSON: {err}")))?;

        // BSS OpenAPI 等接口用 HTTP 200 + `Success=false` 表达失败。
        if parsed.get("Success").and_then(Value::as_bool) == Some(false) {
            return Err(api_error_from_value(&parsed));
        }

        Ok(parsed)
    }

    /// 实际请求的 URL。
    fn endpoint(&self, action: &ApiAction, signed_query: &str) -> String {
        match &self.options.endpoint_override {
            Some(base) => format!("{}/?{signed_query}", base.trim_end_matches('/')),
            None => format!("https://{}/?{signed_query}", action.host),
        }
    }
}

fn nonce() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{sequence:x}")
}

fn api_error_from_body(bytes: &[u8], fallback: ErrorClass) -> HarvestError {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => {
            let mut err = api_error_from_value(&value);
            if err.class == ErrorClass::Other {
                err.class = fallback;
            }
            err
        }
        Err(_) => {
            let snippet = redact::trim_cloud_message(&String::from_utf8_lossy(bytes), 512);
            HarvestError::new(fallback, format!("HTTP 错误: {snippet}"))
        }
    }
}

fn api_error_from_value(value: &Value) -> HarvestError {
    let code = value
        .get("Code")
        .and_then(Value::as_str)
        .unwrap_or("UnknownError")
        .to_string();
    let message = redact::trim_cloud_message(
        value.get("Message").and_then(Value::as_str).unwrap_or(""),
        512,
    );
    let request_id = value
        .get("RequestId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    HarvestError::new(classify_code(&code), format!("{code}: {message}"))
        .with_code(code)
        .with_request_id(request_id)
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

pub fn classify_code(code: &str) -> ErrorClass {
    if code.starts_with("Throttling") || code.ends_with("Throttling") {
        return ErrorClass::RateLimited;
    }
    if code.starts_with("InternalError")
        || code.starts_with("ServiceUnavailable")
        || code.starts_with("ServiceBusy")
    {
        return ErrorClass::ServerError;
    }
    if code.starts_with("InvalidAccessKeyId")
        || code.starts_with("SignatureDoesNotMatch")
        || code.starts_with("IncompleteSignature")
        || code.starts_with("InvalidSecurityToken")
        || code.starts_with("Forbidden")
        || code.starts_with("NoPermission")
        || code.starts_with("Unauthorized")
        || code.starts_with("NotAuthorized")
    {
        return ErrorClass::Auth;
    }
    if code.starts_with("InvalidParameter")
        || code.starts_with("MissingParameter")
        || code.starts_with("InvalidAction")
        || code.starts_with("InvalidVersion")
        || code.starts_with("InvalidRegionId")
        || code.starts_with("InvalidTimeStamp")
    {
        return ErrorClass::InvalidParam;
    }
    ErrorClass::Other
}

/// 从响应里取 `RequestId`（排障用，不含凭据）。
pub fn request_id(response: &Value) -> Option<String> {
    response
        .get("RequestId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_classification() {
        assert_eq!(classify_code("Throttling.User"), ErrorClass::RateLimited);
        assert_eq!(classify_code("ServiceUnavailable"), ErrorClass::ServerError);
        assert_eq!(
            classify_code("InvalidAccessKeyId.NotFound"),
            ErrorClass::Auth
        );
        assert_eq!(classify_code("SignatureDoesNotMatch"), ErrorClass::Auth);
        assert_eq!(classify_code("Forbidden.RAM"), ErrorClass::Auth);
        assert_eq!(classify_code("NoPermission.Action"), ErrorClass::Auth);
        assert_eq!(
            classify_code("InvalidParameterValue"),
            ErrorClass::InvalidParam
        );
        assert_eq!(classify_code("SomethingNew"), ErrorClass::Other);
        assert!(!classify_code("InvalidAccessKeyId.NotFound").retryable());
        assert!(classify_code("Throttling.User").retryable());
    }

    #[test]
    fn http_status_classification() {
        assert_eq!(classify_status(429), ErrorClass::RateLimited);
        assert_eq!(classify_status(503), ErrorClass::ServerError);
        assert_eq!(classify_status(403), ErrorClass::Auth);
        assert_eq!(classify_status(400), ErrorClass::InvalidParam);
    }

    #[test]
    fn endpoint_override_keeps_query_string() {
        let client = AliyunClient::new(
            SecretString::new("id"),
            SecretString::new("secret"),
            Arc::new(RateLimiter::new(
                harvest_core::ratelimit::RateSpec::default(),
                Default::default(),
            )),
            ClientOptions {
                endpoint_override: Some("http://127.0.0.1:9999/".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let url = client.endpoint(
            &ApiAction {
                name: "DescribeInstances",
                host: "ecs.aliyuncs.com",
                version: "2014-05-26",
            },
            "Action=DescribeInstances",
        );
        assert_eq!(url, "http://127.0.0.1:9999/?Action=DescribeInstances");
    }
}
