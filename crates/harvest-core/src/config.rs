//! 配置分层（设计说明 §2.1）。
//!
//! | 层 | 内容 |
//! | --- | --- |
//! | 服务配置 `harvest.toml` | 并发、限速、超时、spool 目录、帧上限、数据平面端点 |
//! | 数据源定义 `sources.d/<source_code>.toml` | 厂商、地域、类型开关、租户/环境引用 |
//! | 凭据 | 只从 `${SEC_*}` 环境变量或 0600 文件读 |
//!
//! 凭据纪律：配置里**从不出现**凭据明文，只有"去哪读"。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Vendor;
use crate::scope::{GROUP_INVENTORY, Scope};

// ---------------------------------------------------------------------------
// 凭据引用
// ---------------------------------------------------------------------------

/// 凭据来源。只支持两种，且都可审计：`{ env = "SEC_xxx" }` 或 `{ file = "/path" }`。
///
/// 用扁平结构而不是 enum：TOML 的 enum 表示法会把 `{ env = "X" }` 解成
/// 双层表，既难写也容易写错；这里要求二选一，并由 [`SecretRef::validate`] 卡住。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    file: Option<PathBuf>,
    /// 指定时按 JSON 解析文件内容并取该字段。
    #[serde(default)]
    field: Option<String>,
}

impl SecretRef {
    pub fn from_env(name: impl Into<String>) -> Self {
        Self {
            env: Some(name.into()),
            ..Default::default()
        }
    }

    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(path.into()),
            ..Default::default()
        }
    }

    /// 必须且只能指定一个来源。
    pub fn validate(&self) -> Result<(), ConfigError> {
        match (&self.env, &self.file) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (None, None) => Err(ConfigError::new("凭据引用必须指定 env 或 file 之一")),
            (Some(_), Some(_)) => Err(ConfigError::new("凭据引用不能同时指定 env 和 file")),
        }
    }

    pub fn load(&self) -> Result<SecretString, ConfigError> {
        self.validate()?;
        if let Some(env) = &self.env {
            let value = std::env::var(env).map_err(|_| {
                ConfigError::new(format!("环境变量 {env} 未设置（凭据缺失，无法签名）"))
            })?;
            if value.is_empty() {
                return Err(ConfigError::new(format!("环境变量 {env} 为空")));
            }
            return Ok(SecretString(value));
        }

        let file = self.file.as_ref().expect("validate 已保证 file 存在");
        ensure_private_mode(file)?;
        let raw = std::fs::read_to_string(file).map_err(|err| {
            ConfigError::new(format!("读取凭据文件 {} 失败: {err}", file.display()))
        })?;
        let value = match &self.field {
            None => raw.trim().to_string(),
            Some(field) => {
                let parsed = parse_flat_map(&raw).map_err(|why| {
                    ConfigError::new(format!(
                        "凭据文件 {} 无法解析为 JSON 或扁平 YAML（field={field}）：{why}",
                        file.display()
                    ))
                })?;
                parsed
                    .get(field.as_str())
                    .cloned()
                    .ok_or_else(|| ConfigError::new(format!("凭据文件缺少字段 {field}")))?
            }
        };
        if value.is_empty() {
            return Err(ConfigError::new("凭据文件内容为空"));
        }
        Ok(SecretString(value))
    }

    /// 供 `dry-run` / `diagnose` 输出：只说"从哪读、在不在"，不输出值。
    pub fn describe(&self) -> serde_json::Value {
        if let Some(env) = &self.env {
            return serde_json::json!({
                "source": "env",
                "name": env,
                "present": std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false),
            });
        }
        match &self.file {
            Some(file) => serde_json::json!({
                "source": "file",
                "path": file.display().to_string(),
                "present": file.exists(),
                "mode_ok": ensure_private_mode(file).is_ok(),
            }),
            None => serde_json::json!({"source": "unset", "present": false}),
        }
    }
}

/// 解析凭据文件为「扁平键值表」。
///
/// 支持两种输入，按内容自动判定：
/// 1. **JSON 对象**（`{"secret_id":"..."}`）；
/// 2. **扁平 YAML**（每行 `key: value`）—— 平台栈的 `.wist/sec-value.yml` 就是这种形态，
///    由 `gops sec` 维护。
///
/// 只接受**扁平**结构：出现缩进、或行尾裸 `:` 而无值（= 嵌套）就**直接报错**，
/// 不猜 —— 静默误读一个密钥文件比读不出来危险得多。
/// 不剥离行尾 `#`：密钥里可能出现 `#`；要写注释就单独占一行。
fn parse_flat_map(raw: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => {
            let mut out = std::collections::BTreeMap::new();
            for (key, value) in map {
                match value.as_str() {
                    Some(s) => {
                        out.insert(key, s.to_string());
                    }
                    None => return Err(format!("字段 {key} 不是字符串")),
                }
            }
            if out.is_empty() {
                return Err("空文件".to_string());
            }
            return Ok(out);
        }
        Ok(_) => return Err("JSON 顶层必须是对象".to_string()),
        Err(_) => {} // 不是 JSON，按扁平 YAML 试
    }

    let mut out = std::collections::BTreeMap::new();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(format!("第 {} 行有缩进：只支持扁平 key: value", idx + 1));
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("第 {} 行不是 key: value", idx + 1));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("第 {} 行缺少键名", idx + 1));
        }
        let value = value.trim();
        if value.is_empty() {
            return Err(format!(
                "第 {} 行（{key}）没有值：不支持嵌套结构，请展开成扁平键",
                idx + 1
            ));
        }
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        out.insert(key.to_string(), value.to_string());
    }
    if out.is_empty() {
        return Err("空文件".to_string());
    }
    Ok(out)
}

#[cfg(unix)]
fn ensure_private_mode(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .map_err(|err| ConfigError::new(format!("读取凭据文件元数据失败: {err}")))?;
    let mode = meta.permissions().mode() & 0o777;
    // 允许 0600，也允许 0640（**仅属组可读**）。
    //
    // 为什么放宽到 0640：平台把密钥放在 `.wist/`，以 **:ro** 挂进容器，容器固定以 999 运行、
    // 靠「属组」读（`gops sys perms` 也会把这类文件的模式收敛到 0640、属组 = 容器 gid）。
    // 若只认 0600，容器进程（非属主）永远读不到，只能把密钥塞进 compose 环境变量 ——
    // 那会在 `docker inspect` 里裸奔，比 0640 差得多。
    // 仍然拒绝：属组可写/可执行、以及其他任何账户可访问（如 0660 / 0644 / 0604）。
    if mode & 0o037 != 0 {
        return Err(ConfigError::new(format!(
            "凭据文件 {} 权限是 {:o}，必须是 0600，或 0640（仅属组可读）",
            path.display(),
            mode
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_mode(path: &Path) -> Result<(), ConfigError> {
    let _ = path;
    Ok(())
}

/// 凭据值。`Debug`/`Display` 一律打码，避免误打印。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// 直接包装一个值。生产路径应走 [`SecretRef::load`]，这里只给测试与
    /// 内存内传递用。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// 服务配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[serde(default)]
    pub service: ServiceSettings,
    #[serde(default)]
    pub retry: RetrySettings,
    #[serde(default)]
    pub rate: RateSettings,
    #[serde(default)]
    pub sink: SinkSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSettings {
    #[serde(default = "default_spool_dir")]
    pub spool_dir: PathBuf,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_page_size")]
    pub page_size: u64,
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// P0 顺序采集。并发留字段，避免下一阶段改配置结构。
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// 允许"云端确实为空"时发空数组快照。默认不允许：空 ≠ 失败。
    #[serde(default)]
    pub allow_empty: bool,
}

impl Default for ServiceSettings {
    fn default() -> Self {
        Self {
            spool_dir: default_spool_dir(),
            state_dir: default_state_dir(),
            page_size: default_page_size(),
            max_frame_bytes: default_max_frame_bytes(),
            max_response_bytes: default_max_response_bytes(),
            request_timeout_ms: default_request_timeout_ms(),
            concurrency: default_concurrency(),
            allow_empty: false,
        }
    }
}

fn default_spool_dir() -> PathBuf {
    PathBuf::from("/var/lib/wist-harvest/spool")
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/wist-harvest/state")
}
fn default_page_size() -> u64 {
    100
}
fn default_max_frame_bytes() -> usize {
    32 * 1024 * 1024
}
fn default_max_response_bytes() -> usize {
    16 * 1024 * 1024
}
fn default_request_timeout_ms() -> u64 {
    30_000
}
fn default_concurrency() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrySettings {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_base_delay_ms")]
    pub base_delay_ms: u64,
    /// 单次退避上限，防止退避把运行拖死。
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            base_delay_ms: default_base_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

fn default_max_attempts() -> u32 {
    4
}
fn default_base_delay_ms() -> u64 {
    200
}
fn default_max_delay_ms() -> u64 {
    5_000
}

/// 按 action 维度的令牌桶配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateSettings {
    #[serde(default = "default_qps")]
    pub default_qps: f64,
    #[serde(default = "default_burst")]
    pub default_burst: f64,
    /// 覆盖单个 action 的档位，例如 `DescribeInstances = { qps = 20.0, burst = 10.0 }`。
    #[serde(default)]
    pub actions: BTreeMap<String, RateSpec>,
}

impl Default for RateSettings {
    fn default() -> Self {
        Self {
            default_qps: default_qps(),
            default_burst: default_burst(),
            actions: BTreeMap::new(),
        }
    }
}

fn default_qps() -> f64 {
    5.0
}
fn default_burst() -> f64 {
    5.0
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateSpec {
    pub qps: f64,
    #[serde(default)]
    pub burst: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkSettings {
    /// 数据平面端点，`host:port`。
    #[serde(default)]
    pub endpoint: String,
    /// 明文 TCP。默认关闭；只在本地 mock 与受控内网调试时打开。
    #[serde(default)]
    pub allow_plaintext: bool,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub cert_file: Option<PathBuf>,
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
    /// 是否等待数据平面回 ACK。
    ///
    /// 默认 **false**：warp-parse 的 `tcp_src` 只消费长度前缀帧，**不回应答**。
    /// 若坚持等 ACK，每次发布都会超时 → 运行永远 Aborted、spool 永不清理（帧其实已经送到了）。
    /// 留着这个开关是给将来**有**应答语义的 sink（HTTP / MQ）用。
    #[serde(default)]
    pub expect_ack: bool,
    #[serde(default = "default_publish_attempts")]
    pub publish_attempts: u32,
}

impl Default for SinkSettings {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            allow_plaintext: false,
            ca_file: None,
            cert_file: None,
            key_file: None,
            connect_timeout_ms: default_connect_timeout_ms(),
            ack_timeout_ms: default_ack_timeout_ms(),
            expect_ack: false,
            publish_attempts: default_publish_attempts(),
        }
    }
}

fn default_connect_timeout_ms() -> u64 {
    5_000
}
fn default_ack_timeout_ms() -> u64 {
    15_000
}
fn default_publish_attempts() -> u32 {
    3
}

impl ServiceConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|err| {
            ConfigError::new(format!("读取服务配置 {} 失败: {err}", path.display()))
        })?;
        Self::parse(&raw).map_err(|err| err.context(path))
    }

    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(raw)
            .map_err(|err| ConfigError::new(format!("服务配置解析失败: {err}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.service.page_size == 0 {
            return Err(ConfigError::new("service.page_size 必须 > 0"));
        }
        if self.service.max_frame_bytes < 1024 {
            return Err(ConfigError::new(
                "service.max_frame_bytes 太小（< 1KiB），看起来是配置笔误",
            ));
        }
        if self.retry.max_attempts == 0 {
            return Err(ConfigError::new("retry.max_attempts 必须 >= 1"));
        }
        if self.rate.default_qps <= 0.0 {
            return Err(ConfigError::new("rate.default_qps 必须 > 0"));
        }
        for (action, spec) in &self.rate.actions {
            if spec.qps <= 0.0 {
                return Err(ConfigError::new(format!(
                    "rate.actions.{action}.qps 必须 > 0"
                )));
            }
        }
        Ok(())
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.service.state_dir.join("runs")
    }

    pub fn lease_dir(&self) -> PathBuf {
        self.service.state_dir.join("lease")
    }
}

// ---------------------------------------------------------------------------
// 数据源配置
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub source_code: String,
    pub tenant_id: String,
    #[serde(default = "default_env_ref")]
    pub env_ref: String,
    #[serde(default = "default_group")]
    pub group: String,
    #[serde(default)]
    pub regions: RegionsSpec,
    /// 显式地域名表，`regions` 为列表时用于填 `region_name`。
    #[serde(default)]
    pub region_names: BTreeMap<String, String>,
    #[serde(default)]
    pub assets: AssetToggles,
    pub credential: CredentialRefs,
    /// 仅测试/联调用：把四个 API 的 host 全部指向同一个 base URL。
    #[serde(default)]
    pub endpoint_override: Option<String>,
}

fn default_env_ref() -> String {
    "prod".to_string()
}
fn default_group() -> String {
    GROUP_INVENTORY.to_string()
}

/// 地域范围：显式列表，或运行时用 `DescribeRegions` 解析。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RegionsSpec {
    Auto(AutoRegions),
    Explicit(Vec<String>),
}

impl Default for RegionsSpec {
    fn default() -> Self {
        Self::Auto(AutoRegions::Auto)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoRegions {
    Auto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetToggles {
    #[serde(default = "yes")]
    pub cloud_host: bool,
    #[serde(default = "yes")]
    pub eip: bool,
    #[serde(default = "yes")]
    pub vpc: bool,
    #[serde(default = "yes")]
    pub subnet: bool,
}

impl Default for AssetToggles {
    fn default() -> Self {
        Self {
            cloud_host: true,
            eip: true,
            vpc: true,
            subnet: true,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRefs {
    pub secret_id: SecretRef,
    pub secret_key: SecretRef,
}

impl SourceConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|err| {
            ConfigError::new(format!("读取数据源配置 {} 失败: {err}", path.display()))
        })?;
        Self::parse(&raw).map_err(|err| err.context(path))
    }

    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(raw)
            .map_err(|err| ConfigError::new(format!("数据源配置解析失败: {err}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.source_code.trim().is_empty() {
            return Err(ConfigError::new("source_code 不能为空"));
        }
        if self.tenant_id.trim().is_empty() {
            return Err(ConfigError::new("tenant_id 不能为空"));
        }
        if self.group.trim().is_empty() {
            return Err(ConfigError::new("group 不能为空"));
        }
        if let RegionsSpec::Explicit(list) = &self.regions
            && list.is_empty()
        {
            return Err(ConfigError::new(
                "regions 是空列表；要自动解析请写 regions = \"auto\"",
            ));
        }
        self.credential.secret_id.validate()?;
        self.credential.secret_key.validate()?;
        Ok(())
    }

    pub fn vendor(&self) -> Vendor {
        // P0 只有腾讯云；其他厂商出现在配置里要立刻失败，不能静默按腾讯云跑。
        Vendor::TencentCloud
    }

    pub fn scope(&self, regions: Vec<String>) -> Scope {
        Scope::new(
            self.tenant_id.clone(),
            self.source_code.clone(),
            self.group.clone(),
            self.env_ref.clone(),
            self.vendor(),
            regions,
        )
    }

    pub fn enabled_types(&self) -> Vec<crate::model::AssetType> {
        use crate::model::AssetType;
        let mut out = Vec::new();
        if self.assets.cloud_host {
            out.push(AssetType::CloudHost);
        }
        if self.assets.eip {
            out.push(AssetType::Eip);
        }
        if self.assets.vpc {
            out.push(AssetType::Vpc);
        }
        if self.assets.subnet {
            out.push(AssetType::SubNet);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigError {
    pub message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn context(mut self, path: &Path) -> Self {
        self.message = format!("{}: {}", path.display(), self.message);
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = r#"
[service]
spool_dir = "/tmp/spool"
state_dir = "/tmp/state"
page_size = 50

[rate.actions]
DescribeInstances = { qps = 20.0, burst = 10.0 }

[sink]
endpoint = "127.0.0.1:9000"
allow_plaintext = true
"#;

    #[test]
    fn service_config_parses_with_defaults() {
        let cfg = ServiceConfig::parse(SERVICE).expect("配置应可解析");
        assert_eq!(cfg.service.page_size, 50);
        assert_eq!(cfg.retry.max_attempts, 4);
        assert!(!cfg.service.allow_empty);
        assert_eq!(cfg.rate.actions["DescribeInstances"].qps, 20.0);
        assert_eq!(cfg.sink.publish_attempts, 3);
    }

    #[test]
    fn rejects_zero_page_size() {
        let err = ServiceConfig::parse("[service]\npage_size = 0\n").unwrap_err();
        assert!(err.message.contains("page_size"), "{}", err.message);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = ServiceConfig::parse("[service]\nnope = 1\n").unwrap_err();
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    #[test]
    fn source_config_regions_auto_or_list() {
        let auto = r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
regions = "auto"

[credential]
secret_id = { env = "SEC_TENCENT_SECRET_ID" }
secret_key = { env = "SEC_TENCENT_SECRET_KEY" }
"#;
        let cfg = SourceConfig::parse(auto).expect("auto 地域应可解析");
        assert_eq!(cfg.regions, RegionsSpec::Auto(AutoRegions::Auto));
        assert_eq!(cfg.group, "inventory");
        assert_eq!(cfg.env_ref, "prod");

        let list = auto.replace("regions = \"auto\"", "regions = [\"ap-guangzhou\"]");
        let cfg = SourceConfig::parse(&list).expect("显式地域应可解析");
        assert_eq!(
            cfg.regions,
            RegionsSpec::Explicit(vec!["ap-guangzhou".to_string()])
        );
    }

    #[test]
    fn empty_explicit_regions_is_rejected() {
        let raw = r#"
source_code = "s"
tenant_id = "t"
regions = []

[credential]
secret_id = { env = "A" }
secret_key = { env = "B" }
"#;
        assert!(SourceConfig::parse(raw).is_err());
    }

    #[test]
    fn credential_file_must_be_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "topsecret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            let reference = SecretRef::from_file(&file);
            let err = reference.load().unwrap_err();
            assert!(err.message.contains("0600"), "{}", err.message);

            // 属组可写也不能接受（0640 只放宽了「组可读」这一位）。
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o660)).unwrap();
            assert!(reference.load().is_err(), "0660 允许属组写入，必须拒绝");

            // 0640：属组可读 —— 栈里以 :ro 挂进容器、容器以 999 运行只能靠属组读。
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
            let secret = reference.load().expect("0640 应可读");
            assert_eq!(secret.expose(), "topsecret");

            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(secret.expose(), "topsecret");
        }
    }

    #[test]
    fn credential_file_accepts_flat_yaml_and_json() {
        // 扁平 YAML：平台 `.wist/sec-value.yml` 的形态。
        // 值里的 `#` 必须原样保留（不能当行尾注释剥掉）。
        let map = parse_flat_map("admin_token: \"abc#123\"\nother: plain\n# 注释行\n\n").unwrap();
        assert_eq!(map.get("admin_token").unwrap(), "abc#123");
        assert_eq!(map.get("other").unwrap(), "plain");

        // JSON 对象仍然支持
        let map = parse_flat_map("{\"secret_id\":\"x\"}").unwrap();
        assert_eq!(map.get("secret_id").unwrap(), "x");

        // 以下都必须**报错而不是猜**：嵌套（空值/缩进）、JSON 顶层不是对象
        assert!(parse_flat_map("a:\n  b: c\n").is_err(), "嵌套结构应报错");
        assert!(parse_flat_map("a: 1\n  b: 2\n").is_err(), "缩进应报错");
        assert!(parse_flat_map("[1,2]").is_err(), "JSON 数组应报错");
    }

    #[test]
    fn secret_string_debug_is_redacted() {
        let secret = SecretString("abcdef".to_string());
        assert_eq!(format!("{secret:?}"), "SecretString(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
    }
}
