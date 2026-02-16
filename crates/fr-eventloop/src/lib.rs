#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBootstrap {
    pub before_sleep_hook_installed: bool,
    pub after_sleep_hook_installed: bool,
    pub server_cron_timer_installed: bool,
}

impl LoopBootstrap {
    #[must_use]
    pub const fn fully_wired() -> Self {
        Self {
            before_sleep_hook_installed: true,
            after_sleep_hook_installed: true,
            server_cron_timer_installed: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapError {
    BeforeSleepHookMissing,
    AfterSleepHookMissing,
    ServerCronTimerMissing,
}

impl BootstrapError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::BeforeSleepHookMissing | Self::AfterSleepHookMissing => {
                "eventloop.hook_install_missing"
            }
            Self::ServerCronTimerMissing => "eventloop.server_cron_timer_missing",
        }
    }
}

pub fn validate_bootstrap(bootstrap: LoopBootstrap) -> Result<(), BootstrapError> {
    if !bootstrap.before_sleep_hook_installed {
        return Err(BootstrapError::BeforeSleepHookMissing);
    }
    if !bootstrap.after_sleep_hook_installed {
        return Err(BootstrapError::AfterSleepHookMissing);
    }
    if !bootstrap.server_cron_timer_installed {
        return Err(BootstrapError::ServerCronTimerMissing);
    }
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessCallback {
    Readable,
    Writable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackDispatchOrder {
    pub first: Option<ReadinessCallback>,
    pub second: Option<ReadinessCallback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierOrderError {
    AeBarrierViolation,
}

impl BarrierOrderError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::AeBarrierViolation => "eventloop.ae_barrier_violation",
        }
    }
}

#[must_use]
pub const fn plan_readiness_callback_order(
    readable_ready: bool,
    writable_ready: bool,
    ae_barrier: bool,
) -> CallbackDispatchOrder {
    match (readable_ready, writable_ready, ae_barrier) {
        (false, false, _) => CallbackDispatchOrder {
            first: None,
            second: None,
        },
        (true, false, _) => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Readable),
            second: None,
        },
        (false, true, _) => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Writable),
            second: None,
        },
        (true, true, true) => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Writable),
            second: Some(ReadinessCallback::Readable),
        },
        (true, true, false) => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Readable),
            second: Some(ReadinessCallback::Writable),
        },
    }
}

pub fn validate_ae_barrier_order(
    readable_ready: bool,
    writable_ready: bool,
    ae_barrier: bool,
    observed: CallbackDispatchOrder,
) -> Result<(), BarrierOrderError> {
    if readable_ready && writable_ready && ae_barrier {
        let expected = CallbackDispatchOrder {
            first: Some(ReadinessCallback::Writable),
            second: Some(ReadinessCallback::Readable),
        };
        if observed != expected {
            return Err(BarrierOrderError::AeBarrierViolation);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdRegistrationError {
    FdOutOfRange {
        fd: usize,
        setsize: usize,
    },
    FdResizeFailure {
        requested_fd: usize,
        max_setsize: usize,
    },
}

impl FdRegistrationError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::FdOutOfRange { .. } => "eventloop.fd_out_of_range",
            Self::FdResizeFailure { .. } => "eventloop.fd_resize_failure",
        }
    }
}

pub fn validate_fd_registration_bounds(
    fd: usize,
    setsize: usize,
) -> Result<(), FdRegistrationError> {
    if fd >= setsize {
        return Err(FdRegistrationError::FdOutOfRange { fd, setsize });
    }
    Ok(())
}

pub fn plan_fd_setsize_growth(
    current_setsize: usize,
    requested_fd: usize,
    max_setsize: usize,
) -> Result<usize, FdRegistrationError> {
    let required_setsize = requested_fd.saturating_add(1);
    if required_setsize > max_setsize {
        return Err(FdRegistrationError::FdResizeFailure {
            requested_fd,
            max_setsize,
        });
    }
    if requested_fd < current_setsize {
        return Ok(current_setsize);
    }

    let mut next_setsize = current_setsize.max(1);
    while next_setsize < required_setsize {
        if next_setsize >= max_setsize {
            break;
        }
        next_setsize = next_setsize.saturating_mul(2).min(max_setsize);
    }
    if next_setsize < required_setsize {
        return Err(FdRegistrationError::FdResizeFailure {
            requested_fd,
            max_setsize,
        });
    }
    Ok(next_setsize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptPathError {
    MaxClientsReached {
        current_clients: usize,
        max_clients: usize,
    },
    HandlerBindFailure,
}

impl AcceptPathError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::MaxClientsReached { .. } => "eventloop.accept.maxclients_reached",
            Self::HandlerBindFailure => "eventloop.accept.handler_bind_failure",
        }
    }
}

pub fn validate_accept_path(
    current_clients: usize,
    max_clients: usize,
    read_handler_bound: bool,
) -> Result<(), AcceptPathError> {
    if current_clients >= max_clients {
        return Err(AcceptPathError::MaxClientsReached {
            current_clients,
            max_clients,
        });
    }
    if !read_handler_bound {
        return Err(AcceptPathError::HandlerBindFailure);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPathError {
    QueryBufferLimitExceeded { observed: usize, limit: usize },
    FatalErrorDisconnect,
}

impl ReadPathError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::QueryBufferLimitExceeded { .. } => "eventloop.read.querybuf_limit_exceeded",
            Self::FatalErrorDisconnect => "eventloop.read.fatal_error_disconnect",
        }
    }
}

pub fn validate_read_path(
    current_query_buffer_len: usize,
    newly_read_bytes: usize,
    query_buffer_limit: usize,
    fatal_read_error: bool,
) -> Result<usize, ReadPathError> {
    if fatal_read_error {
        return Err(ReadPathError::FatalErrorDisconnect);
    }
    let next_query_buffer_len = current_query_buffer_len.saturating_add(newly_read_bytes);
    if next_query_buffer_len > query_buffer_limit {
        return Err(ReadPathError::QueryBufferLimitExceeded {
            observed: next_query_buffer_len,
            limit: query_buffer_limit,
        });
    }
    Ok(next_query_buffer_len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingWriteError {
    FlushOrderViolation { client_id: u64 },
    PendingReplyLost { client_id: u64 },
}

impl PendingWriteError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::FlushOrderViolation { .. } => "eventloop.write.flush_order_violation",
            Self::PendingReplyLost { .. } => "eventloop.write.pending_reply_lost",
        }
    }
}

pub fn validate_pending_write_delivery(
    queued_before_flush: &[u64],
    flushed_now: &[u64],
    pending_after_flush: &[u64],
) -> Result<(), PendingWriteError> {
    let mut queue_positions = BTreeMap::new();
    for (idx, client_id) in queued_before_flush.iter().copied().enumerate() {
        if queue_positions.insert(client_id, idx).is_some() {
            return Err(PendingWriteError::FlushOrderViolation { client_id });
        }
    }

    let mut seen = BTreeSet::new();
    validate_delivery_slice(flushed_now, &queue_positions, &mut seen)?;
    validate_delivery_slice(pending_after_flush, &queue_positions, &mut seen)?;

    for client_id in queued_before_flush {
        if !seen.contains(client_id) {
            return Err(PendingWriteError::PendingReplyLost {
                client_id: *client_id,
            });
        }
    }

    Ok(())
}

fn validate_delivery_slice(
    sequence: &[u64],
    queue_positions: &BTreeMap<u64, usize>,
    seen: &mut BTreeSet<u64>,
) -> Result<(), PendingWriteError> {
    let mut prev_index = None;
    for client_id in sequence {
        let Some(&index) = queue_positions.get(client_id) else {
            return Err(PendingWriteError::PendingReplyLost {
                client_id: *client_id,
            });
        };
        if !seen.insert(*client_id) {
            return Err(PendingWriteError::FlushOrderViolation {
                client_id: *client_id,
            });
        }
        if let Some(previous) = prev_index
            && index < previous
        {
            return Err(PendingWriteError::FlushOrderViolation {
                client_id: *client_id,
            });
        }
        prev_index = Some(index);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsAcceptPlan {
    pub accepted_tls: usize,
    pub deferred_tls: usize,
    pub accepted_non_tls: usize,
    pub total_accepted: usize,
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

#[must_use]
pub fn apply_tls_accept_rate_limit(
    total_accept_budget: usize,
    pending_tls_accepts: usize,
    pending_non_tls_accepts: usize,
    max_new_tls_connections_per_cycle: usize,
) -> TlsAcceptPlan {
    let tls_budget = total_accept_budget.min(max_new_tls_connections_per_cycle);
    let accepted_tls = pending_tls_accepts.min(tls_budget);
    let remaining_accept_budget = total_accept_budget.saturating_sub(accepted_tls);
    let accepted_non_tls = pending_non_tls_accepts.min(remaining_accept_budget);
    let total_accepted = accepted_tls.saturating_add(accepted_non_tls);
    let deferred_tls = pending_tls_accepts.saturating_sub(accepted_tls);

    TlsAcceptPlan {
        accepted_tls,
        deferred_tls,
        accepted_non_tls,
        total_accepted,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptPathError, BarrierOrderError, BootstrapError, EVENT_LOOP_PHASE_ORDER, EventLoopMode,
        EventLoopPhase, FdRegistrationError, LoopBootstrap, PendingWriteError, PhaseReplayError,
        ReadPathError, ReadinessCallback, TickBudget, apply_tls_accept_rate_limit,
        plan_fd_setsize_growth, plan_readiness_callback_order, plan_tick, replay_phase_trace,
        run_tick, validate_accept_path, validate_ae_barrier_order, validate_bootstrap,
        validate_fd_registration_bounds, validate_pending_write_delivery, validate_read_path,
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
    fn fr_p2c_001_u002_barrier_order_preserves_writable_before_readable() {
        let observed = plan_readiness_callback_order(true, true, true);
        assert_eq!(observed.first, Some(ReadinessCallback::Writable));
        assert_eq!(observed.second, Some(ReadinessCallback::Readable));
        validate_ae_barrier_order(true, true, true, observed).expect("barrier order valid");
    }

    #[test]
    fn fr_p2c_001_u002_barrier_order_violation_is_rejected() {
        let err = validate_ae_barrier_order(
            true,
            true,
            true,
            super::CallbackDispatchOrder {
                first: Some(ReadinessCallback::Readable),
                second: Some(ReadinessCallback::Writable),
            },
        )
        .expect_err("must reject readable-before-writable order under barrier");
        assert_eq!(err, BarrierOrderError::AeBarrierViolation);
        assert_eq!(err.reason_code(), "eventloop.ae_barrier_violation");
    }

    #[test]
    fn fr_p2c_001_u002_barrier_order_property_covers_readiness_combinations() {
        for readable_ready in [false, true] {
            for writable_ready in [false, true] {
                let observed = plan_readiness_callback_order(readable_ready, writable_ready, true);
                if readable_ready && writable_ready {
                    validate_ae_barrier_order(true, true, true, observed)
                        .expect("barrier validation succeeds for planned order");
                } else {
                    validate_ae_barrier_order(readable_ready, writable_ready, true, observed)
                        .expect("barrier validation is vacuously true without dual readiness");
                }
            }
        }
    }

    #[test]
    fn fr_p2c_001_u004_fd_registration_rejects_out_of_range_descriptor() {
        let err = validate_fd_registration_bounds(64, 64).expect_err("fd must be in range");
        assert_eq!(
            err,
            FdRegistrationError::FdOutOfRange {
                fd: 64,
                setsize: 64
            }
        );
        assert_eq!(err.reason_code(), "eventloop.fd_out_of_range");
    }

    #[test]
    fn fr_p2c_001_u004_fd_resize_growth_is_deterministic() {
        let grown = plan_fd_setsize_growth(64, 120, 1_024).expect("must grow");
        assert_eq!(grown, 128);
    }

    #[test]
    fn fr_p2c_001_u004_fd_resize_failure_reports_reason_code() {
        let err = plan_fd_setsize_growth(64, 2_048, 1_024).expect_err("must fail");
        assert_eq!(
            err,
            FdRegistrationError::FdResizeFailure {
                requested_fd: 2_048,
                max_setsize: 1_024
            }
        );
        assert_eq!(err.reason_code(), "eventloop.fd_resize_failure");
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
    fn fr_p2c_001_u006_accept_path_rejects_over_maxclients() {
        let err = validate_accept_path(10_000, 10_000, true).expect_err("must reject");
        assert_eq!(
            err,
            AcceptPathError::MaxClientsReached {
                current_clients: 10_000,
                max_clients: 10_000
            }
        );
        assert_eq!(err.reason_code(), "eventloop.accept.maxclients_reached");
    }

    #[test]
    fn fr_p2c_001_u006_accept_path_detects_handler_bind_failure() {
        let err = validate_accept_path(9_999, 10_000, false).expect_err("bind must fail");
        assert_eq!(err, AcceptPathError::HandlerBindFailure);
        assert_eq!(err.reason_code(), "eventloop.accept.handler_bind_failure");
    }

    #[test]
    fn fr_p2c_001_u007_read_path_enforces_query_buffer_limit() {
        let err = validate_read_path(6, 5, 10, false).expect_err("must exceed limit");
        assert_eq!(
            err,
            ReadPathError::QueryBufferLimitExceeded {
                observed: 11,
                limit: 10
            }
        );
        assert_eq!(err.reason_code(), "eventloop.read.querybuf_limit_exceeded");
    }

    #[test]
    fn fr_p2c_001_u008_read_path_terminates_on_fatal_error() {
        let err = validate_read_path(0, 0, 32, true).expect_err("fatal read must disconnect");
        assert_eq!(err, ReadPathError::FatalErrorDisconnect);
        assert_eq!(err.reason_code(), "eventloop.read.fatal_error_disconnect");
    }

    #[test]
    fn fr_p2c_001_u009_pending_write_delivery_accepts_prefix_flushes() {
        let queued = [11_u64, 13, 17, 19];
        for split in 0..=queued.len() {
            let flushed = &queued[..split];
            let pending = &queued[split..];
            validate_pending_write_delivery(&queued, flushed, pending)
                .expect("prefix flush must preserve delivery integrity");
        }
    }

    #[test]
    fn fr_p2c_001_u009_pending_write_delivery_rejects_reordered_flushes() {
        let queued = [11_u64, 13, 17];
        let err = validate_pending_write_delivery(&queued, &[13, 11], &[17])
            .expect_err("reordered flush must fail");
        assert_eq!(
            err,
            PendingWriteError::FlushOrderViolation { client_id: 11 }
        );
        assert_eq!(err.reason_code(), "eventloop.write.flush_order_violation");
    }

    #[test]
    fn fr_p2c_001_u009_pending_write_delivery_rejects_missing_replies() {
        let queued = [11_u64, 13, 17];
        let err = validate_pending_write_delivery(&queued, &[11], &[17])
            .expect_err("missing client reply must fail");
        assert_eq!(err, PendingWriteError::PendingReplyLost { client_id: 13 });
        assert_eq!(err.reason_code(), "eventloop.write.pending_reply_lost");
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

    #[test]
    fn fr_p2c_001_u010_bootstrap_accepts_required_hooks_and_timer() {
        validate_bootstrap(LoopBootstrap::fully_wired()).expect("fully wired");
    }

    #[test]
    fn fr_p2c_001_u010_bootstrap_rejects_missing_hooks() {
        let err = validate_bootstrap(LoopBootstrap {
            before_sleep_hook_installed: false,
            after_sleep_hook_installed: true,
            server_cron_timer_installed: true,
        })
        .expect_err("missing hook");
        assert_eq!(err, BootstrapError::BeforeSleepHookMissing);
        assert_eq!(err.reason_code(), "eventloop.hook_install_missing");
    }

    #[test]
    fn fr_p2c_001_u010_bootstrap_rejects_missing_server_cron_timer() {
        let err = validate_bootstrap(LoopBootstrap {
            before_sleep_hook_installed: true,
            after_sleep_hook_installed: true,
            server_cron_timer_installed: false,
        })
        .expect_err("missing timer");
        assert_eq!(err, BootstrapError::ServerCronTimerMissing);
        assert_eq!(err.reason_code(), "eventloop.server_cron_timer_missing");
    }

    #[test]
    fn fr_p2c_009_u011_tls_accept_limit_clamps_tls_accepts() {
        let plan = apply_tls_accept_rate_limit(10, 15, 5, 4);
        assert_eq!(plan.accepted_tls, 4);
        assert_eq!(plan.deferred_tls, 11);
        assert_eq!(plan.accepted_non_tls, 5);
        assert_eq!(plan.total_accepted, 9);
    }

    #[test]
    fn fr_p2c_009_u011_tls_accept_limit_preserves_non_tls_when_tls_is_zero() {
        let plan = apply_tls_accept_rate_limit(8, 6, 10, 0);
        assert_eq!(plan.accepted_tls, 0);
        assert_eq!(plan.deferred_tls, 6);
        assert_eq!(plan.accepted_non_tls, 8);
        assert_eq!(plan.total_accepted, 8);
    }

    #[test]
    fn fr_p2c_009_u011_tls_accept_limit_respects_global_accept_budget() {
        let plan = apply_tls_accept_rate_limit(3, 2, 8, 5);
        assert_eq!(plan.accepted_tls, 2);
        assert_eq!(plan.deferred_tls, 0);
        assert_eq!(plan.accepted_non_tls, 1);
        assert_eq!(plan.total_accepted, 3);
    }
}
