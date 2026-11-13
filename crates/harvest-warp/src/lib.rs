//! 数据平面 sink：TLS（可选 mTLS）+ 长度前缀帧。
//!
//! 只用一条规则：**`publish_snapshot` 是唯一出口**，且只在 `Verified` 之后被调用。
//! `Aborted` 路径根本不会走到这里，所以"零帧"由结构保证，不靠调用方记性。

pub mod frame;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use harvest_core::config::SinkSettings;
use harvest_core::error::{ErrorClass, HarvestError};
use harvest_core::redact;
use harvest_core::source::{CompleteSnapshot, PublishAck, Sink};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// ACK 帧的读取上限：ACK 是很小的 JSON。
const MAX_ACK_BYTES: usize = 1024 * 1024;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct WarpSink {
    endpoint: String,
    plaintext: bool,
    connector: Option<(TlsConnector, ServerName<'static>)>,
    connect_timeout: Duration,
    ack_timeout: Duration,
    expect_ack: bool,
    attempts: u32,
}

impl WarpSink {
    pub fn from_settings(settings: &SinkSettings) -> Result<Self, HarvestError> {
        if settings.endpoint.trim().is_empty() {
            return Err(HarvestError::new(
                ErrorClass::InvalidParam,
                "sink.endpoint 未配置",
            ));
        }
        let (host, _port) = split_endpoint(&settings.endpoint)?;

        let connector = if settings.allow_plaintext {
            None
        } else {
            let config = build_tls_config(settings)?;
            let server_name = ServerName::try_from(host.to_string()).map_err(|err| {
                HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("sink.endpoint 主机名不可用作 TLS ServerName: {err}"),
                )
            })?;
            Some((TlsConnector::from(Arc::new(config)), server_name))
        };

        Ok(Self {
            endpoint: settings.endpoint.clone(),
            plaintext: settings.allow_plaintext,
            connector,
            connect_timeout: Duration::from_millis(settings.connect_timeout_ms),
            ack_timeout: Duration::from_millis(settings.ack_timeout_ms),
            expect_ack: settings.expect_ack,
            attempts: settings.publish_attempts.max(1),
        })
    }

    async fn connect(&self) -> Result<Box<dyn Io>, HarvestError> {
        let tcp = tokio::time::timeout(self.connect_timeout, TcpStream::connect(&self.endpoint))
            .await
            .map_err(|_| {
                HarvestError::new(ErrorClass::Timeout, format!("连接 {} 超时", self.endpoint))
            })?
            .map_err(|err| {
                HarvestError::new(
                    ErrorClass::Network,
                    format!("连接 {} 失败: {err}", self.endpoint),
                )
            })?;
        tcp.set_nodelay(true).ok();

        if self.plaintext {
            return Ok(Box::new(tcp));
        }
        let (connector, server_name) = self
            .connector
            .as_ref()
            .expect("非明文模式必然已构建 TLS connector");
        // 握手也要超时：对端 TCP 通了但 TLS 卡住时，不能把整轮采集挂死在这里。
        let tls: TlsStream<TcpStream> = tokio::time::timeout(
            self.connect_timeout,
            connector.connect(server_name.clone(), tcp),
        )
        .await
        .map_err(|_| HarvestError::new(ErrorClass::Timeout, "TLS 握手超时"))?
        .map_err(|err| HarvestError::new(ErrorClass::Network, format!("TLS 握手失败: {err}")))?;
        Ok(Box::new(tls))
    }

    async fn publish_once(&self, frame_bytes: &[u8]) -> Result<String, HarvestError> {
        let mut stream = self.connect().await?;
        tokio::time::timeout(
            self.ack_timeout,
            frame::write_frame(&mut stream, frame_bytes),
        )
        .await
        .map_err(|_| HarvestError::new(ErrorClass::Timeout, "写入帧超时"))?
        .map_err(|err| HarvestError::new(ErrorClass::Network, err.message))?;

        // 不等应答的 sink：写完即为已送达。
        //
        // 为什么这是默认：warp-parse 的 tcp_src 没有应用层应答，死等 ACK 的结果是
        // 「帧已到、运行却记 Aborted、spool 永不清理」。
        if !self.expect_ack {
            return Ok("none".to_string());
        }

        let ack = tokio::time::timeout(
            self.ack_timeout,
            frame::read_frame(&mut stream, MAX_ACK_BYTES),
        )
        .await
        // 超时 = 未确认，按"可重试"处理，绝不当作已发布。
        .map_err(|_| HarvestError::new(ErrorClass::Timeout, "等待数据平面 ACK 超时"))?
        .map_err(|err| HarvestError::new(ErrorClass::Network, err.message))?;
        parse_ack(&ack)
    }
}

fn parse_ack(bytes: &[u8]) -> Result<String, HarvestError> {
    let parsed: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|err| HarvestError::data_format(format!("ACK 不是合法 JSON: {err}")))?;
    let status = parsed
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match status.as_deref() {
        Some("ok") => Ok("ok".to_string()),
        Some(other) => Err(HarvestError::new(
            ErrorClass::ServerError,
            format!(
                "数据平面拒绝接收（status={other}）: {}",
                redact::trim_cloud_message(&String::from_utf8_lossy(bytes), 256)
            ),
        )),
        None => {
            if parsed.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                Ok("ok".to_string())
            } else {
                Err(HarvestError::data_format(
                    "ACK 缺少 status 或 ok 字段（无法确认已接收）",
                ))
            }
        }
    }
}

fn split_endpoint(endpoint: &str) -> Result<(&str, &str), HarvestError> {
    match endpoint.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => Ok((host, port)),
        _ => Err(HarvestError::new(
            ErrorClass::InvalidParam,
            format!("sink.endpoint 需要 host:port 形式，实际是 {endpoint}"),
        )),
    }
}

/// 只做形态校验的端点解析（不联网）。
///
/// `diagnose` 与 sink 共用同一口径：容器 healthcheck 不能因为数据平面忙就重启采集器。
pub fn parse_endpoint(endpoint: &str) -> Result<(String, String), String> {
    split_endpoint(endpoint)
        .map(|(host, port)| (host.to_string(), port.to_string()))
        .map_err(|err| err.message)
}

fn build_tls_config(settings: &SinkSettings) -> Result<rustls::ClientConfig, HarvestError> {
    let mut roots = rustls::RootCertStore::empty();
    match &settings.ca_file {
        Some(path) => {
            let mut added = 0usize;
            for cert in CertificateDer::pem_file_iter(path).map_err(|err| {
                HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("读取 CA 文件 {} 失败: {err}", path.display()),
                )
            })? {
                let cert = cert.map_err(|err| {
                    HarvestError::new(ErrorClass::InvalidParam, format!("CA 证书解析失败: {err}"))
                })?;
                roots.add(cert).map_err(|err| {
                    HarvestError::new(ErrorClass::InvalidParam, format!("CA 证书不可用: {err}"))
                })?;
                added += 1;
            }
            if added == 0 {
                return Err(HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("CA 文件 {} 里没有证书", path.display()),
                ));
            }
        }
        None => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }

    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = match (&settings.cert_file, &settings.key_file) {
        (Some(cert_path), Some(key_path)) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
                .map_err(|err| {
                    HarvestError::new(
                        ErrorClass::InvalidParam,
                        format!("读取客户端证书 {} 失败: {err}", cert_path.display()),
                    )
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| {
                    HarvestError::new(
                        ErrorClass::InvalidParam,
                        format!("客户端证书解析失败: {err}"),
                    )
                })?;
            let key = PrivateKeyDer::from_pem_file(key_path).map_err(|err| {
                HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("读取客户端私钥 {} 失败: {err}", key_path.display()),
                )
            })?;
            builder.with_client_auth_cert(certs, key).map_err(|err| {
                HarvestError::new(
                    ErrorClass::InvalidParam,
                    format!("客户端证书与私钥不匹配: {err}"),
                )
            })?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => {
            return Err(HarvestError::new(
                ErrorClass::InvalidParam,
                "sink.cert_file 与 sink.key_file 必须同时配置或同时留空",
            ));
        }
    };
    Ok(config)
}

#[async_trait]
impl Sink for WarpSink {
    async fn publish_snapshot(&self, snap: &CompleteSnapshot) -> Result<PublishAck, HarvestError> {
        let started = std::time::Instant::now();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self.publish_once(&snap.bytes).await {
                Ok(ack) => {
                    return Ok(PublishAck {
                        bytes: snap.bytes.len() as u64,
                        ack,
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                }
                Err(err) if err.retryable() && attempt < self.attempts => {
                    let delay = Duration::from_millis(200 * u64::from(attempt));
                    redact::warn(format!(
                        "发布第 {attempt} 次失败（{}），{:?} 后重试: {err}",
                        err.class, delay
                    ));
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_parsing() {
        assert_eq!(parse_ack(br#"{"status":"ok"}"#).unwrap(), "ok");
        assert_eq!(parse_ack(br#"{"ok":true}"#).unwrap(), "ok");
        assert!(parse_ack(br#"{"status":"error"}"#).is_err());
        assert!(parse_ack(b"{}").is_err());
        assert!(parse_ack(b"not json").is_err());
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            split_endpoint("warp-parse:9100").unwrap(),
            ("warp-parse", "9100")
        );
        assert!(split_endpoint("no-port").is_err());
    }

    #[test]
    fn plaintext_requires_explicit_flag() {
        let settings = SinkSettings {
            endpoint: "127.0.0.1:1".to_string(),
            ..Default::default()
        };
        let sink = WarpSink::from_settings(&settings).expect("非明文也应在构建期成功");
        assert!(sink.connector.is_some(), "默认走 TLS");

        let plain = WarpSink::from_settings(&SinkSettings {
            allow_plaintext: true,
            ..settings
        })
        .unwrap();
        assert!(plain.connector.is_none());
    }
}
