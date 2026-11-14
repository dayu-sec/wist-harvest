//! 网关工作授权客户端（`work:poll` / `work:ack`）+ 接入与续期（CSR → 客户端证书）。
//!
//! 与 agentd 同一套纪律：
//! - **私钥永不上送**：本地生成密钥对，只把 CSR 交上去；主体（URI SAN）由网关填；
//! - **不存共享开关**：采什么、什么节奏由网关的 standing 决定；
//! - **云账号凭据不经网关**：`spec.credential_ref` 只是引用名。
//!
//! 身份落在 `<identity_dir>/client.{crt,key}`（0600）。没有证书时用一次性接入券换一张，
//! 临近到期（`RENEW_WINDOW`）时用旧证书续期 —— 两者都是**后台动作**：
//! [`GatewayConnector::connect`] 失败不会让进程退出，由调用方退避重试。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use harvest_core::config::GatewaySettings;
use harvest_core::redact;
use wist_api::harvest_work::{
    AckHarvestWork, HarvestCertificateRenewal, HarvestCertificateRenewed,
    HarvestEnrollmentEnvelope, HarvestEnrollmentRequest, HarvestEnrollmentStatus,
    HarvestResultAccepted, HarvestWorkGrant, PollHarvestWork, ReportHarvestResult,
};

/// 一次接入 / 轮询用的超时；网关在栈内网，慢到这个程度就是它自己的问题。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 到期前多久开始续期。网关签发 37 天，留 7 天足够跨过一次网关升级窗口。
const RENEW_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

/// 网关客户端：持有 mTLS 身份与端点信息。
pub struct GatewayClient {
    http: reqwest::Client,
    endpoint: String,
    worker_id: String,
    source_code: String,
    instance_id: String,
}

impl GatewayClient {
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn source_code(&self) -> &str {
        &self.source_code
    }

    /// 拉一份工作授权快照（幂等）。
    pub async fn poll(&self, last_seen_sequence: i64) -> Result<HarvestWorkGrant, String> {
        let request = PollHarvestWork::new(
            self.worker_id.clone(),
            self.instance_id.clone(),
            last_seen_sequence,
            now_rfc3339(),
        );
        let url = format!("{}/api/v1/harvest/work:poll", self.endpoint);
        let response = self
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|err| format!("请求工作授权失败：{err}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("读取工作授权响应失败：{err}"))?;
        if !status.is_success() {
            return Err(format!(
                "工作授权返回 {}：{}",
                status.as_u16(),
                truncate(&String::from_utf8_lossy(&body), 200)
            ));
        }
        serde_json::from_slice(&body).map_err(|err| format!("工作授权报文不合法：{err}"))
    }

    /// 确认收到某份 standing（`plan_version` 变化后也必须确认）。
    pub async fn ack(&self, work_id: &str, plan_version: i64) -> Result<(), String> {
        let request = AckHarvestWork::new(
            self.worker_id.clone(),
            self.instance_id.clone(),
            work_id,
            plan_version,
            now_rfc3339(),
        );
        let url = format!("{}/api/v1/harvest/work:ack", self.endpoint);
        let response = self
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|err| format!("发送工作确认失败：{err}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("读取工作确认响应失败：{err}"))?;
        if !status.is_success() {
            return Err(format!(
                "工作确认返回 {}：{}",
                status.as_u16(),
                truncate(&String::from_utf8_lossy(&body), 200)
            ));
        }
        Ok(())
    }

    /// 上报一次性工作的执行结果（`running` / `succeeded` / `failed`）。
    ///
    /// 返回 `stale` = 网关已经把这件活终结了（迟到上报，不覆盖）；`unknown` = 网关不认这件活。
    pub async fn report(
        &self,
        work_id: &str,
        status: &str,
        detail: &str,
    ) -> Result<HarvestResultAccepted, String> {
        let request = ReportHarvestResult::new(
            self.worker_id.clone(),
            self.instance_id.clone(),
            work_id,
            status,
            detail,
            now_rfc3339(),
        );
        let url = format!("{}/api/v1/harvest/work:result", self.endpoint);
        let response = self
            .http
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|err| format!("上报工作结果失败：{err}"))?;
        let code = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|err| format!("读取工作结果响应失败：{err}"))?;
        if !code.is_success() {
            return Err(format!(
                "上报工作结果返回 {}：{}",
                code.as_u16(),
                truncate(&String::from_utf8_lossy(&body), 200)
            ));
        }
        serde_json::from_slice(&body).map_err(|err| format!("工作结果报文不合法：{err}"))
    }
}

/// 接入所需的全部输入（与 `run` 同寿）。
///
/// 为什么是“连接器”而不是一个连接：证书可能还没签、接入券可能还没落盘、
/// 网关可能还没起 —— 这些都不该要求人工重启容器。
#[derive(Clone)]
pub struct GatewayConnector {
    settings: GatewaySettings,
    state_dir: PathBuf,
    /// 启动时已知的接入券（配置 / 环境变量）。没有就每次尝试时去 `<state_dir>/enroll.ticket` 找，
    /// 这样 `harvest-bootstrap` 晚一点写入也能生效。
    ticket: Option<String>,
    instance_id: String,
}

impl GatewayConnector {
    pub fn new(settings: &GatewaySettings, state_dir: &Path, ticket: Option<String>) -> Self {
        Self {
            settings: settings.clone(),
            state_dir: state_dir.to_path_buf(),
            ticket,
            instance_id: new_instance_id(),
        }
    }

    pub fn poll_interval(&self) -> Duration {
        self.settings.poll_interval()
    }

    pub fn worker_id(&self) -> String {
        self.settings.worker_id()
    }

    /// 本地是否已有客户端证书（决定 bootstrap 要不要现签一张接入券）。
    pub fn identity_ready(&self) -> bool {
        self.identity_paths().ready()
    }

    /// 身份目录（证书 + 私钥）。`bootstrap` 打日志时用。
    pub fn identity_dir(&self) -> PathBuf {
        self.identity_paths().dir
    }

    /// 建一个可用客户端。**失败只返回错误**（不落任何状态），调用方决定退避节奏。
    pub async fn connect(&self) -> Result<GatewayClient, String> {
        let endpoint = self.settings.endpoint.trim().trim_end_matches('/');
        if endpoint.is_empty() {
            return Err("[gateway].endpoint 为空".to_string());
        }
        let source_code = self.settings.source_code.trim();
        if source_code.is_empty() {
            return Err("[gateway].source_code 为空".to_string());
        }
        let worker_id = self.settings.worker_id();
        let identity = self.identity_paths();

        if !identity.ready() {
            let ticket = self.ticket();
            let ticket = ticket.as_deref().ok_or_else(|| {
                "没有客户端证书，也没有接入券：等同栈 `harvest-bootstrap` 签一张，\
                 或在网关页「签发接入券」后写进宿主 secret 文件"
                    .to_string()
            })?;
            enroll(
                &self.settings,
                endpoint,
                &worker_id,
                source_code,
                &identity,
                ticket,
            )
            .await?;
            // 券已用掉：留在盘上只是多一份凭据。
            let _ = std::fs::remove_file(self.state_dir.join("enroll.ticket"));
        }

        let expiry = certificate_expiry(&identity.cert).map_err(|err| {
            format!(
                "{err}（删掉 {} 后重启，bootstrap 会重新签一张）",
                identity.dir.display()
            )
        })?;
        match expiry {
            Expiry::Expired { not_after } => {
                return Err(format!(
                    "客户端证书已于 {not_after} 过期：删掉 {} 后重启（bootstrap 会重新签券）",
                    identity.dir.display()
                ));
            }
            Expiry::RenewDue { not_after } => {
                // 续期失败不影响本次使用：旧证书还能用，下一拍还会再试。
                match renew(&self.settings, endpoint, &worker_id, &identity).await {
                    Ok(new_not_after) => {
                        redact::info(format!("客户端证书已续期：{not_after} → {new_not_after}"));
                    }
                    Err(err) => {
                        redact::warn(format!(
                            "客户端证书续期失败（继续用现有证书，{not_after} 到期）：{err}"
                        ));
                    }
                }
            }
            Expiry::Valid => {}
        }

        let cert = std::fs::read_to_string(&identity.cert)
            .map_err(|err| format!("读取客户端证书失败 {}：{err}", identity.cert.display()))?;
        let key = std::fs::read_to_string(&identity.key)
            .map_err(|err| format!("读取客户端私钥失败 {}：{err}", identity.key.display()))?;
        let http = mtls_client(&self.settings, &cert, &key)?;

        Ok(GatewayClient {
            http,
            endpoint: endpoint.to_string(),
            worker_id,
            source_code: source_code.to_string(),
            instance_id: self.instance_id.clone(),
        })
    }

    fn identity_paths(&self) -> IdentityPaths {
        let dir = self
            .settings
            .identity_dir
            .clone()
            .unwrap_or_else(|| self.state_dir.join("identity"));
        IdentityPaths {
            cert: dir.join("client.crt"),
            key: dir.join("client.key"),
            dir,
        }
    }

    /// 这次接入要用的券：现场显式给的 → `<state_dir>/enroll.ticket`。
    pub(crate) fn ticket(&self) -> Option<String> {
        self.ticket
            .clone()
            .or_else(|| ticket_from_state_dir(&self.state_dir))
    }
}

struct IdentityPaths {
    dir: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

impl IdentityPaths {
    fn ready(&self) -> bool {
        self.cert.is_file() && self.key.is_file()
    }
}

/// 证书到期状态（判定在本地读证书本身，不靠旁路文件）。
enum Expiry {
    Valid,
    RenewDue { not_after: String },
    Expired { not_after: String },
}

fn certificate_expiry(cert_path: &Path) -> Result<Expiry, String> {
    let pem = std::fs::read_to_string(cert_path)
        .map_err(|err| format!("读取客户端证书失败 {}：{err}", cert_path.display()))?;
    certificate_expiry_from_pem(&pem, Utc::now())
}

fn certificate_expiry_from_pem(pem: &str, now: DateTime<Utc>) -> Result<Expiry, String> {
    use x509_parser::prelude::{FromDer, X509Certificate};

    let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|err| format!("客户端证书不是合法 PEM：{err:?}"))?;
    let (_, certificate) = X509Certificate::from_der(&parsed.contents)
        .map_err(|err| format!("客户端证书无法解析：{err}"))?;
    let not_after_unix = certificate.validity().not_after.timestamp();
    let remaining = not_after_unix - now.timestamp();
    let not_after = DateTime::from_timestamp(not_after_unix, 0)
        .map(|at| at.to_rfc3339())
        .unwrap_or_else(|| not_after_unix.to_string());
    if remaining <= 0 {
        Ok(Expiry::Expired { not_after })
    } else if remaining as u64 <= RENEW_WINDOW.as_secs() {
        Ok(Expiry::RenewDue { not_after })
    } else {
        Ok(Expiry::Valid)
    }
}

/// 首启接入：本地生成密钥 → CSR → 换客户端证书。
async fn enroll(
    settings: &GatewaySettings,
    endpoint: &str,
    worker_id: &str,
    source_code: &str,
    identity: &IdentityPaths,
    ticket: &str,
) -> Result<(), String> {
    let key_pair = rcgen::KeyPair::generate().map_err(|err| format!("生成密钥对失败：{err}"))?;
    let csr_pem = generate_csr(&key_pair)?;
    let key_pem = key_pair.serialize_pem();

    let http = plain_client(settings)?;
    let request = HarvestEnrollmentRequest::new(
        ticket,
        worker_id,
        source_code,
        new_instance_id(),
        csr_pem,
        now_rfc3339(),
    );
    let url = format!("{endpoint}/api/v1/harvest/enroll");
    let response = http
        .post(&url)
        .json(&request)
        .send()
        .await
        .map_err(|err| format!("提交接入请求失败：{err}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("读取接入响应失败：{err}"))?;
    if !status.is_success() {
        return Err(format!(
            "接入返回 {}：{}",
            status.as_u16(),
            truncate(&String::from_utf8_lossy(&body), 200)
        ));
    }
    let envelope: HarvestEnrollmentEnvelope =
        serde_json::from_slice(&body).map_err(|err| format!("接入报文不合法：{err}"))?;
    if envelope.result.status != HarvestEnrollmentStatus::Accepted {
        return Err(format!(
            "接入被拒：{}",
            envelope
                .result
                .reason_code
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }
    let certificate = envelope
        .result
        .certificate_pem
        .ok_or_else(|| "接入成功但没返回证书".to_string())?;

    std::fs::create_dir_all(&identity.dir)
        .map_err(|err| format!("创建身份目录失败 {}：{err}", identity.dir.display()))?;
    write_private(&identity.cert, &certificate)?;
    write_private(&identity.key, &key_pem)?;
    Ok(())
}

/// 用现有私钥（不换钥匙）签一张新 CSR 去续期。
async fn renew(
    settings: &GatewaySettings,
    endpoint: &str,
    worker_id: &str,
    identity: &IdentityPaths,
) -> Result<String, String> {
    let key_pem = std::fs::read_to_string(&identity.key)
        .map_err(|err| format!("读取客户端私钥失败 {}：{err}", identity.key.display()))?;
    let key_pair =
        rcgen::KeyPair::from_pem(&key_pem).map_err(|err| format!("客户端私钥无法解析：{err}"))?;
    let csr_pem = generate_csr(&key_pair)?;

    let cert = std::fs::read_to_string(&identity.cert)
        .map_err(|err| format!("读取客户端证书失败 {}：{err}", identity.cert.display()))?;
    let http = mtls_client(settings, &cert, &key_pem)?;

    let request =
        HarvestCertificateRenewal::new(worker_id, new_instance_id(), csr_pem, now_rfc3339());
    let url = format!("{endpoint}/api/v1/harvest/credentials:renew");
    let response = http
        .post(&url)
        .json(&request)
        .send()
        .await
        .map_err(|err| format!("提交续期请求失败：{err}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("读取续期响应失败：{err}"))?;
    if !status.is_success() {
        return Err(format!(
            "续期返回 {}：{}",
            status.as_u16(),
            truncate(&String::from_utf8_lossy(&body), 200)
        ));
    }
    let renewed: HarvestCertificateRenewed =
        serde_json::from_slice(&body).map_err(|err| format!("续期报文不合法：{err}"))?;
    // 先写临时文件再改名：写一半就断电不能留下半张证书。
    write_private_atomic(&identity.cert, &renewed.certificate_pem)?;
    Ok(renewed.not_after)
}

fn generate_csr(key_pair: &rcgen::KeyPair) -> Result<String, String> {
    let params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|err| format!("构建 CSR 参数失败：{err}"))?;
    let csr = params
        .serialize_request(key_pair)
        .map_err(|err| format!("生成 CSR 失败：{err}"))?;
    csr.pem().map_err(|err| format!("CSR 转 PEM 失败：{err}"))
}

/// 只带信任锚的客户端（接入 / 续期前的探活也算这一类）。
/// bootstrap（同模块的管理面动作）复用它，避免两份 CA 读取逻辑。
pub(crate) fn plain_client(settings: &GatewaySettings) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
    if let Some(ca_file) = settings.ca_file.as_deref() {
        builder = builder.add_root_certificate(read_ca(ca_file)?);
    }
    builder
        .build()
        .map_err(|err| format!("构建 HTTPS 客户端失败：{err}"))
}

/// mTLS 客户端：信任锚 + 客户端证书。私钥只在进程内存里。
fn mtls_client(
    settings: &GatewaySettings,
    cert_pem: &str,
    key_pem: &str,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
    if let Some(ca_file) = settings.ca_file.as_deref() {
        builder = builder.add_root_certificate(read_ca(ca_file)?);
    }
    let identity_pem = format!("{cert_pem}{key_pem}");
    let identity = reqwest::Identity::from_pem(identity_pem.as_bytes())
        .map_err(|err| format!("客户端证书/私钥不是合法 PEM：{err}"))?;
    builder
        .identity(identity)
        .build()
        .map_err(|err| format!("构建 HTTPS 客户端失败：{err}"))
}

fn read_ca(path: &Path) -> Result<reqwest::Certificate, String> {
    let ca = std::fs::read(path)
        .map_err(|err| format!("读取服务端 CA 失败 {}：{err}", path.display()))?;
    reqwest::Certificate::from_pem(&ca).map_err(|err| format!("CA 不是合法 PEM：{err}"))
}

fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|err| format!("写入 {} 失败：{err}", path.display()))?;
    restrict(path)
}

fn write_private_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    write_private(&tmp, contents)?;
    std::fs::rename(&tmp, path).map_err(|err| format!("替换 {} 失败：{err}", path.display()))
}

fn restrict(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, mode)
            .map_err(|err| format!("设置 {} 权限失败：{err}", path.display()))?;
    }
    Ok(())
}

fn new_instance_id() -> String {
    format!("hinst-{}", std::process::id())
}

/// bootstrap 写在 `<state_dir>/enroll.ticket` 里的一次性接入券（还没换成证书时）。
pub fn ticket_from_state_dir(state_dir: &Path) -> Option<String> {
    let path = state_dir.join("enroll.ticket");
    let raw = std::fs::read_to_string(path).ok()?;
    let ticket = raw.trim();
    if ticket.is_empty() {
        None
    } else {
        Some(ticket.to_string())
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> GatewaySettings {
        GatewaySettings {
            endpoint: "https://gw.example.com:3000".to_string(),
            source_code: "tencent-cloud-prod".to_string(),
            ..Default::default()
        }
    }

    fn self_signed_pem(not_after_days: i64) -> String {
        let key_pair = rcgen::KeyPair::generate().expect("key");
        let mut params =
            rcgen::CertificateParams::new(vec!["harvest.test".to_string()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        let expiry = Utc::now() + chrono::Duration::days(not_after_days);
        params.not_after = rcgen::date_time_ymd(
            expiry.format("%Y").to_string().parse().expect("year"),
            expiry.format("%m").to_string().parse().expect("month"),
            expiry.format("%d").to_string().parse().expect("day"),
        );
        params.self_signed(&key_pair).expect("cert").pem()
    }

    #[test]
    fn classifies_certificate_expiry_from_the_certificate_itself() {
        assert!(matches!(
            certificate_expiry_from_pem(&self_signed_pem(30), Utc::now()).expect("valid"),
            Expiry::Valid
        ));
        assert!(matches!(
            certificate_expiry_from_pem(&self_signed_pem(3), Utc::now()).expect("due"),
            Expiry::RenewDue { .. }
        ));
        assert!(matches!(
            certificate_expiry_from_pem(&self_signed_pem(-1), Utc::now()).expect("expired"),
            Expiry::Expired { .. }
        ));
    }

    #[test]
    fn rejects_a_certificate_that_is_not_a_certificate() {
        assert!(certificate_expiry_from_pem("not a pem", Utc::now()).is_err());
    }

    /// 续期窗口的边界按秒判定（不在当天/当分上模糊）。
    /// 网关签 37 天，客户端应在剩 ≤7 天时续期 —— 早一天晚一天都是行为差异，得钉住。
    #[test]
    fn renewal_window_boundary_is_exact() {
        let pem = self_signed_pem(30);
        use x509_parser::prelude::{FromDer, X509Certificate};
        let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).expect("pem");
        let (_, certificate) = X509Certificate::from_der(&parsed.contents).expect("certificate");
        let not_after = certificate.validity().not_after.timestamp();
        let at =
            |offset: i64| DateTime::from_timestamp(not_after + offset, 0).expect("valid timestamp");

        let window = RENEW_WINDOW.as_secs() as i64;
        assert!(matches!(
            certificate_expiry_from_pem(&pem, at(-window - 60)).expect("valid"),
            Expiry::Valid
        ));
        assert!(matches!(
            certificate_expiry_from_pem(&pem, at(-window + 60)).expect("due"),
            Expiry::RenewDue { .. }
        ));
        assert!(matches!(
            certificate_expiry_from_pem(&pem, at(-1)).expect("due"),
            Expiry::RenewDue { .. }
        ));
        // 到期那一秒就算过期：闸门不能在边界上放行。
        assert!(matches!(
            certificate_expiry_from_pem(&pem, at(0)).expect("expired"),
            Expiry::Expired { .. }
        ));
    }

    /// 身份目录默认跟 `state_dir` 走：**必须**落在容器挂进来的那个目录里（否则重建即丢）。
    #[test]
    fn identity_dir_defaults_under_state_dir_and_can_be_overridden() {
        let state = Path::new("/var/lib/wist-harvest/state");
        let connector = GatewayConnector::new(&settings(), state, None);
        assert_eq!(
            connector.identity_dir(),
            state.join("identity"),
            "默认必须挂在 state_dir 下"
        );

        let mut custom = settings();
        custom.identity_dir = Some(PathBuf::from("/etc/wist-harvest/identity"));
        let connector = GatewayConnector::new(&custom, state, None);
        assert_eq!(
            connector.identity_dir(),
            PathBuf::from("/etc/wist-harvest/identity")
        );
    }

    #[test]
    fn one_shot_ticket_file_is_read_and_trimmed() {
        let state = tempfile::tempdir().expect("tempdir");
        assert!(ticket_from_state_dir(state.path()).is_none(), "缺文件");

        std::fs::write(state.path().join("enroll.ticket"), "   \n").expect("blank");
        assert!(ticket_from_state_dir(state.path()).is_none(), "只有空白");

        std::fs::write(state.path().join("enroll.ticket"), "hwt-abc\n").expect("ticket");
        assert_eq!(
            ticket_from_state_dir(state.path()).as_deref(),
            Some("hwt-abc")
        );
    }

    /// 私钥落盘必须 0600；续期换证书走 tmp+rename，不能留下半张证书。
    #[cfg(unix)]
    #[test]
    fn identity_files_are_private_and_certificate_swap_is_atomic() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path().join("client.key");
        write_private(&key, "secret").expect("write key");
        let mode = std::fs::metadata(&key).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "私钥必须是 0600");

        let cert = dir.path().join("client.crt");
        write_private(&cert, "old").expect("write cert");
        write_private_atomic(&cert, "new").expect("swap cert");
        assert_eq!(std::fs::read_to_string(&cert).expect("read"), "new");
        let mode = std::fs::metadata(&cert).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "续期后的证书也要收紧到 0600");
        assert!(
            !cert.with_extension("tmp").exists(),
            "临时文件不能留在身份目录里"
        );
    }

    /// 没证书也没券：只能报错（带下一步该做什么），不能 panic。
    /// 这是 `run` 能“常驻重试”而不是“重启才恢复”的前提。
    #[tokio::test]
    async fn connect_without_identity_or_ticket_explains_what_is_missing() {
        let state = tempfile::tempdir().expect("tempdir");
        let connector = GatewayConnector::new(&settings(), state.path(), None);
        assert!(!connector.identity_ready());
        let err = connector.connect().await.err().expect("no identity");
        assert!(err.contains("接入券"), "{err}");
    }

    /// 过期证书在**建连前**就被拦住，并且不读私钥、不发请求。
    #[tokio::test]
    async fn expired_certificate_is_refused_before_any_request() {
        let state = tempfile::tempdir().expect("tempdir");
        let identity = state.path().join("identity");
        std::fs::create_dir_all(&identity).expect("identity dir");
        std::fs::write(identity.join("client.crt"), self_signed_pem(-1)).expect("cert");
        std::fs::write(identity.join("client.key"), "not a key").expect("key");

        let connector = GatewayConnector::new(&settings(), state.path(), None);
        assert!(connector.identity_ready());
        let err = connector.connect().await.err().expect("expired");
        assert!(err.contains("过期"), "{err}");
    }
}
