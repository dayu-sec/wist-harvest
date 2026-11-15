//! `bootstrap`：同栈部署的一次性接入 + 初始授权。
//!
//! 同一台栈里跑的 worker 也需要两件“控制面签字”的事：
//! 1. **接入**：用管理面 admin token 签发一张一次性券，**当场换证书**并写到
//!    `<state_dir>/identity/`（`run` 起来直接用，不用再等券）；
//! 2. **初始授权**：仅当 `service.enabled=true` **且从未授权过**时，用 `sources.d/source.toml`
//!    的 assets / regions 派生一条 `plan_version=1` 的 standing。
//!
//! 顺序不能反：worker 行是 `enroll` 时建立的，先建 standing 会 404（`harvest_worker_not_found`）。
//!
//! 幂等：已有证书 / 已有 standing 就跳过。**失败不阻断采集**：`run` 自己会重试接入，
//! 这里退出 0 并留一条 warn，只有“配置自相矛盾”这种改不改都跑不起来的情况才退出 3。

use std::path::Path;
use std::time::Duration;

use harvest_core::config::{RegionsSpec, ServiceConfig, SourceConfig};
use harvest_core::redact;
use serde_json::{Value, json};

use crate::gateway::{self, GatewayConnector};

/// 等网关起来的窗口：bootstrap 在 `gops run start` 之后跑，网关通常几秒内就绪。
const GATEWAY_WAIT_SECONDS: u64 = 120;
const STEP: Duration = Duration::from_secs(2);

pub async fn run(config_path: &Path, source_path: &Path) -> i32 {
    match run_inner(config_path, source_path).await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("harvest bootstrap 失败：{err}");
            3
        }
    }
}

async fn run_inner(config_path: &Path, source_path: &Path) -> Result<(), String> {
    let config = ServiceConfig::load(config_path).map_err(|err| err.to_string())?;
    if !config.gateway.is_configured() {
        println!("未配置 [gateway]：跳过 bootstrap（离线模式，用本机调度）");
        return Ok(());
    }
    let source = SourceConfig::load(source_path).map_err(|err| err.to_string())?;
    if config.gateway.source_code.trim() != source.source_code {
        return Err(format!(
            "[gateway].source_code={:?} 与 source.toml 的 {:?} 不一致",
            config.gateway.source_code, source.source_code
        ));
    }

    let endpoint = config.gateway.endpoint.trim().trim_end_matches('/');
    let http = gateway::plain_client(&config.gateway)?;
    wait_gateway(&http, endpoint).await?;

    // ① 身份：缺就接入。
    //
    //    接入券的两个来源都是**瞬时**的，不进长期密钥库（`.wist/sec-value.yml` 里只放
    //    admin token 这种要轮换 / 进备份的凭据）：
    //      · `HARVEST_ENROLL_TICKET`（一次性环境变量；会在 `docker inspect` 里可见）
    //      · `<state_dir>/enroll.ticket`（操作者投放，0600，用完即删）
    //    两者都没有才现签一张（同栈默认路径：券只在内存里活几秒）。
    let mut connector =
        GatewayConnector::new(&config.gateway, &config.service.state_dir, env_ticket());
    if connector.identity_ready() {
        println!(
            "身份已存在：{}（跳过接入券）",
            connector.identity_dir().display()
        );
    } else {
        let operator_ticket =
            env_ticket().or_else(|| gateway::ticket_from_state_dir(&config.service.state_dir));
        let mut enrolled = false;
        if let Some(ticket) = operator_ticket.as_deref() {
            redact::register_secret(ticket);
            match enroll_with(&config, ticket).await {
                Ok(()) => enrolled = true,
                Err(err) => {
                    // 券只能消费一次：文件里那张多半是用过的。留着它只会每轮都失败，
                    // 所以删掉再说 —— 它是**我们自己的**临时文件，不是别人的凭据。
                    redact::warn(format!(
                        "现场接入券不可用（{err}）：已删除 {}，改为必要时现签",
                        config.service.state_dir.join("enroll.ticket").display()
                    ));
                    let _ = std::fs::remove_file(config.service.state_dir.join("enroll.ticket"));
                }
            }
        }
        if !enrolled {
            match mint_ticket(&http, endpoint, &config, &source.source_code).await? {
                Some(ticket) => {
                    redact::register_secret(&ticket);
                    if let Err(err) = enroll_with(&config, &ticket).await {
                        redact::warn(format!("接入网关失败（run 会按拍重试）：{err}"));
                    } else {
                        enrolled = true;
                    }
                }
                None => redact::warn(
                    "没有接入券也拿不到 admin token：跳过接入，run 会按拍重试（补上其一即可自愈）",
                ),
            }
        }
        // 身份已就位后重建 connector：`connect()` 顺带重查/续期证书。
        connector = GatewayConnector::new(&config.gateway, &config.service.state_dir, None);
        if !enrolled {
            redact::warn("尚未接入：跳过初始授权（接入后到页面「云资产采集」卡授权）");
            return Ok(());
        }
    }

    // 已有身份也走一次 connect：临近到期的证书在这里续掉，不用等 run 触发。
    // 失败**不阻断**：harvest 自己会重试，bootstrap 把容器卡死在启动前反而弄丢了自愈能力。
    let connected = match connector.connect().await {
        Ok(gateway) => {
            println!(
                "已接入网关：{}（{}）→ {}",
                connector.worker_id(),
                gateway.source_code(),
                connector.identity_dir().display()
            );
            true
        }
        Err(err) => {
            redact::warn(format!("接入网关失败（run 会按拍重试）：{err}"));
            false
        }
    };

    // ② 初始授权（仅新装且本地允许调度时）
    if !config.service.enabled {
        println!("HARVEST_ENABLED=false：本地硬关，不自动授权");
        return Ok(());
    }
    if !connected {
        redact::warn("尚未接入：跳过初始授权（接入后到页面「云资产采集」卡授权）");
        return Ok(());
    }
    let worker_id = connector.worker_id();
    let admin = match admin_token(&config) {
        Ok(admin) => admin,
        Err(err) => {
            redact::warn(format!("{err}：跳过初始授权"));
            return Ok(());
        }
    };
    if works_has_worker(&http, endpoint, &admin, &worker_id).await? {
        println!("standing 已存在（跳过初始授权）");
        return Ok(());
    }
    let spec = spec_from_source(&source, &cadence_at(), cadence_jitter());
    put_standing(&http, endpoint, &admin, &worker_id, &spec).await?;
    println!("已建立初始 standing：{worker_id}（plan_version=1）");
    Ok(())
}

/// 一次性接入券：**只有环境变量这一条“配置”通道**。
///
/// 券是短命的、一次性的，不应进 `.wist/sec-value.yml` 那种长期密钥库（那里面是 admin token、
/// 云 SecretId/Key 这类要轮换、要进备份的东西）。跨机部署的常规通道是
/// `<state_dir>/enroll.ticket`（见 §①）；环境变量留给一次性排障。
fn env_ticket() -> Option<String> {
    std::env::var("HARVEST_ENROLL_TICKET")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 用一张券完成接入（成功即写好 `identity/`，并删掉盘上那张券）。
async fn enroll_with(config: &ServiceConfig, ticket: &str) -> Result<(), String> {
    GatewayConnector::new(
        &config.gateway,
        &config.service.state_dir,
        Some(ticket.to_string()),
    )
    .connect()
    .await
    .map(|_| ())
}

fn admin_token(config: &ServiceConfig) -> Result<String, String> {
    let reference = config
        .gateway
        .admin_token
        .as_ref()
        .ok_or_else(|| "缺少 [gateway].admin_token（管理面动作需要它）".to_string())?;
    let secret = reference.load().map_err(|err| err.to_string())?;
    redact::register_secret(secret.expose());
    Ok(secret.expose().to_string())
}

/// 签一张接入券；**没有 admin token 时不报错**（返回 `None`，交给 run 重试）。
async fn mint_ticket(
    http: &reqwest::Client,
    endpoint: &str,
    config: &ServiceConfig,
    source_code: &str,
) -> Result<Option<String>, String> {
    let admin = match admin_token(config) {
        Ok(admin) => admin,
        Err(err) => {
            redact::warn(format!("{err}；跳过自动接入"));
            return Ok(None);
        }
    };
    let url = format!("{endpoint}/api/v1/admin/harvest/enroll-ticket");
    let response = http
        .post(&url)
        .bearer_auth(&admin)
        .json(&json!({ "source_code": source_code }))
        .send()
        .await
        .map_err(|err| format!("签发接入券失败：{err}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("读取接入券响应失败：{err}"))?;
    if !status.is_success() {
        return Err(format!(
            "签发接入券返回 {}：{}",
            status.as_u16(),
            truncate(&String::from_utf8_lossy(&body), 200)
        ));
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|err| format!("接入券报文不合法：{err}"))?;
    value
        .get("ticket")
        .and_then(Value::as_str)
        .map(|ticket| Some(ticket.to_string()))
        .ok_or_else(|| "接入券响应里没有 ticket".to_string())
}

async fn wait_gateway(http: &reqwest::Client, endpoint: &str) -> Result<(), String> {
    let url = format!("{endpoint}/api/v1/version");
    let deadline = std::time::Instant::now() + Duration::from_secs(GATEWAY_WAIT_SECONDS);
    loop {
        match http.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                redact::warn(format!("网关还没就绪（{}），继续等", response.status()));
            }
            Err(err) => redact::warn(format!("网关还没就绪（{err}），继续等")),
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("等了 {GATEWAY_WAIT_SECONDS}s 网关仍未就绪：{url}"));
        }
        tokio::time::sleep(STEP).await;
    }
}

async fn works_has_worker(
    http: &reqwest::Client,
    endpoint: &str,
    admin: &str,
    worker_id: &str,
) -> Result<bool, String> {
    let url = format!("{endpoint}/api/v1/admin/harvest/works");
    let response = http
        .get(&url)
        .bearer_auth(admin)
        .send()
        .await
        .map_err(|err| format!("查询工作授权失败：{err}"))?;
    if !response.status().is_success() {
        return Err(format!("查询工作授权返回 {}", response.status().as_u16()));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|err| format!("工作授权报文不合法：{err}"))?;
    Ok(value
        .get("works")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .any(|row| row.get("worker_id").and_then(Value::as_str) == Some(worker_id))
        })
        .unwrap_or(false))
}

async fn put_standing(
    http: &reqwest::Client,
    endpoint: &str,
    admin: &str,
    worker_id: &str,
    spec: &Value,
) -> Result<(), String> {
    let url = format!("{endpoint}/api/v1/admin/harvest/standing");
    let response = http
        .put(&url)
        .bearer_auth(admin)
        .json(&json!({ "worker_id": worker_id, "spec": spec }))
        .send()
        .await
        .map_err(|err| format!("建立 standing 失败：{err}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "建立 standing 返回 {}：{}",
            status.as_u16(),
            truncate(&body, 200)
        ));
    }
    Ok(())
}

/// 初始 standing 的节奏来源：compose 把 `HARVEST_RUN_AT` / `HARVEST_JITTER_SECONDS`
/// 同时给了 bootstrap 与 run，两边不能各写一份。
fn cadence_at() -> String {
    std::env::var("HARVEST_RUN_AT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "00:30".to_string())
}

fn cadence_jitter() -> u32 {
    std::env::var("HARVEST_JITTER_SECONDS")
        .ok()
        .and_then(|value| value.trim().trim_end_matches('s').parse().ok())
        .unwrap_or(300)
}

/// 初始 standing 的 spec：资产面与地域来自本机数据源，节奏由调用方给（见 [`cadence_at`]）。
///
/// 纯函数：不读环境、不联网，才能在单测里把映射钉住。
fn spec_from_source(source: &SourceConfig, at: &str, jitter_seconds: u32) -> Value {
    let mut assets = Vec::new();
    if source.assets.cloud_host {
        assets.push("cloud_host");
    }
    if source.assets.vpc {
        assets.push("vpc");
    }
    if source.assets.subnet {
        assets.push("subnet");
    }
    if source.assets.eip {
        assets.push("eip");
    }
    let regions = match &source.regions {
        RegionsSpec::Auto(_) => json!("auto"),
        RegionsSpec::Explicit(list) => json!(list),
    };
    json!({
        "credential_ref": source.source_code,
        "vendor": source.vendor.as_str(),
        "assets": assets,
        "regions": regions,
        "cadence": {"mode": "daily", "at": at, "jitter_seconds": jitter_seconds},
    })
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvest_core::config::ServiceConfig;

    fn source(toml: &str) -> SourceConfig {
        SourceConfig::parse(toml).expect("source parses")
    }

    const BASE: &str = r#"
source_code = "tencent-cloud-prod"
vendor = "tencent"
"#;

    /// 表必须写在所有顶层键**之后**（否则 `regions = …` 会落进 `[credential]`）。
    const CREDENTIAL: &str = r#"
[credential]
secret_id = { env = "SEC_TENCENT_SECRET_ID" }
secret_key = { env = "SEC_TENCENT_SECRET_KEY" }
"#;

    fn source_with(extra: &str) -> SourceConfig {
        source(&format!("{BASE}{extra}{CREDENTIAL}"))
    }

    /// 初始 standing 必须逐字反映本机数据源：资产面顺序、地域形态、节奏。
    /// 顺序变了会让页面上的勾选顺序跟着变（同样的集合显示成“被改过”）。
    #[test]
    fn initial_standing_mirrors_the_source_and_the_site_cadence() {
        let spec = spec_from_source(&source_with(""), "03:15", 120);
        assert_eq!(spec["credential_ref"], "tencent-cloud-prod");
        assert_eq!(spec["vendor"], "TENCENT_CLOUD");
        assert_eq!(
            spec["assets"],
            serde_json::json!(["cloud_host", "vpc", "subnet", "eip"])
        );
        assert_eq!(spec["regions"], "auto");
        assert_eq!(
            spec["cadence"],
            serde_json::json!({"mode": "daily", "at": "03:15", "jitter_seconds": 120})
        );
    }

    /// 站点显式地域要原样下发（不能悄悄折回 `auto`）；关掉的资产面不出现在 spec 里。
    #[test]
    fn explicit_regions_and_disabled_assets_are_carried_through() {
        let spec = spec_from_source(
            &source_with(
                "regions = [\"ap-guangzhou\", \"ap-shanghai\"]\n[assets]\nvpc = false\neip = false\n",
            ),
            "00:30",
            300,
        );
        assert_eq!(
            spec["regions"],
            serde_json::json!(["ap-guangzhou", "ap-shanghai"])
        );
        assert_eq!(spec["assets"], serde_json::json!(["cloud_host", "subnet"]));
    }

    /// 一个都不采也是合法 spec（网关会告警，但不下发非法值）。
    #[test]
    fn all_assets_disabled_yields_an_empty_list() {
        let spec = spec_from_source(
            &source_with(
                "[assets]\ncloud_host = false\nvpc = false\nsubnet = false\neip = false\n",
            ),
            "00:30",
            300,
        );
        assert_eq!(spec["assets"], serde_json::json!([]));
    }

    /// 券是**一次性**的，不能进长期密钥库：`harvest.toml` 里出现 `[gateway.enroll_ticket]`
    /// 必须是解析错误（`deny_unknown_fields`），而不是被默默接受。
    #[test]
    fn a_ticket_in_the_secret_file_is_no_longer_a_supported_channel() {
        let parsed = ServiceConfig::parse(
            "[gateway]\nendpoint = \"https://gw.example.com:3000\"\nsource_code = \"demo\"\n\n\
             [gateway.enroll_ticket]\nfile = \"/secrets/sec-value.yml\"\nfield = \"harvest_enroll_ticket\"\n",
        );
        assert!(
            parsed.is_err(),
            "接入券不能走 sec-value.yml（它是一次性的，不是要维护的凭据）"
        );
    }

    /// 跨机部署的常规通道是 `state/enroll.ticket`，由操作者投放、用完即删。
    #[test]
    fn the_state_dir_ticket_file_is_the_cross_host_channel() {
        let state = tempfile::tempdir().expect("tempdir");
        assert!(gateway::ticket_from_state_dir(state.path()).is_none());
        std::fs::write(state.path().join("enroll.ticket"), "hwt-from-operator\n")
            .expect("write ticket");
        assert_eq!(
            gateway::ticket_from_state_dir(state.path()).as_deref(),
            Some("hwt-from-operator")
        );
    }
}
