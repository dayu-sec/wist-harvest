//! 统一错误分类：既决定"能不能重试"，也决定运行报告里怎么记账。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 错误类别。重试策略只看 [`ErrorClass::retryable`]，不看到处散落的字符串匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// 云端限流（含 HTTP 429、`RequestLimitExceeded`）。
    RateLimited,
    /// 云/网关 5xx。
    ServerError,
    /// 建连、读超时之外的传输错误。
    Network,
    /// 请求超时。
    Timeout,
    /// 鉴权失败（凭据错误、无权限）。
    Auth,
    /// 参数/配额问题，重试无用。
    InvalidParam,
    /// 响应体超过上限。
    ResponseTooLarge,
    /// 响应结构不符合预期（字段缺失、类型不对）。
    DataFormat,
    /// 数据自身不完整（总数不一致、重复 ID、地域缺失）。
    Integrity,
    /// 其他。
    Other,
}

impl ErrorClass {
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::ServerError | Self::Network | Self::Timeout
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Auth => "auth",
            Self::InvalidParam => "invalid_param",
            Self::ResponseTooLarge => "response_too_large",
            Self::DataFormat => "data_format",
            Self::Integrity => "integrity",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 本服务里跨模块传递的唯一错误类型。
///
/// `message` 必须是可以直接落盘/落日志的文本：调用方负责在构造时或落盘前
/// 用 [`crate::redact::Redactor`] 擦除凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestError {
    pub class: ErrorClass,
    pub message: String,
    // 这四个是排障元数据，用 `Box<str>` 而不是 `String`：
    // 错误类型要在所有 `Result` 里传递，体积必须小（否则 `result_large_err`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<Box<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Box<str>>,
    /// 云 API `RequestId`，排障用，不含凭据。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Box<str>>,
    /// 云 API 错误码，例如 `RequestLimitExceeded`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Box<str>>,
}

impl HarvestError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            action: None,
            region: None,
            request_id: None,
            code: None,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Other, message)
    }

    pub fn data_format(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::DataFormat, message)
    }

    pub fn integrity(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Integrity, message)
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into().into_boxed_str());
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into().into_boxed_str());
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        let id = request_id.into();
        if !id.is_empty() {
            self.request_id = Some(id.into_boxed_str());
        }
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into().into_boxed_str());
        self
    }

    pub fn retryable(&self) -> bool {
        self.class.retryable()
    }

    /// 擦除可能混进来的凭据。所有对外暴露错误前都应调用一次。
    pub fn scrubbed(mut self, redactor: &crate::redact::Redactor) -> Self {
        self.message = redactor.scrub(&self.message);
        self.code = self.code.map(|code| redactor.scrub(&code).into_boxed_str());
        self
    }
}

impl fmt::Display for HarvestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.class, self.message)?;
        if let Some(action) = &self.action {
            write!(f, " (action={action})")?;
        }
        if let Some(region) = &self.region {
            write!(f, " (region={region})")?;
        }
        if let Some(code) = &self.code {
            write!(f, " (code={code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for HarvestError {}

impl From<std::io::Error> for HarvestError {
    fn from(err: std::io::Error) -> Self {
        let class = match err.kind() {
            std::io::ErrorKind::TimedOut => ErrorClass::Timeout,
            _ => ErrorClass::Network,
        };
        Self::new(class, err.to_string())
    }
}

impl From<crate::spool::SpoolError> for HarvestError {
    fn from(err: crate::spool::SpoolError) -> Self {
        Self::new(ErrorClass::Other, err.to_string())
    }
}

impl From<crate::config::ConfigError> for HarvestError {
    fn from(err: crate::config::ConfigError) -> Self {
        Self::new(ErrorClass::InvalidParam, err.to_string())
    }
}

impl From<crate::lease::LeaseError> for HarvestError {
    fn from(err: crate::lease::LeaseError) -> Self {
        Self::new(ErrorClass::Other, err.to_string())
    }
}
