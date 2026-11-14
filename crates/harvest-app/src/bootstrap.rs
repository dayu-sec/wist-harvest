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

    // ① 身份：缺就用券接入。券优先用现场给的那张（跨机部署 / 手工排障），
    //    没有就现签一张 —— 已有旧券文件也直接覆盖，避免“留着张用过的券”把接入卡死。
    let mut connector = GatewayConnector::new(
        &config.gateway,
        &config.service.state_dir,
        configured_ticket(&config),
    );
    if connector.identity_ready() {
        println!(
            "身份已存在：{}（跳过接入券）",
            connector.identity_dir().display()
        );
    } else {
        // 这里**故意不**用 `connector.ticket()`：它会回退到 `state/enroll.ticket`，
        // 而那份很可能是上一轮用过的券 —— 拿它去换证书只会永远失败（券只能消费一次），
        // 且下一轮还会读到同一份。要么现场显式给券，要么现签一张。
        let ticket = match configured_ticket(&config) {
            Some(ticket) => Some(ticket),
            None => mint_ticket(&http, endpoint, &config, &source.source_code).await?,
        };
        match ticket {
            Some(ticket) => {
                redact::register_secret(&ticket);
                connector =
                    GatewayConnector::new(&config.gateway, &config.service.state_dir, Some(ticket));
            }
            None => {
                redact::warn(
                    "没有接入券也拿不到 admin token：跳过接入，run 会按拍重试（补上其一即可自愈）",
                );
            }
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

/// 现场显式配置的接入券（配置引用 → 环境变量）。**不含** `state/enroll.ticket`：
/// 那份可能是用过的旧券，bootstrap 宁可重新签一张（见 §①）。
fn configured_ticket(config: &ServiceConfig) -> Option<String> {
    let from_config = config
        .gateway
        .enroll_ticket
        .as_ref()
        .and_then(|reference| reference.load().ok())
        .map(|secret| secret.expose().to_string());
    from_config.or_else(|| {
        std::env::var("HARVEST_ENROLL_TICKET")
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
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

    /// 现场显式配置的券要能被读到（0600/0640 才合法），且去掉行尾空白。
    #[cfg(unix)]
    #[test]
    fn configured_ticket_reads_the_operator_supplied_secret() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join("sec-value.yml");
        std::fs::write(&secret, "harvest_enroll_ticket: hwt-from-operator\n")
            .expect("write secret");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o640))
            .expect("mode 0640");

        let config = ServiceConfig::parse(&format!(
            "[gateway]\nendpoint = \"https://gw.example.com:3000\"\nsource_code = \"demo\"\n\n\
             [gateway.enroll_ticket]\nfile = \"{}\"\nfield = \"harvest_enroll_ticket\"\n",
            secret.display()
        ))
        .expect("config parses");

        assert_eq!(
            configured_ticket(&config).as_deref(),
            Some("hwt-from-operator")
        );
    }

    /// 没有配置引用时不能凭空造一张 —— 交给 `mint_ticket` 现签（它需要 admin token）。
    #[test]
    fn without_a_configured_ticket_there_is_nothing_to_use() {
        // 环境变量是进程级的，测试里只断言“配置里没有”，避免和环境抢同一个键。
        let config = ServiceConfig::parse(
            "[gateway]\nendpoint = \"https://gw.example.com:3000\"\nsource_code = \"demo\"\n",
        )
        .expect("config parses");
        if std::env::var("HARVEST_ENROLL_TICKET").is_ok_and(|v| !v.trim().is_empty()) {
            return; // 宿主环境注入了券：这条用例不成立，跳过。
        }
        assert!(configured_ticket(&config).is_none());
    }
}
