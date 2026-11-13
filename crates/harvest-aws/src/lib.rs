//! AWS 数据源实现：SigV4 签名 + `ec2` 库存 reader + `sts` 身份。
//!
//! 输出形态与腾讯云一致：一个采集组一条完整快照帧（`wist.harvest.snapshot.v1`），
//! 走 `harvest-warp` 进数据平面。成本链路（CUR / Cost Explorer）尚未实现。

pub mod client;
pub mod readers;
pub mod sigv4;
pub mod source;

pub use source::{AwsSource, REQUIRED_CREDENTIALS, plan_from_config, plan_items};
