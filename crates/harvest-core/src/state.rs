//! 运行状态机：**不完整绝不发送**。
//!
//! ```text
//! Planned -> Fetching -> Spooled -> Verified -> Published -> Done
//!    |          |           |          |
//!    +----------+-----------+----------+--> Aborted（保留 spool + 原因，不发送）
//! ```
//!
//! 另有 `Abandoned`：本次运行**放弃**旧 spool（典型是采集计划变了），保留目录供诊断，
//! 但不再参与续跑。它与 `Aborted` 的区别是「不再重试」。

use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, HarvestError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Planned,
    Fetching,
    Spooled,
    Verified,
    Published,
    Done,
    Aborted,
    /// 主动放弃：保留目录供诊断，但**不再续跑**。
    ///
    /// 和 `Aborted` 分开是必须的：`Aborted` 的语义是「这次没采全，下次接着采」，
    /// 而计划变更（改了 `[assets]` 开关）之后的旧 spool 永远不可能变合法，
    /// 如果它继续可续跑，采集器会每轮都撞在同一个校验错误上，永不发布。
    Abandoned,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "Planned",
            Self::Fetching => "Fetching",
            Self::Spooled => "Spooled",
            Self::Verified => "Verified",
            Self::Published => "Published",
            Self::Done => "Done",
            Self::Aborted => "Aborted",
            Self::Abandoned => "Abandoned",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Aborted | Self::Abandoned)
    }

    /// 允许的下一步。
    pub fn successors(self) -> &'static [RunState] {
        match self {
            Self::Planned => &[RunState::Fetching, RunState::Aborted],
            Self::Fetching => &[RunState::Spooled, RunState::Aborted],
            Self::Spooled => &[RunState::Verified, RunState::Aborted],
            Self::Verified => &[RunState::Published, RunState::Aborted],
            Self::Published => &[RunState::Done],
            Self::Done | Self::Aborted | Self::Abandoned => &[],
        }
    }

    pub fn can_transition_to(self, next: RunState) -> bool {
        self.successors().contains(&next)
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct StateMachine {
    state: RunState,
    trail: Vec<RunState>,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            state: RunState::Planned,
            trail: vec![RunState::Planned],
        }
    }

    /// 从 spool manifest 的既有状态恢复（续跑）。
    pub fn resume_from(state: RunState) -> Self {
        Self {
            state,
            trail: vec![state],
        }
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    pub fn trail(&self) -> Vec<RunState> {
        self.trail.clone()
    }

    pub fn advance(&mut self, next: RunState) -> Result<(), HarvestError> {
        if !self.state.can_transition_to(next) {
            return Err(HarvestError::new(
                ErrorClass::Other,
                format!("非法状态迁移: {} -> {}", self.state, next),
            ));
        }
        self.state = next;
        self.trail.push(next);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let mut sm = StateMachine::new();
        for next in [
            RunState::Fetching,
            RunState::Spooled,
            RunState::Verified,
            RunState::Published,
            RunState::Done,
        ] {
            sm.advance(next).expect("合法迁移");
        }
        assert!(sm.state().is_terminal());
    }

    #[test]
    fn any_pre_publish_state_can_abort() {
        for from in [
            RunState::Planned,
            RunState::Fetching,
            RunState::Spooled,
            RunState::Verified,
        ] {
            let mut sm = StateMachine::resume_from(from);
            sm.advance(RunState::Aborted).expect("应允许中止");
        }
    }

    #[test]
    fn cannot_skip_states() {
        let mut sm = StateMachine::new();
        assert!(sm.advance(RunState::Verified).is_err());
        assert!(sm.advance(RunState::Published).is_err());
    }

    #[test]
    fn published_cannot_abort() {
        let mut sm = StateMachine::resume_from(RunState::Published);
        assert!(sm.advance(RunState::Aborted).is_err());
        sm.advance(RunState::Done).expect("可以收尾");
    }
}
