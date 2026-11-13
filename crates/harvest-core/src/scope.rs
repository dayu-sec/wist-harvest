//! 范围身份：采集组 + 数据源 + 地域集合。
//!
//! **不含客户 / 租户**：一个数据面（网关）只服务一个客户，客户身份由发送通道
//! （wparse 的全局 tag）在入库时打上，采集器不感知。
//!
//! 三条约束（设计说明 §4.2/§4.3）：
//!
//! - 采集组体现在 `source.producer` 后缀里（`{source_code}:{group}`），**不进** `schema`；
//! - `producer` 不含实例 ID、版本号、时间；
//! - `snap_id = {producer}-{scope_hash}-{UTC yyyyMMddTHHmmssZ}`，同一轮重试沿用，
//!   新一轮必须换新。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::Vendor;

/// 采集组常量。
pub const GROUP_INVENTORY: &str = "inventory";
/// 暴露面组（P2 起，P0 只做登记）。
pub const GROUP_EXPOSURE: &str = "exposure";
/// 成本组（下一阶段）。
pub const GROUP_COST: &str = "cost";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub source_code: String,
    pub group: String,
    pub env_ref: String,
    pub vendor: Vendor,
    /// 已解析的地域列表（排序去重后使用）。
    pub regions: Vec<String>,
}

impl Scope {
    pub fn new(
        source_code: impl Into<String>,
        group: impl Into<String>,
        env_ref: impl Into<String>,
        vendor: Vendor,
        mut regions: Vec<String>,
    ) -> Self {
        regions.sort();
        regions.dedup();
        Self {
            source_code: source_code.into(),
            group: group.into(),
            env_ref: env_ref.into(),
            vendor,
            regions,
        }
    }

    /// `{source_code}:{group}`。数据平面作用域隔离靠它。
    pub fn producer(&self) -> String {
        format!("{}:{}", self.source_code, self.group)
    }

    /// 前 6 位 hex，用于 snap_id。
    ///
    /// 覆盖范围变了（比如多了一个地域）就是另一个范围，必须换 hash。
    pub fn scope_hash(&self) -> String {
        let mut canonical = String::new();
        canonical.push_str(&self.producer());
        canonical.push('\n');
        canonical.push_str(&self.env_ref);
        canonical.push('\n');
        canonical.push_str(self.vendor.as_str());
        canonical.push('\n');
        canonical.push_str(&self.regions.join(","));
        let digest = Sha256::digest(canonical.as_bytes());
        hex::encode(digest)[..6].to_string()
    }

    /// 新一轮运行的 snap_id。
    ///
    /// `producer` 里的 `:` 换成 `-`：snap_id 要当目录名用。
    pub fn new_snap_id(&self, at: DateTime<Utc>) -> String {
        format!(
            "{}-{}-{}",
            sanitize(&self.producer()),
            self.scope_hash(),
            crate::time::snap_timestamp(at)
        )
    }

    /// spool 下的相对目录：`<producer>`。
    pub fn spool_prefix(&self) -> String {
        sanitize(&self.producer())
    }
}

/// 目录名安全化：只保留字母数字与 `-_.`，其余换 `-`。
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::from_unix;

    fn scope(regions: Vec<&str>) -> Scope {
        Scope::new(
            "tencent-cloud-prod",
            GROUP_INVENTORY,
            "prod",
            Vendor::TencentCloud,
            regions.into_iter().map(str::to_string).collect(),
        )
    }

    #[test]
    fn producer_carries_group() {
        assert_eq!(scope(vec![]).producer(), "tencent-cloud-prod:inventory");
    }

    #[test]
    fn scope_hash_is_stable_and_region_sensitive() {
        let a = scope(vec!["ap-guangzhou", "ap-shanghai"]);
        let b = scope(vec!["ap-shanghai", "ap-guangzhou"]);
        assert_eq!(a.scope_hash(), b.scope_hash(), "地域顺序不影响范围身份");
        assert_eq!(a.scope_hash().len(), 6);

        let c = scope(vec!["ap-guangzhou"]);
        assert_ne!(a.scope_hash(), c.scope_hash(), "地域集合变了就是另一个范围");
    }

    #[test]
    fn snap_id_shape() {
        let id = scope(vec!["ap-guangzhou"]).new_snap_id(from_unix(1_760_000_000));
        assert!(id.starts_with("tencent-cloud-prod-inventory-"), "{id}");
        assert!(id.ends_with("-20251009T085320Z"), "{id}");
        assert!(!id.contains(':'), "snap_id 要当目录名，不能含冒号: {id}");
    }
}
