//! 腾讯云数据源实现：TC3 签名 + 四个 inventory reader。
//!
//! P0 只做 `inventory` 组（云主机 / 公网 IP / VPC / 子网）。
//! 成本 reader（`DescribeCostDetail` 等）是下一阶段，这里不提供入口。

pub mod client;
pub mod readers;
pub mod source;
pub mod tc3;
pub mod value;

pub use source::{TencentSource, credential_report, plan_items};
