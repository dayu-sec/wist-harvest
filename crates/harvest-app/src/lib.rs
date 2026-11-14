//! `wist-harvest` 的可测试部分。
//!
//! bin 只负责解析参数；`once` / `run` / `dry-run` / `diagnose` 的逻辑都放在
//! lib 里，便于集成测试直接断言（尤其是退出码与"无凭据泄漏"）。

pub mod bootstrap;
pub mod cli;
pub mod cost;
pub mod diagnose;
pub mod dryrun;
pub mod gateway;
pub mod once;
pub mod round;
pub mod serve;
pub mod vendor;
