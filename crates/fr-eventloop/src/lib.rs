#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickBudget {
    pub max_accepts: usize,
    pub max_commands: usize,
}

impl Default for TickBudget {
    fn default() -> Self {
        Self {
            max_accepts: 64,
            max_commands: 4096,
        }
    }
}

impl TickBudget {
    pub const BLOCKED_MODE_MAX_ACCEPTS: usize = 1;
    pub const BLOCKED_MODE_MAX_COMMANDS: usize = 128;

    #[must_use]
    pub fn bounded_for_blocked_mode(self) -> Self {
        Self {
            max_accepts: self.max_accepts.min(Self::BLOCKED_MODE_MAX_ACCEPTS),
            max_commands: self.max_commands.min(Self::BLOCKED_MODE_MAX_COMMANDS),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLoopMode {
    Normal,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLoopPhase {
    BeforeSleep,
    Poll,
    FileDispatch,
    TimeDispatch,
    AfterSleep,
}

pub const EVENT_LOOP_PHASE_ORDER: [EventLoopPhase; 5] = [
    EventLoopPhase::BeforeSleep,
    EventLoopPhase::Poll,
    EventLoopPhase::FileDispatch,
    EventLoopPhase::TimeDispatch,
    EventLoopPhase::AfterSleep,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    pub accepted: usize,
    pub processed_commands: usize,
    pub accept_backlog_remaining: usize,
    pub command_backlog_remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickPlan {
    pub mode: EventLoopMode,
    pub poll_timeout_ms: u64,
    pub phase_order: [EventLoopPhase; 5],
    pub stats: TickStats,
}

#[must_use]
pub fn run_tick(pending_accepts: usize, pending_commands: usize, budget: TickBudget) -> TickStats {
    let accepted = pending_accepts.min(budget.max_accepts);
    let processed_commands = pending_commands.min(budget.max_commands);
    TickStats {
        accepted,
        processed_commands,
        accept_backlog_remaining: pending_accepts.saturating_sub(accepted),
        command_backlog_remaining: pending_commands.saturating_sub(processed_commands),
    }
}

#[must_use]
pub fn plan_tick(
    pending_accepts: usize,
    pending_commands: usize,
    budget: TickBudget,
    mode: EventLoopMode,
) -> TickPlan {
    let effective_budget = match mode {
        EventLoopMode::Normal => budget,
        EventLoopMode::Blocked => budget.bounded_for_blocked_mode(),
    };
    let stats = run_tick(pending_accepts, pending_commands, effective_budget);
    let poll_timeout_ms = match mode {
        EventLoopMode::Blocked => 0,
        EventLoopMode::Normal if pending_accepts > 0 || pending_commands > 0 => 0,
        EventLoopMode::Normal => 10,
    };

    TickPlan {
        mode,
        poll_timeout_ms,
        phase_order: EVENT_LOOP_PHASE_ORDER,
        stats,
    }
}

#[cfg(test)]
mod tests {
    use super::{EVENT_LOOP_PHASE_ORDER, EventLoopMode, TickBudget, plan_tick, run_tick};

    #[test]
    fn tick_respects_budget() {
        let stats = run_tick(
            100,
            10_000,
            TickBudget {
                max_accepts: 10,
                max_commands: 500,
            },
        );
        assert_eq!(stats.accepted, 10);
        assert_eq!(stats.processed_commands, 500);
        assert_eq!(stats.accept_backlog_remaining, 90);
        assert_eq!(stats.command_backlog_remaining, 9_500);
    }

    #[test]
    fn fr_p2c_001_u001_phase_order_is_deterministic() {
        let plan = plan_tick(0, 0, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.phase_order, EVENT_LOOP_PHASE_ORDER);
    }

    #[test]
    fn fr_p2c_001_u003_no_sleep_path_sets_zero_poll_timeout() {
        let plan = plan_tick(0, 7, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.poll_timeout_ms, 0);
    }

    #[test]
    fn fr_p2c_001_u003_idle_path_uses_blocking_timeout() {
        let plan = plan_tick(0, 0, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.poll_timeout_ms, 10);
    }

    #[test]
    fn fr_p2c_001_u005_blocked_mode_bounded_scope() {
        let plan = plan_tick(
            100,
            10_000,
            TickBudget {
                max_accepts: 64,
                max_commands: 4_096,
            },
            EventLoopMode::Blocked,
        );
        assert_eq!(plan.poll_timeout_ms, 0);
        assert_eq!(plan.stats.accepted, TickBudget::BLOCKED_MODE_MAX_ACCEPTS);
        assert_eq!(
            plan.stats.processed_commands,
            TickBudget::BLOCKED_MODE_MAX_COMMANDS
        );
    }

    #[test]
    fn fr_p2c_001_u005_blocked_mode_does_not_expand_budget() {
        let plan = plan_tick(
            100,
            100,
            TickBudget {
                max_accepts: 0,
                max_commands: 7,
            },
            EventLoopMode::Blocked,
        );
        assert_eq!(plan.stats.accepted, 0);
        assert_eq!(plan.stats.processed_commands, 7);
    }
}
