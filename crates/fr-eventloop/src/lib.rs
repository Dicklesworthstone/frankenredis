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
pub enum PhaseReplayError {
    EmptyTrace,
    MissingMainLoopEntry {
        first: EventLoopPhase,
    },
    StageTransitionInvalid {
        from: EventLoopPhase,
        to: EventLoopPhase,
    },
    PartialTick {
        observed: usize,
    },
}

impl PhaseReplayError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::EmptyTrace | Self::MissingMainLoopEntry { .. } => {
                "eventloop.main_loop_entry_missing"
            }
            Self::StageTransitionInvalid { .. } => "eventloop.dispatch.stage_transition_invalid",
            Self::PartialTick { .. } => "eventloop.dispatch.order_mismatch",
        }
    }
}

#[must_use]
pub const fn next_phase(phase: EventLoopPhase) -> EventLoopPhase {
    match phase {
        EventLoopPhase::BeforeSleep => EventLoopPhase::Poll,
        EventLoopPhase::Poll => EventLoopPhase::FileDispatch,
        EventLoopPhase::FileDispatch => EventLoopPhase::TimeDispatch,
        EventLoopPhase::TimeDispatch => EventLoopPhase::AfterSleep,
        EventLoopPhase::AfterSleep => EventLoopPhase::BeforeSleep,
    }
}

pub fn replay_phase_trace(trace: &[EventLoopPhase]) -> Result<usize, PhaseReplayError> {
    let Some((&first, rest)) = trace.split_first() else {
        return Err(PhaseReplayError::EmptyTrace);
    };
    if first != EventLoopPhase::BeforeSleep {
        return Err(PhaseReplayError::MissingMainLoopEntry { first });
    }

    let mut completed_ticks = 0usize;
    let mut current = first;
    for &next in rest {
        let expected = next_phase(current);
        if next != expected {
            return Err(PhaseReplayError::StageTransitionInvalid {
                from: current,
                to: next,
            });
        }
        if current == EventLoopPhase::AfterSleep {
            completed_ticks = completed_ticks.saturating_add(1);
        }
        current = next;
    }
    if current != EventLoopPhase::AfterSleep {
        return Err(PhaseReplayError::PartialTick {
            observed: trace.len(),
        });
    }
    Ok(completed_ticks.saturating_add(1))
}

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
    use super::{
        EVENT_LOOP_PHASE_ORDER, EventLoopMode, EventLoopPhase, PhaseReplayError, TickBudget,
        plan_tick, replay_phase_trace, run_tick,
    };

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

    #[test]
    fn fr_p2c_001_u011_replay_trace_accepts_single_tick() {
        let ticks = replay_phase_trace(&EVENT_LOOP_PHASE_ORDER).expect("single tick");
        assert_eq!(ticks, 1);
    }

    #[test]
    fn fr_p2c_001_u011_replay_trace_accepts_multiple_ticks() {
        let mut trace = Vec::new();
        trace.extend_from_slice(&EVENT_LOOP_PHASE_ORDER);
        trace.extend_from_slice(&EVENT_LOOP_PHASE_ORDER);
        let ticks = replay_phase_trace(&trace).expect("two ticks");
        assert_eq!(ticks, 2);
    }

    #[test]
    fn fr_p2c_001_u011_replay_trace_rejects_invalid_transition() {
        let err = replay_phase_trace(&[
            EventLoopPhase::BeforeSleep,
            EventLoopPhase::FileDispatch,
            EventLoopPhase::AfterSleep,
        ])
        .expect_err("invalid transition");
        assert_eq!(
            err,
            PhaseReplayError::StageTransitionInvalid {
                from: EventLoopPhase::BeforeSleep,
                to: EventLoopPhase::FileDispatch
            }
        );
        assert_eq!(
            err.reason_code(),
            "eventloop.dispatch.stage_transition_invalid"
        );
    }

    #[test]
    fn fr_p2c_001_u011_replay_trace_rejects_missing_main_loop_entry() {
        let err = replay_phase_trace(&[EventLoopPhase::Poll]).expect_err("missing entry");
        assert_eq!(
            err,
            PhaseReplayError::MissingMainLoopEntry {
                first: EventLoopPhase::Poll
            }
        );
        assert_eq!(err.reason_code(), "eventloop.main_loop_entry_missing");
    }

    #[test]
    fn fr_p2c_001_u011_replay_trace_rejects_partial_tick() {
        let err = replay_phase_trace(&[
            EventLoopPhase::BeforeSleep,
            EventLoopPhase::Poll,
            EventLoopPhase::FileDispatch,
        ])
        .expect_err("partial tick");
        assert_eq!(err, PhaseReplayError::PartialTick { observed: 3 });
        assert_eq!(err.reason_code(), "eventloop.dispatch.order_mismatch");
    }
}
