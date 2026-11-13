//! `wist-harvest` 的公共内核：范围身份、spool/manifest、完整性校验、状态机、
//! 租约、限速、脱敏、配置。
//!
//! 这里只放**与云厂商无关**的东西。厂商差异由 `harvest-tencent` 之类的 crate
//! 通过 [`source::Source`] 契约接入；数据平面差异由 [`source::Sink`] 契约接入。

pub mod assemble;
pub mod config;
pub mod cost;
pub mod error;
pub mod integrity;
pub mod lease;
pub mod model;
pub mod page;
pub mod ratelimit;
pub mod redact;
pub mod report;
pub mod run;
pub mod schedule;
pub mod scope;
pub mod source;
pub mod spool;
pub mod state;
pub mod time;
pub mod wire;

pub use error::{ErrorClass, HarvestError};
pub use scope::Scope;

/// 生产者原生格式的 schema 标识（见设计说明 §4.4）。
pub const SCHEMA_NATIVE: &str = "wist.harvest.snapshot.v1";
/// spool manifest 的 schema 标识。
pub const SCHEMA_SPOOL_MANIFEST: &str = "wist.harvest.spool.manifest.v1";
/// 运行报告的 schema 标识。
pub const SCHEMA_RUN_REPORT: &str = "wist.harvest.run.report.v1";
/// 服务名常量：数据平面身份的一部分，换实现不变（设计说明 §4.3）。
pub const SOURCE_SYSTEM: &str = "wist-harvest";
