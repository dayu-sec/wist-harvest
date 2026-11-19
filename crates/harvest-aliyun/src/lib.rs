//! 阿里云数据源实现：RPC 签名（`HMAC-SHA256`）+ `ecs`/`vpc` 库存 reader + `sts` 身份。
//!
//! 输出形态与腾讯云、AWS 一致：一个采集组一条完整快照帧
//! （`wist.harvest.snapshot.v1`），走 `harvest-warp` 进数据平面。
//! 成本链路走 BSS OpenAPI 的 `QueryAccountBill`。

pub mod client;
pub mod readers;
pub mod rpc;
pub mod source;
pub mod value;

pub use source::{AliyunSource, REQUIRED_CREDENTIALS, plan_from_config, plan_items};
