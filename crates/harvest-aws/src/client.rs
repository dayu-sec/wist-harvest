//! AWS API 调用：SigV4 签名 + 限速 + 错误分类 + 重试。
//!
//! 三条与腾讯云一致的纪律：
//!
//! 1. **HTTP 200 也可能带错误**（这里少见，但错误体必须识别）；
//! 2. **重试只看 `ErrorClass::retryable`**：限流/5xx/网络可重试，鉴权/参数立即失败；
//! 3. **错误体只保留 `Code`/`Message`/`RequestId`**，凭据不进日志。
//!
//! 协议差异都在这里吸收：AWS 用 `Action=...&Version=...` 的表单 POST，响应是 XML。

use std::sync::Arc;
use std::time::Duration;

use harvest_core::config::SecretString;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::ratelimit::RateLimiter;
use harvest_core::redact;
use serde::Deserialize;

use crate::sigv4;

/// 一个 AWS API 动作的固定元数据。
#[derive(Debug, Clone, Copy)]
pub struct ApiAction {
    pub name: &'static str,
    /// 服务名，拼域名与签名 scope：`ec2` / `sts`。
    pub service: &'static str,
    pub version: &'static str,
}

pub const DESCRIBE_REGIONS: ApiAction = ApiAction {
    name: "DescribeRegions",
    service: "ec2",
    version: "2016-11-15",
};

pub const DESCRIBE_INSTANCES: ApiAction = ApiAction {
    name: "DescribeInstances",
    service: "ec2",
    version: "2016-11-15",
};

pub const DESCRIBE_ADDRESSES: ApiAction = ApiAction {
    name: "DescribeAddresses",
    service: "ec2",
    version: "2016-11-15",
};

pub const DESCRIBE_VPCS: ApiAction = ApiAction {
    name: "DescribeVpcs",
    service: "ec2",
    version: "2016-11-15",
};

pub const DESCRIBE_SUBNETS: ApiAction = ApiAction {
    name: "DescribeSubnets",
    service: "ec2",
    version: "2016-11-15",
};

pub const GET_CALLER_IDENTITY: ApiAction = ApiAction {
    name: "GetCallerIdentity",
    service: "sts",
    version: "2011-06-15",
};

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
    /// AWS 分区：`aws` / `aws-cn` / `aws-us-gov`。决定域名后缀。
    pub partition: String,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_response_bytes: 16 * 1024 * 1024,
            retry: RetrySpec::default(),
            endpoint_override: None,
            partition: "aws".to_string(),
        }
    }
}

pub struct AwsClient {
    http: reqwest::Client,
    access_key_id: SecretString,
    secret_access_key: SecretString,
    session_token: Option<SecretString>,
    limiter: Arc<RateLimiter>,
    options: ClientOptions,
}

#[derive(Debug, Clone)]
pub struct CallOutcome {
    /// 原始 XML 响应体。
    pub body: String,
    pub retries: u32,
}

impl AwsClient {
    pub fn new(
        access_key_id: SecretString,
        secret_access_key: SecretString,
        session_token: Option<SecretString>,
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
            secret_access_key,
            session_token,
            limiter,
            options,
        })
    }

    /// 调一个 action。`params` 是 Action/Version 之外的参数。
    pub async fn call(
        &self,
        action: &ApiAction,
        region: &str,
        params: &[(String, String)],
    ) -> Result<CallOutcome, HarvestError> {
        let mut all = Vec::with_capacity(params.len() + 2);
        all.push(("Action".to_string(), action.name.to_string()));
        all.push(("Version".to_string(), action.version.to_string()));
        all.extend(params.iter().cloned());
        let payload = sigv4::form_encode(&all);

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.limiter.acquire(action.name).await;
            match self.attempt(action, region, &payload).await {
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
                    return Err(err.with_action(action.name).with_region(region));
                }
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32 << (attempt.saturating_sub(1).min(6));
        (self.options.retry.base_delay * factor).min(self.options.retry.max_delay)
    }

    async fn attempt(
        &self,
        action: &ApiAction,
        region: &str,
        payload: &str,
    ) -> Result<String, HarvestError> {
        let (url, host) = self.endpoint(action, region);
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let signed = sigv4::sign(&sigv4::SigV4Request {
            access_key_id: self.access_key_id.expose(),
            secret_access_key: self.secret_access_key.expose(),
            session_token: self.session_token.as_ref().map(SecretString::expose),
            region,
            service: action.service,
            host: &host,
            amz_date: &amz_date,
            date: &date,
            payload: payload.as_bytes(),
        });

        let mut request = self
            .http
            .post(&url)
            .header("Content-Type", sigv4::CONTENT_TYPE)
            .header("X-Amz-Date", &amz_date)
            .header("Authorization", signed.authorization)
            .body(payload.to_string());
        if let Some(token) = &self.session_token {
            request = request.header("X-Amz-Security-Token", token.expose());
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
        let body = String::from_utf8_lossy(&bytes).to_string();

        if !status.is_success() {
            let parsed = parse_error(&body);
            let (code, message, request_id) = match parsed {
                Some(err) => (err.code, err.message, err.request_id),
                None => (String::new(), truncate(&body), None),
            };
            let class = if code.is_empty() {
                classify_status(status.as_u16())
            } else {
                classify_api_error(&code)
            };
            let detail = if message.is_empty() {
                format!("HTTP {}（{}）", status.as_u16(), code)
            } else {
                format!("{code}: {message}")
            };
            let error = HarvestError::new(class, detail).with_code(code);
            return Err(match request_id {
                Some(id) => error.with_request_id(id),
                None => error,
            });
        }

        // 200 也可能带错误（少数接口）。
        if let Some(err) = parse_error(&body) {
            return Err(HarvestError::new(
                classify_api_error(&err.code),
                format!("{}: {}", err.code, err.message),
            )
            .with_code(err.code)
            .with_request_id(err.request_id.unwrap_or_default()));
        }

        Ok(body)
    }

    /// 实际请求的 URL 与签名用的 host。
    fn endpoint(&self, action: &ApiAction, region: &str) -> (String, String) {
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
            None => {
                let suffix = match self.options.partition.as_str() {
                    "aws-cn" => "amazonaws.com.cn",
                    _ => "amazonaws.com",
                };
                let host = format!("{}.{}.{}", action.service, region, suffix);
                (format!("https://{host}/"), host)
            }
        }
    }
}

struct ParsedError {
    code: String,
    message: String,
    request_id: Option<String>,
}

/// 两种错误 XML：EC2 的 `<Response><Errors><Error>`，STS 的 `<ErrorResponse><Error>`。
#[derive(Deserialize)]
struct Ec2ErrorEnvelope {
    #[serde(rename = "Errors", default)]
    errors: Ec2ErrorList,
    #[serde(rename = "RequestID", default)]
    request_id: Option<String>,
}

#[derive(Default, Deserialize)]
struct Ec2ErrorList {
    #[serde(rename = "Error", default)]
    error: Vec<Ec2ErrorItem>,
}

#[derive(Deserialize)]
struct Ec2ErrorItem {
    #[serde(rename = "Code", default)]
    code: Option<String>,
    #[serde(rename = "Message", default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct StsErrorEnvelope {
    #[serde(rename = "Error", default)]
    error: Option<StsErrorItem>,
    #[serde(rename = "RequestId", default)]
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct StsErrorItem {
    #[serde(rename = "Code", default)]
    code: Option<String>,
    #[serde(rename = "Message", default)]
    message: Option<String>,
}

fn parse_error(body: &str) -> Option<ParsedError> {
    if let Ok(envelope) = quick_xml::de::from_str::<Ec2ErrorEnvelope>(body)
        && let Some(item) = envelope.errors.error.into_iter().next()
    {
        return Some(ParsedError {
            code: item.code.unwrap_or_default(),
            message: truncate(&item.message.unwrap_or_default()),
            request_id: envelope.request_id,
        });
    }
    if let Ok(envelope) = quick_xml::de::from_str::<StsErrorEnvelope>(body)
        && let Some(item) = envelope.error
    {
        return Some(ParsedError {
            code: item.code.unwrap_or_default(),
            message: truncate(&item.message.unwrap_or_default()),
            request_id: envelope.request_id,
        });
    }
    None
}

fn truncate(text: &str) -> String {
    redact::trim_cloud_message(text, 512)
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
    let code = code.trim();
    if code.starts_with("Throttl")
        || code == "RequestLimitExceeded"
        || code == "TooManyRequests"
        || code == "SlowDown"
    {
        return ErrorClass::RateLimited;
    }
    if code.starts_with("InternalError")
        || code.starts_with("InternalFailure")
        || code.starts_with("ServiceUnavailable")
        || code == "Unavailable"
    {
        return ErrorClass::ServerError;
    }
    if code.starts_with("AuthFailure")
        || code == "InvalidClientTokenId"
        || code == "SignatureDoesNotMatch"
        || code == "RequestExpired"
        || code == "UnauthorizedOperation"
        || code == "AccessDenied"
        || code == "OptInRequired"
    {
        return ErrorClass::Auth;
    }
    if code.starts_with("InvalidParameter")
        || code.starts_with("MissingParameter")
        || code == "InvalidAction"
        || code == "ValidationError"
    {
        return ErrorClass::InvalidParam;
    }
    ErrorClass::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_classification() {
        assert_eq!(classify_api_error("Throttling"), ErrorClass::RateLimited);
        assert_eq!(
            classify_api_error("RequestLimitExceeded"),
            ErrorClass::RateLimited
        );
        assert_eq!(classify_api_error("InternalError"), ErrorClass::ServerError);
        assert_eq!(classify_api_error("InvalidClientTokenId"), ErrorClass::Auth);
        assert_eq!(
            classify_api_error("UnauthorizedOperation"),
            ErrorClass::Auth
        );
        assert_eq!(
            classify_api_error("InvalidParameterValue"),
            ErrorClass::InvalidParam
        );
        assert!(!classify_api_error("InvalidClientTokenId").retryable());
        assert!(classify_api_error("Throttling").retryable());
    }

    #[test]
    fn endpoint_follows_the_partition() {
        let client = AwsClient::new(
            SecretString::new("id"),
            SecretString::new("key"),
            None,
            Arc::new(RateLimiter::new(
                harvest_core::ratelimit::RateSpec::default(),
                Default::default(),
            )),
            ClientOptions::default(),
        )
        .unwrap();
        let (url, host) = client.endpoint(&DESCRIBE_INSTANCES, "us-east-1");
        assert_eq!(url, "https://ec2.us-east-1.amazonaws.com/");
        assert_eq!(host, "ec2.us-east-1.amazonaws.com");

        let china = AwsClient::new(
            SecretString::new("id"),
            SecretString::new("key"),
            None,
            Arc::new(RateLimiter::new(
                harvest_core::ratelimit::RateSpec::default(),
                Default::default(),
            )),
            ClientOptions {
                partition: "aws-cn".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let (_, host) = china.endpoint(&DESCRIBE_VPCS, "cn-north-1");
        assert_eq!(host, "ec2.cn-north-1.amazonaws.com.cn");
    }

    #[test]
    fn ec2_and_sts_error_bodies_are_recognized() {
        let ec2 = r#"<Response><Errors><Error><Code>OptInRequired</Code><Message>not opted in</Message></Error></Errors><RequestID>req-1</RequestID></Response>"#;
        let err = parse_error(ec2).unwrap();
        assert_eq!(err.code, "OptInRequired");
        assert_eq!(err.request_id.as_deref(), Some("req-1"));

        let sts = r#"<ErrorResponse><Error><Type>Sender</Type><Code>InvalidClientTokenId</Code><Message>bad token</Message></Error><RequestId>req-2</RequestId></ErrorResponse>"#;
        let err = parse_error(sts).unwrap();
        assert_eq!(err.code, "InvalidClientTokenId");
        assert_eq!(err.request_id.as_deref(), Some("req-2"));

        assert!(parse_error("<DescribeInstancesResponse></DescribeInstancesResponse>").is_none());
    }
}
