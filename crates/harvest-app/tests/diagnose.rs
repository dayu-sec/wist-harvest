//! `diagnose` 的验收：退出码、分项结果、零凭据泄漏。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use harvest_app::diagnose::{self, Check, DiagnoseReport};
use serde_json::json;

/// 只在这个测试里出现的哨兵值：任何输出里出现它就说明泄漏了。
const SENTINEL: &str = "SUPERSECRETSENTINEL1234567890";

fn write_secret(dir: &Path, name: &str, value: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, value).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn write_configs(dir: &Path, spool_dir: &Path) -> (PathBuf, PathBuf) {
    let secret_id = write_secret(dir, "id.secret", SENTINEL);
    let secret_key = write_secret(dir, "key.secret", &format!("{SENTINEL}-key"));

    let config = dir.join("harvest.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"

[sink]
endpoint = "warp-parse:9100"
"#,
            spool = spool_dir.display(),
            state = dir.join("state").display(),
        ),
    )
    .unwrap();

    let source = dir.join("tencent-cloud-prod.toml");
    std::fs::write(
        &source,
        format!(
            r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
regions = ["ap-guangzhou"]

[credential]
secret_id = {{ file = "{id}" }}
secret_key = {{ file = "{key}" }}
"#,
            id = secret_id.display(),
            key = secret_key.display(),
        ),
    )
    .unwrap();

    (config, source)
}

fn assert_no_secret(text: &str, where_: &str) {
    assert!(!text.contains(SENTINEL), "{where_} 里出现了凭据值：{text}");
}

#[test]
fn healthy_environment_exits_zero_and_leaks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let (config, source) = write_configs(dir.path(), &spool);

    let report = diagnose::diagnose(&config, &source);
    assert!(report.ok(), "健康环境不该有失败项: {:?}", report.failures());
    assert_eq!(report.exit_code(), 0);

    let json = report.to_json(false).to_string();
    assert_no_secret(&json, "诊断 JSON");
    for line in report.lines(false) {
        assert_no_secret(&line, "诊断输出行");
    }

    // 端点的分项结果应包含 host/port 这类可核对的信息。
    assert!(json.contains("warp-parse"));
    // 凭据项只报来源，不报值。
    assert!(json.contains("file"));
    assert!(json.contains("secret_id"));

    assert_eq!(diagnose::run(&config, &source, false), 0);
}

#[test]
fn failures_make_exit_non_zero_and_quiet_hides_ok_items() {
    let dir = tempfile::tempdir().unwrap();
    // 把 /dev/null 当目录用：create_dir_all 必然失败。
    let broken_spool = PathBuf::from("/dev/null/wist-harvest-spool");
    let (config, source) = write_configs(dir.path(), &broken_spool);

    let report = diagnose::diagnose(&config, &source);
    assert!(!report.ok());
    assert_eq!(report.exit_code(), 1);
    assert!(
        report
            .failures()
            .iter()
            .any(|check| check.name == "spool_dir"),
        "{:?}",
        report.failures()
    );

    let quiet = report.to_json(true);
    let checks = quiet["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    assert!(
        checks.iter().all(|check| check["ok"] == json!(false)),
        "--quiet 只输出失败项: {quiet}"
    );
    assert_eq!(quiet["failures"], json!(report.failures().len()));

    let lines = report.lines(true);
    assert!(lines.iter().all(|line| line.starts_with("FAIL")));
    assert!(
        !report
            .lines(false)
            .iter()
            .any(|line| line.starts_with("FAIL credential")),
        "凭据本身是可读的，不该报失败"
    );

    assert_ne!(diagnose::run(&config, &source, true), 0);
}

#[test]
fn missing_credential_file_fails_without_exposing_values() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let config = dir.path().join("harvest.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"

[sink]
endpoint = "127.0.0.1:9100"
"#,
            spool = spool.display(),
            state = dir.path().join("state").display(),
        ),
    )
    .unwrap();

    let missing = dir.path().join("does-not-exist.secret");
    let source = dir.path().join("source.toml");
    std::fs::write(
        &source,
        format!(
            r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
regions = ["ap-guangzhou"]

[credential]
secret_id = {{ file = "{id}" }}
secret_key = {{ file = "{id}" }}
"#,
            id = missing.display(),
        ),
    )
    .unwrap();

    let report = diagnose::diagnose(&config, &source);
    assert!(!report.ok());
    let credential = report
        .checks
        .iter()
        .find(|check| check.name == "credential")
        .expect("必须有凭据检查项");
    assert!(!credential.ok);
    assert_eq!(report.exit_code(), 1);
    assert_no_secret(&report.to_json(false).to_string(), "诊断 JSON");
}

#[test]
fn insecure_credential_permissions_are_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let spool = dir.path().join("spool");
    let secret = dir.path().join("loose.secret");
    std::fs::write(&secret, SENTINEL).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();

    let config = dir.path().join("harvest.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"

[sink]
endpoint = "warp-parse:9100"
"#,
            spool = spool.display(),
            state = dir.path().join("state").display(),
        ),
    )
    .unwrap();
    let source = dir.path().join("source.toml");
    std::fs::write(
        &source,
        format!(
            r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
regions = ["ap-guangzhou"]

[credential]
secret_id = {{ file = "{path}" }}
secret_key = {{ file = "{path}" }}
"#,
            path = secret.display(),
        ),
    )
    .unwrap();

    let report = diagnose::diagnose(&config, &source);
    assert!(!report.ok(), "0644 的凭据文件必须报失败");
    let credential = report
        .checks
        .iter()
        .find(|check| check.name == "credential")
        .unwrap();
    let rendered = credential.detail.to_string();
    assert!(rendered.contains("0600"), "{rendered}");
    assert_no_secret(&rendered, "凭据检查详情");
}

#[test]
fn rendering_scrubs_credentials_even_if_a_detail_echoes_them() {
    // 兜底防线：即使某天有人把凭据值塞进 detail，输出前也会被打码。
    harvest_core::redact::register_secret(SENTINEL);
    let report = DiagnoseReport {
        checks: vec![Check {
            name: "leak_probe",
            ok: false,
            detail: json!(format!("text={SENTINEL}")),
        }],
    };
    let json = report.to_json(false).to_string();
    assert_no_secret(&json, "兜底脱敏");
    assert!(json.contains("***"), "{json}");
    assert!(!report.ok());
}
