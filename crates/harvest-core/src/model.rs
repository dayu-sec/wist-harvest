//! 归一后的资产模型与快照帧体。
//!
//! 三条硬约束（设计说明 §4.4 / 交接文档 §3）：
//!
//! 1. **沿用既有字段名**：`ip` / `public_ip` / `vpcid` / `subvpcid` / `ins_charge_type`，
//!    不改名、不做单位换算、不做时间格式化。
//! 2. **三态**：可选字段必须能区分「字段缺席 / `null` / `[]`」，用 [`Presence`] 承载，
//!    不允许用 `Option<Vec<_>>` 把「缺席」和「null」糊成一种。
//! 3. **兼容常量**：`version:"1"`、`risk_level:"MEDIUM"`、`enable:1`、`network_type:""`、
//!    VPC 根节点 `parent_source_asset_id:"-1"`。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `version` 兼容常量。不是云 API 版本。
pub const COMPAT_VERSION: &str = "1";
/// `risk_level` 兼容常量。**不是**实际风险评估结论。
pub const COMPAT_RISK_LEVEL: &str = "MEDIUM";
/// `enable` 兼容常量。
pub const COMPAT_ENABLE: i64 = 1;
/// `network_type` 兼容常量。
pub const COMPAT_NETWORK_TYPE: &str = "";
/// 网络类资产的 `manage_asset_type` / `first_asset_type`。
pub const COMPAT_NET_WORK: &str = "NET_WORK";
/// VPC 作为根节点时的 `parent_source_asset_id`。
pub const COMPAT_VPC_ROOT_PARENT: &str = "-1";

/// 字段三态。序列化时 `Absent` 靠 `skip_serializing_if` 整字段省略，
/// `Null` 输出 `null`，`Present` 输出值。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Presence<T> {
    /// 云响应里没有这个字段。
    #[default]
    Absent,
    /// 云响应里有这个字段，值为 `null`。
    Null,
    /// 有值（可能是空数组）。
    Present(T),
}

impl<T> Presence<T> {
    pub fn absent() -> Self {
        Self::Absent
    }

    pub fn null() -> Self {
        Self::Null
    }

    pub fn present(value: T) -> Self {
        Self::Present(value)
    }

    /// `Some(v)` → `Present`，`None` → `Null`（"字段存在但为空"）。
    /// "字段缺席"只能由 `serde(default)` 产生，刻意不给 `Option` 重载。
    pub fn from_option(value: Option<T>) -> Self {
        match value {
            Some(v) => Self::Present(v),
            None => Self::Null,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Present(v) => Some(v),
            Self::Absent | Self::Null => None,
        }
    }

    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Present(v) => Some(v),
            Self::Absent | Self::Null => None,
        }
    }
}

impl<T: Serialize> Serialize for Presence<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Present(value) => value.serialize(serializer),
            Self::Absent | Self::Null => serializer.serialize_none(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(v) => Self::Present(v),
            None => Self::Null,
        })
    }
}

/// 采集组类型。序列化成帧体里的 `assets` / `stats` 键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetType {
    CloudHost,
    Eip,
    Vpc,
    SubNet,
}

impl AssetType {
    pub const ALL: [AssetType; 4] = [
        AssetType::CloudHost,
        AssetType::Eip,
        AssetType::Vpc,
        AssetType::SubNet,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CloudHost => "CLOUD_HOST",
            Self::Eip => "EIP",
            Self::Vpc => "VPC",
            Self::SubNet => "SUB_NET",
        }
    }
}

impl std::fmt::Display for AssetType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AssetType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "CLOUD_HOST" => Ok(Self::CloudHost),
            "EIP" => Ok(Self::Eip),
            "VPC" => Ok(Self::Vpc),
            "SUB_NET" => Ok(Self::SubNet),
            other => Err(format!("unknown asset type: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Vendor {
    TencentCloud,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TencentCloud => "TENCENT_CLOUD",
        }
    }

    /// `cloud_account` 组合键的前缀（`tencent:<AccountId>`）。
    ///
    /// 与 [`Vendor::as_str`] 分开：那个是上报字段的取值（大写、给人看），这个是
    /// **下游的相等比较键**。改了它等于把所有存量资产改了一个账号身份，
    /// 存量行会永久失配（退不了役），所以不要跟着展示层一起改。
    pub fn key(self) -> &'static str {
        match self {
            Self::TencentCloud => "tencent",
        }
    }
}

/// 云标签。只保留 `Key`/`Value`，去掉 `tagKey`/`tagValue` 别名。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tag {
    #[serde(rename = "Key", default)]
    pub key: String,
    #[serde(rename = "Value", default)]
    pub value: String,
}

/// 云主机（交接文档 §3.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudHost {
    pub host_id: String,
    pub host_name: String,
    pub source_asset_id: String,
    pub asset_code: String,
    pub asset_name: String,
    pub asset_status: String,
    pub host_state: String,
    pub restrict_state: String,
    /// 第一个私网地址；兼容字段，不要改成 `private_ip`。
    pub ip: String,
    /// 第一个公网地址；没有则空字符串。
    pub public_ip: String,
    pub private_ips: Vec<String>,
    pub public_ips: Vec<String>,
    pub ipv6_addresses: Vec<String>,
    pub security_group_ids: Vec<String>,
    /// 三态：缺席 / null / 数组。
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub tags: Presence<Vec<Tag>>,
    pub cpu_core_num: i64,
    pub memory_capacity: i64,
    pub os_name: String,
    pub ins_type: String,
    pub ins_charge_type: String,
    pub ins_charge_type_name: String,
    pub version: String,
    pub create_time: String,
    pub expired_time: String,
    pub region: String,
    pub region_name: String,
    pub zone: String,
    /// 兼容字段名。
    pub vpcid: String,
    /// 兼容字段名。
    pub subvpcid: String,
    pub project_id: String,
    pub project_name: String,
}

impl Default for CloudHost {
    fn default() -> Self {
        Self {
            host_id: String::new(),
            host_name: String::new(),
            source_asset_id: String::new(),
            asset_code: String::new(),
            asset_name: String::new(),
            asset_status: String::new(),
            host_state: String::new(),
            restrict_state: String::new(),
            ip: String::new(),
            public_ip: String::new(),
            private_ips: Vec::new(),
            public_ips: Vec::new(),
            ipv6_addresses: Vec::new(),
            security_group_ids: Vec::new(),
            tags: Presence::Absent,
            cpu_core_num: 0,
            memory_capacity: 0,
            os_name: String::new(),
            ins_type: String::new(),
            ins_charge_type: String::new(),
            ins_charge_type_name: String::new(),
            version: COMPAT_VERSION.to_string(),
            create_time: String::new(),
            expired_time: String::new(),
            region: String::new(),
            region_name: String::new(),
            zone: String::new(),
            vpcid: String::new(),
            subvpcid: String::new(),
            project_id: String::new(),
            project_name: String::new(),
        }
    }
}

/// 公网 IP（交接文档 §3.3）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eip {
    pub source_asset_id: String,
    pub asset_name: String,
    pub eip_address_type: String,
    pub eip_address_ip: String,
    pub eip_address_status: String,
    pub eip_instance_type: String,
    pub eip_instance_id: String,
    pub eip_provider: String,
    pub eip_bandwidth: i64,
    pub eip_charge_type: String,
    pub deadline_date: String,
    pub region: String,
}

/// VPC（交接文档 §3.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vpc {
    pub source_asset_id: String,
    pub manage_asset_type: String,
    pub first_asset_type: String,
    pub asset_type: String,
    pub asset_code: String,
    pub asset_name: String,
    pub parent_source_asset_id: String,
    pub cidr: String,
    pub cidrv6: String,
    pub network_type: String,
    pub risk_level: String,
    pub region: String,
    pub region_name: String,
    pub enable: i64,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub cloud_create_time: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub tags: Presence<Vec<Tag>>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub is_default: Presence<bool>,
    /// 保留每项原始结构，不做投影。
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub assistant_cidr_set: Presence<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub ipv6_cidr_block_set: Presence<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub enable_dhcp: Presence<bool>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub dhcp_options_id: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub dns_servers: Presence<Vec<String>>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub domain_name: Presence<String>,
}

impl Default for Vpc {
    fn default() -> Self {
        Self {
            source_asset_id: String::new(),
            manage_asset_type: COMPAT_NET_WORK.to_string(),
            first_asset_type: COMPAT_NET_WORK.to_string(),
            asset_type: "VPC".to_string(),
            asset_code: String::new(),
            asset_name: String::new(),
            parent_source_asset_id: COMPAT_VPC_ROOT_PARENT.to_string(),
            cidr: String::new(),
            cidrv6: String::new(),
            network_type: COMPAT_NETWORK_TYPE.to_string(),
            risk_level: COMPAT_RISK_LEVEL.to_string(),
            region: String::new(),
            region_name: String::new(),
            enable: COMPAT_ENABLE,
            cloud_create_time: Presence::Absent,
            tags: Presence::Absent,
            is_default: Presence::Absent,
            assistant_cidr_set: Presence::Absent,
            ipv6_cidr_block_set: Presence::Absent,
            enable_dhcp: Presence::Absent,
            dhcp_options_id: Presence::Absent,
            dns_servers: Presence::Absent,
            domain_name: Presence::Absent,
        }
    }
}

/// 子网（交接文档 §3.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subnet {
    pub source_asset_id: String,
    pub manage_asset_type: String,
    pub first_asset_type: String,
    pub asset_type: String,
    pub asset_code: String,
    pub asset_name: String,
    /// 所属 VPC ID，必须有值。
    pub parent_source_asset_id: String,
    pub cidr: String,
    pub cidrv6: String,
    pub network_type: String,
    pub risk_level: String,
    pub region: String,
    pub region_name: String,
    pub zone: String,
    pub enable: i64,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub cloud_create_time: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub tags: Presence<Vec<Tag>>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub is_default: Presence<bool>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub route_table_id: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub network_acl_id: Presence<String>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub available_ip_address_count: Presence<i64>,
    #[serde(default, skip_serializing_if = "Presence::is_absent")]
    pub total_ip_address_count: Presence<i64>,
}

impl Default for Subnet {
    fn default() -> Self {
        Self {
            source_asset_id: String::new(),
            manage_asset_type: COMPAT_NET_WORK.to_string(),
            first_asset_type: COMPAT_NET_WORK.to_string(),
            asset_type: "SUB_NET".to_string(),
            asset_code: String::new(),
            asset_name: String::new(),
            parent_source_asset_id: String::new(),
            cidr: String::new(),
            cidrv6: String::new(),
            network_type: COMPAT_NETWORK_TYPE.to_string(),
            risk_level: COMPAT_RISK_LEVEL.to_string(),
            region: String::new(),
            region_name: String::new(),
            zone: String::new(),
            enable: COMPAT_ENABLE,
            cloud_create_time: Presence::Absent,
            tags: Presence::Absent,
            is_default: Presence::Absent,
            route_table_id: Presence::Absent,
            network_acl_id: Presence::Absent,
            available_ip_address_count: Presence::Absent,
            total_ip_address_count: Presence::Absent,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Assets {
    #[serde(rename = "CLOUD_HOST")]
    pub cloud_host: Vec<CloudHost>,
    #[serde(rename = "EIP")]
    pub eip: Vec<Eip>,
    #[serde(rename = "VPC")]
    pub vpc: Vec<Vpc>,
    #[serde(rename = "SUB_NET")]
    pub sub_net: Vec<Subnet>,
}

/// `stats` 是完整性证据，不是下游必需字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TypeStat {
    pub total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<BTreeMap<String, u64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    #[serde(rename = "CLOUD_HOST")]
    pub cloud_host: TypeStat,
    #[serde(rename = "EIP")]
    pub eip: TypeStat,
    #[serde(rename = "VPC")]
    pub vpc: TypeStat,
    #[serde(rename = "SUB_NET")]
    pub sub_net: TypeStat,
}

impl Stats {
    pub fn get(&self, asset_type: AssetType) -> &TypeStat {
        match asset_type {
            AssetType::CloudHost => &self.cloud_host,
            AssetType::Eip => &self.eip,
            AssetType::Vpc => &self.vpc,
            AssetType::SubNet => &self.sub_net,
        }
    }
}

/// 生产者原生格式的一帧（一条消息 = 一份完整快照）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeSnapshot {
    pub schema_native: String,
    pub tenant_id: String,
    pub source_code: String,
    pub env_ref: String,
    pub snap_id: String,
    pub vendor: Vendor,
    pub collected_at: String,
    pub regions: Vec<String>,
    pub assets: Assets,
    pub stats: Stats,
    /// 覆盖范围声明。下游据此判断「快照里没有它」能不能当「已经不存在」。
    pub coverage: Coverage,
}

/// 这一帧覆盖了什么，以及能不能当全集用。
///
/// 为什么必须随帧发：`快照里没有某台机器` 有两种完全不同的原因 —— 它被销毁了，
/// 或者这次没采全（地域超时、限流、分页断、只开了一部分类型）。消费方只看数据
/// 区分不出来，只能二选一：把采集不全当销毁（误删资产），或者永远不退役。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub scope: CoverageScope,
    /// 采集器自身的完整性校验是否通过。
    ///
    /// 由运行器在 `verify` 之后置真（见 `run::fill_and_publish`）：不完整的帧根本
    /// 不会走到组装/发布，所以这里写进去的永远是已校验的结论。
    pub complete: bool,
    /// 本帧覆盖的云账号键（`vendor:AccountId`）。取不到就是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl Coverage {
    /// 保守默认：什么也没声明。下游会因此只 upsert、不判死。
    pub fn partial() -> Self {
        Self {
            scope: CoverageScope::Partial,
            complete: false,
            account: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverageScope {
    /// 账号级全集：所有可见地域、所有配置的资产类型都采到了。
    Account,
    /// 部分覆盖（手写地域子集等）：「没有」什么也不证明。
    Partial,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_serializes_the_shape_atlas_reads() {
        // 下游按 `scope` / `complete` / `account` 三个键做判定，名字不能改。
        let full = Coverage {
            scope: CoverageScope::Account,
            complete: true,
            account: Some("tencent:1300000000".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&full).unwrap(),
            serde_json::json!({
                "scope": "account",
                "complete": true,
                "account": "tencent:1300000000"
            })
        );

        // 没账号就不输出这个键：`null` 和「没声明」在下游是同一个意思（不退役），
        // 但少一个 null 更不容易被误读成「账号是空的」。
        let partial = Coverage::partial();
        assert_eq!(
            serde_json::to_value(&partial).unwrap(),
            serde_json::json!({ "scope": "partial", "complete": false })
        );
    }

    #[test]
    fn vendor_key_is_the_account_prefix_not_the_report_value() {
        // `as_str` 是上报给人看的；`key` 是下游相等比较用的 `cloud_account` 前缀。
        assert_eq!(Vendor::TencentCloud.as_str(), "TENCENT_CLOUD");
        assert_eq!(Vendor::TencentCloud.key(), "tencent");
    }
}
