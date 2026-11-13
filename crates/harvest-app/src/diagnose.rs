//! `diagnose`：容器 healthcheck 用。
//!
//! 三条约束，都是 healthcheck 语义要求的：
//!
//! 1. **非交互、不联网**：只做本地检查。数据平面端点**只校验 `host:port` 形态**，
//!    不做 TCP 探测——否则对端一忙就重启采集器，反而放大故障。
//! 2. **分项结果**：每项一条 `{name, ok, detail}`，失败项足以定位问题。
//! 3. **零凭据泄漏**：只输出"来自 env:XXX / 来自文件路径"，任何字符串输出前
//!    都过一遍全局脱敏器（成功读到的凭据值会注册进去做兜底）。

use std::path::Path;

use harvest_core::config::{RegionsSpec, ServiceConfig, SourceConfig};
use harvest_core::redact;
use harvest_core::spool::Spool;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: Value,
}

#[derive(Debug, Clone, Default)]
pub struct DiagnoseReport {
    pub checks: Vec<Check>,
}

impl DiagnoseReport {
    pub fn ok(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|check| check.ok)
    }

    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|check| !check.ok).collect()
    }

    pub fn exit_code(&self) -> i32 {
        if self.ok() { 0 } else { 1 }
    }

    /// `quiet = true` 时只保留失败项。
    pub fn to_json(&self, quiet: bool) -> Value {
        let checks: Vec<Value> = self
            .checks
            .iter()
            .filter(|check| !quiet || !check.ok)
            .map(|check| {
                json!({
                    "name": check.name,
                    "ok": check.ok,
                    "detail": check.detail,
                })
            })
            .collect();
        let mut value = json!({
            "ok": self.ok(),
            "failures": self.failures().len(),
            "checks": checks,
        });
        scrub_value(&mut value);
        value
    }

    /// 逐行输出（stderr 用）。`quiet` 时只输出失败项。
    pub fn lines(&self, quiet: bool) -> Vec<String> {
        self.checks
            .iter()
            .filter(|check| !quiet || !check.ok)
            .map(|check| {
                format!(
                    "{} {} {}",
                    if check.ok { "OK  " } else { "FAIL" },
                    check.name,
                    redact::scrub(&check.detail.to_string())
                )
            })
            .collect()
    }
}

fn scrub_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact::scrub(text),
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        Value::Object(map) => map.values_mut().for_each(scrub_value),
        _ => {}
    }
}

/// 跑完全部检查。不联网、不写 spool、不改状态。
pub fn diagnose(config_path: &Path, source_path: &Path) -> DiagnoseReport {
    let mut checks = Vec::new();

    let service = match ServiceConfig::load(config_path) {
        Ok(service) => {
            checks.push(Check {
                name: "service_config",
                ok: true,
                detail: json!(config_path.display().to_string()),
            });
            Some(service)
        }
        Err(err) => {
            checks.push(Check {
                name: "service_config",
                ok: false,
                detail: json!(err.to_string()),
            });
            None
        }
    };

    // 关闭态：healthcheck 只探活 + **落点目录仍要可用**，不检查数据源 / 凭据 / 采集计划 ——
    // 关掉采集的站点本来就没有凭据，检查它会让容器以 unhealthy 无限重启；
    // 但 spool/state 是「现在没事、开启就炸」的那类问题，必须在关闭态也报出来。
    if let Some(service) = &service
        && !service.service.enabled
    {
        checks.push(Check {
            name: "service_enabled",
            ok: true,
            detail: json!({
                "enabled": false,
                "note": "采集已关闭（service.enabled=false）：只探活与落点目录，不检查数据源 / 凭据 / 采集计划",
            }),
        });
        checks.push(check_dir("spool_dir", &service.service.spool_dir));
        checks.push(check_dir("state_dir", &service.service.state_dir));
        return DiagnoseReport { checks };
    }

    let source = match SourceConfig::load(source_path) {
        Ok(source) => {
            checks.push(Check {
                name: "source_config",
                ok: true,
                detail: json!(source_path.display().to_string()),
            });
            Some(source)
        }
        Err(err) => {
            checks.push(Check {
                name: "source_config",
                ok: false,
                detail: json!(err.to_string()),
            });
            None
        }
    };

    // 凭据：只报来源与可用性；读到的值立刻注册到脱敏器，绝不输出。
    if let Some(source) = &source {
        checks.push(check_credential(source));
    }

    if let Some(service) = &service {
        checks.push(check_dir("spool_dir", &service.service.spool_dir));
        checks.push(check_dir("state_dir", &service.service.state_dir));
        checks.push(check_sink(&service.sink));
    }

    if let (Some(service), Some(source)) = (&service, &source) {
        checks.push(check_resumable(&service.service.spool_dir, source));
        checks.push(check_plan(source));
    }

    DiagnoseReport { checks }
}

fn check_credential(source: &SourceConfig) -> Check {
    let mut detail = serde_json::Map::new();
    let mut ok = true;
    let described = source.credential.describe();

    // 逐个读：能读出来才算「存在」。读到的值只用来注册脱敏，不落任何输出。
    for name in source.credential.names() {
        let description = described.get(name).cloned().unwrap_or(Value::Null);
        match source.credential.load(name) {
            Ok(secret) => {
                redact::register_secret(secret.expose());
                detail.insert(name.to_string(), description);
            }
            Err(err) => {
                ok = false;
                detail.insert(name.to_string(), description);
                detail.insert(format!("{name}_error"), json!(err.to_string()));
            }
        }
    }

    // 该厂商必填的键必须在：漏一个就是「配置看起来对、启动就失败」。
    let required = crate::vendor::required_credentials(source.vendor());
    for name in required {
        if !source.credential.names().any(|existing| existing == *name) {
            ok = false;
            detail.insert(format!("{name}_missing"), json!(true));
        }
    }
    detail.insert("vendor".to_string(), json!(source.vendor().as_str()));
    detail.insert("required".to_string(), json!(required));

    Check {
        name: "credential",
        ok,
        detail: Value::Object(detail),
    }
}

fn check_dir(name: &'static str, path: &Path) -> Check {
    let result = std::fs::create_dir_all(path).and_then(|_| {
        let probe = path.join(".wist-harvest-probe");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(&probe)
    });
    match result {
        Ok(()) => Check {
            name,
            ok: true,
            detail: json!(path.display().to_string()),
        },
        Err(err) => Check {
            name,
            ok: false,
            detail: json!(format!("{}: {err}", path.display())),
        },
    }
}

/// 端点只做形态与 TLS 配置检查（不拨号）。
fn check_sink(settings: &harvest_core::config::SinkSettings) -> Check {
    let endpoint = match harvest_warp::parse_endpoint(&settings.endpoint) {
        Ok((host, port)) => (host, port),
        Err(err) => {
            return Check {
                name: "sink",
                ok: false,
                detail: json!(err),
            };
        }
    };

    let mut problems: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    match (&settings.cert_file, &settings.key_file) {
        (Some(_), None) | (None, Some(_)) => {
            problems.push("cert_file 与 key_file 必须同时配置或同时留空".to_string());
        }
        _ => {}
    }
    if let Some(ca) = &settings.ca_file
        && !ca.exists()
    {
        problems.push(format!("ca_file 不存在: {}", ca.display()));
    }
    if settings.allow_plaintext {
        // 不是失败（本地 mock 会用），但 healthcheck 输出里要明说。
        // 不报失败是故意的：把"配置如此"当成故障会让容器反复重启。
        notes.push("allow_plaintext=true：流量未加密".to_string());
    }

    Check {
        name: "sink",
        ok: problems.is_empty(),
        detail: json!({
            "host": endpoint.0,
            "port": endpoint.1,
            "tls": !settings.allow_plaintext,
            "mtls": settings.cert_file.is_some() && settings.key_file.is_some(),
            "problems": problems,
            "notes": notes,
        }),
    }
}

fn check_resumable(spool_dir: &Path, source: &SourceConfig) -> Check {
    let regions = match &source.regions {
        RegionsSpec::Explicit(list) => list.clone(),
        RegionsSpec::Auto(_) => {
            return Check {
                name: "resumable_spool",
                ok: true,
                detail: json!("regions=auto：范围 hash 需联网解析，本项在运行时判断"),
            };
        }
    };
    let scope = source.scope(regions);
    match Spool::find_resumable(spool_dir, &scope) {
        Ok(Some(dir)) => Check {
            name: "resumable_spool",
            ok: true,
            detail: json!({"dir": dir.display().to_string(), "note": "存在未完成 spool，once/run 会续跑"}),
        },
        Ok(None) => Check {
            name: "resumable_spool",
            ok: true,
            detail: json!("无未完成 spool"),
        },
        Err(err) => Check {
            name: "resumable_spool",
            ok: false,
            detail: json!(err.to_string()),
        },
    }
}

fn check_plan(source: &SourceConfig) -> Check {
    let items = crate::vendor::plan_from_config(source);
    Check {
        name: "plan",
        ok: items.is_ok(),
        detail: match items {
            Ok(items) => json!({
                "vendor": source.vendor().as_str(),
                "work_items": items.len(),
                "note": match &source.regions {
                    RegionsSpec::Auto(_) => "regions=auto：运行时解析后生成",
                    RegionsSpec::Explicit(_) => "按配置地域生成",
                },
            }),
            Err(err) => json!(err.to_string()),
        },
    }
}

/// 打印结果并返回退出码：全 OK → 0；任一 FAIL → 1。
pub fn run(config_path: &Path, source_path: &Path, quiet: bool) -> i32 {
    let report = diagnose(config_path, source_path);
    for line in report.lines(quiet) {
        eprintln!("{line}");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&report.to_json(quiet)).unwrap_or_default()
    );
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_service_probes_aliveness_and_dirs_only() {
        // 关闭态：healthcheck 必须过 —— 关掉采集的站点本来就没有凭据，
        // 否则容器会以 unhealthy 无限重启（这是「常驻 + 运行时开关」的前提）。
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let config = dir.path().join("harvest.toml");
        std::fs::write(
            &config,
            format!(
                "[service]\nenabled = false\nspool_dir = {:?}\nstate_dir = {:?}\n",
                spool, state
            ),
        )
        .unwrap();
        let source = dir.path().join("does-not-exist.toml");

        let report = diagnose(&config, &source);
        assert!(report.ok(), "{}", report.to_json(false));
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "service_enabled")
        );
        // 关闭态不读数据源
        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.name == "source_config")
        );
    }

    #[test]
    fn disabled_service_still_probes_the_spool_and_state_dirs() {
        // 关闭态也要检查落点：`check_dir` 的语义是「能建出来并且写得进去」
        // （盘没挂上 / 只读挂载会在这里红），所以断言「检查发生了 + 目录真的被建了」。
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("spool");
        let state = dir.path().join("state");
        let config = dir.path().join("harvest.toml");
        std::fs::write(
            &config,
            format!(
                "[service]\nenabled = false\nspool_dir = {:?}\nstate_dir = {:?}\n",
                spool, state
            ),
        )
        .unwrap();
        let source = dir.path().join("does-not-exist.toml");

        let report = diagnose(&config, &source);
        assert!(
            report.checks.iter().any(|check| check.name == "spool_dir"),
            "{}",
            report.to_json(false)
        );
        assert!(report.checks.iter().any(|check| check.name == "state_dir"));
        assert!(spool.is_dir() && state.is_dir(), "落点目录应被检查时建出来");
    }

    #[test]
    fn enabled_service_still_requires_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("harvest.toml");
        std::fs::write(&config, "[service]\nenabled = true\n").unwrap();
        let source = dir.path().join("does-not-exist.toml");

        let report = diagnose(&config, &source);
        assert!(!report.ok(), "开着采集时数据源读不到必须报失败");
    }
}
