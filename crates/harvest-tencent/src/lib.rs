//! 腾讯云数据源实现：TC3 签名 + inventory reader + 账单 reader。
//!
//! 两类采集分属两条主干，**输出形态不同**：
//! - `inventory`：资产快照帧，走数据平面进 atlas；
//! - `cost`：账单分片 + manifest，落对象存储进 `wist-cost`（不进数据平面）。

pub mod client;
pub mod readers;
pub mod source;
pub mod tc3;
pub mod value;

pub use source::{TencentSource, credential_report, plan_items};
