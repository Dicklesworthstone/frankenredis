#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_eventloop::{
    AcceptPathError, BarrierOrderError, BootstrapError, CallbackDispatchOrder, EventLoopMode,
    EventLoopPhase, FdRegistrationError, LoopBootstrap, PendingWriteError, PhaseReplayError,
    ReadPathError, ReadinessCallback, TickBudget, apply_tls_accept_rate_limit,
    plan_fd_setsize_growth, plan_readiness_callback_order, plan_tick, replay_phase_trace,
    run_tick, validate_accept_path, validate_ae_barrier_order, validate_bootstrap,
    validate_fd_registration_bounds, validate_pending_write_delivery, validate_read_path,
};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};

const MAX_INPUT_LEN: usize = 4_096;
const MAX_TRACE_LEN: usize = 64;
const MAX_QUEUE_LEN: usize = 32;

#[derive(Debug, Arbitrary)]
enum EventLoopCase {
    Bootstrap(StructuredBootstrapCase),
    Barrier(StructuredBarrierCase),
    PhaseTrace(StructuredPhaseTraceCase),
    Tick(StructuredTickCase),
    FdGrowth(StructuredFdGrowthCase),
    AcceptPath(StructuredAcceptPathCase),
    ReadPath(StructuredReadPathCase),
    PendingWrite(StructuredPendingWriteCase),
    TlsAccept(StructuredTlsAcceptCase),
}

#[derive(Debug, Arbitrary)]
struct StructuredBootstrapCase {
    before_sleep_hook_installed: bool,
    after_sleep_hook_installed: bool,
    server_cron_timer_installed: bool,
}

#[derive(Debug, Arbitrary)]
struct StructuredBarrierCase {
    readable_ready: bool,
    writable_ready: bool,
    ae_barrier: bool,
    observed: StructuredCallbackDispatchOrder,
}

#[derive(Debug, Arbitrary)]
enum StructuredCallbackDispatchOrder {
    None,
    ReadableOnly,
    WritableOnly,
    ReadableThenWritable,
    WritableThenReadable,
}

#[derive(Debug, Arbitrary)]
struct StructuredPhaseTraceCase {
    trace: Vec<StructuredPhase>,
}

#[derive(Debug, Arbitrary)]
enum StructuredPhase {
    BeforeSleep,
    Poll,
    FileDispatch,
    TimeDispatch,
    AfterSleep,
}

#[derive(Debug, Arbitrary)]
struct StructuredTickCase {
    pending_accepts: u16,
    pending_commands: u16,
    max_accepts: u8,
    max_commands: u16,
    blocked_mode: bool,
}

#[derive(Debug, Arbitrary)]
struct StructuredFdGrowthCase {
    current_setsize: u16,
    requested_fd: u16,
    max_setsize: u16,
}

#[derive(Debug, Arbitrary)]
struct StructuredAcceptPathCase {
    current_clients: u16,
    max_clients: u16,
    read_handler_bound: bool,
}

#[derive(Debug, Arbitrary)]
struct StructuredReadPathCase {
    current_query_buffer_len: u16,
    newly_read_bytes: u16,
    query_buffer_limit: u16,
    fatal_read_error: bool,
}

#[derive(Debug, Arbitrary)]
struct StructuredPendingWriteCase {
    queued_before_flush: Vec<u8>,
    flushed_now: Vec<u8>,
    pending_after_flush: Vec<u8>,
}

#[derive(Debug, Arbitrary)]
struct StructuredTlsAcceptCase {
    total_accept_budget: u8,
    pending_tls_accepts: u8,
    pending_non_tls_accepts: u8,
    max_new_tls_connections_per_cycle: u8,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    fuzz_raw_phase_trace(data);

    let mut unstructured = Unstructured::new(data);
    let Ok(case) = EventLoopCase::arbitrary(&mut unstructured) else {
        return;
    };
    fuzz_structured_case(case);
});

fn fuzz_raw_phase_trace(data: &[u8]) {
    let trace: Vec<EventLoopPhase> = data
        .iter()
        .take(MAX_TRACE_LEN)
        .map(|byte| phase_from_byte(*byte))
        .collect();
    assert_eq!(
        replay_phase_trace(&trace),
        model_replay_phase_trace(&trace),
        "event-loop phase replay drifted for raw trace {:?}",
        trace
    );
}

fn fuzz_structured_case(case: EventLoopCase) {
    match case {
        EventLoopCase::Bootstrap(case) => fuzz_bootstrap_case(case),
        EventLoopCase::Barrier(case) => fuzz_barrier_case(case),
        EventLoopCase::PhaseTrace(case) => fuzz_phase_trace_case(case),
        EventLoopCase::Tick(case) => fuzz_tick_case(case),
        EventLoopCase::FdGrowth(case) => fuzz_fd_growth_case(case),
        EventLoopCase::AcceptPath(case) => fuzz_accept_path_case(case),
        EventLoopCase::ReadPath(case) => fuzz_read_path_case(case),
        EventLoopCase::PendingWrite(case) => fuzz_pending_write_case(case),
        EventLoopCase::TlsAccept(case) => fuzz_tls_accept_case(case),
    }
}

fn fuzz_bootstrap_case(case: StructuredBootstrapCase) {
    let bootstrap = LoopBootstrap {
        before_sleep_hook_installed: case.before_sleep_hook_installed,
        after_sleep_hook_installed: case.after_sleep_hook_installed,
        server_cron_timer_installed: case.server_cron_timer_installed,
    };
    assert_eq!(
        validate_bootstrap(bootstrap),
        model_validate_bootstrap(bootstrap),
        "event-loop bootstrap validation drifted for {:?}",
        bootstrap
    );
}

fn fuzz_barrier_case(case: StructuredBarrierCase) {
    let observed = callback_dispatch_order(case.observed);
    let actual = validate_ae_barrier_order(
        case.readable_ready,
        case.writable_ready,
        case.ae_barrier,
        observed,
    );
    let expected = model_validate_ae_barrier_order(
        case.readable_ready,
        case.writable_ready,
        case.ae_barrier,
        observed,
    );
    assert_eq!(
        actual, expected,
        "AE_BARRIER validation drifted for inputs {:?}",
        (
            case.readable_ready,
            case.writable_ready,
            case.ae_barrier,
            observed
        )
    );

    let planned =
        plan_readiness_callback_order(case.readable_ready, case.writable_ready, case.ae_barrier);
    if case.readable_ready && case.writable_ready && case.ae_barrier {
        assert_eq!(
            planned,
            CallbackDispatchOrder {
                first: Some(ReadinessCallback::Writable),
                second: Some(ReadinessCallback::Readable),
            },
            "AE_BARRIER plan must put writable before readable"
        );
    }
}

fn fuzz_phase_trace_case(case: StructuredPhaseTraceCase) {
    let trace: Vec<EventLoopPhase> = case
        .trace
        .into_iter()
        .take(MAX_TRACE_LEN)
        .map(phase_from_structured)
        .collect();
    assert_eq!(
        replay_phase_trace(&trace),
        model_replay_phase_trace(&trace),
        "structured phase replay drifted for trace {:?}",
        trace
    );
}

fn fuzz_tick_case(case: StructuredTickCase) {
    let budget = TickBudget {
        max_accepts: usize::from(case.max_accepts),
        max_commands: usize::from(case.max_commands),
    };
    let mode = if case.blocked_mode {
        EventLoopMode::Blocked
    } else {
        EventLoopMode::Normal
    };
    let actual = plan_tick(
        usize::from(case.pending_accepts),
        usize::from(case.pending_commands),
        budget,
        mode,
    );
    let effective_budget = match mode {
        EventLoopMode::Normal => budget,
        EventLoopMode::Blocked => budget.bounded_for_blocked_mode(),
    };
    let expected_stats = run_tick(
        usize::from(case.pending_accepts),
        usize::from(case.pending_commands),
        effective_budget,
    );

    assert_eq!(
        actual.stats, expected_stats,
        "plan_tick stats drifted from run_tick"
    );
    match mode {
        EventLoopMode::Blocked => {
            assert_eq!(actual.poll_timeout_ms, 0);
            assert!(actual.stats.accepted <= TickBudget::BLOCKED_MODE_MAX_ACCEPTS);
            assert!(actual.stats.processed_commands <= TickBudget::BLOCKED_MODE_MAX_COMMANDS);
        }
        EventLoopMode::Normal => {
            let expected_timeout = if case.pending_accepts > 0 || case.pending_commands > 0 {
                0
            } else {
                10
            };
            assert_eq!(actual.poll_timeout_ms, expected_timeout);
        }
    }
    assert!(actual.stats.accepted <= usize::from(case.pending_accepts));
    assert!(actual.stats.processed_commands <= usize::from(case.pending_commands));
}

fn fuzz_fd_growth_case(case: StructuredFdGrowthCase) {
    let current_setsize = usize::from(case.current_setsize);
    let requested_fd = usize::from(case.requested_fd);
    let max_setsize = usize::from(case.max_setsize);
    let actual = plan_fd_setsize_growth(current_setsize, requested_fd, max_setsize);
    let expected = model_plan_fd_setsize_growth(current_setsize, requested_fd, max_setsize);
    assert_eq!(
        actual, expected,
        "fd setsize growth drifted for current={current_setsize} requested={requested_fd} max={max_setsize}"
    );
    if let Ok(next_setsize) = actual {
        assert_eq!(
            validate_fd_registration_bounds(requested_fd, next_setsize),
            Ok(()),
            "successful setsize growth must allow registering the requested fd"
        );
    }
}

fn fuzz_accept_path_case(case: StructuredAcceptPathCase) {
    let current_clients = usize::from(case.current_clients);
    let max_clients = usize::from(case.max_clients);
    let actual = validate_accept_path(current_clients, max_clients, case.read_handler_bound);
    let expected = model_validate_accept_path(current_clients, max_clients, case.read_handler_bound);
    assert_eq!(
        actual, expected,
        "accept-path validation drifted for current={current_clients} max={max_clients} bound={}",
        case.read_handler_bound
    );
}

fn fuzz_read_path_case(case: StructuredReadPathCase) {
    let current_query_buffer_len = usize::from(case.current_query_buffer_len);
    let newly_read_bytes = usize::from(case.newly_read_bytes);
    let query_buffer_limit = usize::from(case.query_buffer_limit);
    let actual = validate_read_path(
        current_query_buffer_len,
        newly_read_bytes,
        query_buffer_limit,
        case.fatal_read_error,
    );
    let expected = model_validate_read_path(
        current_query_buffer_len,
        newly_read_bytes,
        query_buffer_limit,
        case.fatal_read_error,
    );
    assert_eq!(
        actual, expected,
        "read-path validation drifted for current={current_query_buffer_len} read={newly_read_bytes} limit={query_buffer_limit} fatal={}",
        case.fatal_read_error
    );
}

fn fuzz_pending_write_case(case: StructuredPendingWriteCase) {
    let queued_before_flush: Vec<u64> = case
        .queued_before_flush
        .into_iter()
        .take(MAX_QUEUE_LEN)
        .map(u64::from)
        .collect();
    let flushed_now: Vec<u64> = case
        .flushed_now
        .into_iter()
        .take(MAX_QUEUE_LEN)
        .map(u64::from)
        .collect();
    let pending_after_flush: Vec<u64> = case
        .pending_after_flush
        .into_iter()
        .take(MAX_QUEUE_LEN)
        .map(u64::from)
        .collect();

    let actual =
        validate_pending_write_delivery(&queued_before_flush, &flushed_now, &pending_after_flush);
    let expected = model_validate_pending_write_delivery(
        &queued_before_flush,
        &flushed_now,
        &pending_after_flush,
    );
    assert_eq!(
        actual, expected,
        "pending-write validation drifted for queue={queued_before_flush:?} flushed={flushed_now:?} pending={pending_after_flush:?}"
    );
}

fn fuzz_tls_accept_case(case: StructuredTlsAcceptCase) {
    let plan = apply_tls_accept_rate_limit(
        usize::from(case.total_accept_budget),
        usize::from(case.pending_tls_accepts),
        usize::from(case.pending_non_tls_accepts),
        usize::from(case.max_new_tls_connections_per_cycle),
    );

    assert!(
        plan.accepted_tls <= usize::from(case.pending_tls_accepts),
        "accepted TLS connections must not exceed pending TLS accepts"
    );
    assert!(
        plan.accepted_tls <= usize::from(case.max_new_tls_connections_per_cycle),
        "accepted TLS connections must respect the per-cycle TLS cap"
    );
    assert_eq!(
        plan.deferred_tls + plan.accepted_tls,
        usize::from(case.pending_tls_accepts),
        "TLS accept planning must conserve pending TLS accepts"
    );
    assert!(
        plan.accepted_non_tls <= usize::from(case.pending_non_tls_accepts),
        "accepted non-TLS connections must not exceed pending non-TLS accepts"
    );
    assert_eq!(
        plan.total_accepted,
        plan.accepted_tls + plan.accepted_non_tls,
        "total accepted connections must equal TLS + non-TLS accepts"
    );
    assert!(
        plan.total_accepted <= usize::from(case.total_accept_budget),
        "accept planning must not exceed the total accept budget"
    );
}

fn callback_dispatch_order(observed: StructuredCallbackDispatchOrder) -> CallbackDispatchOrder {
    match observed {
        StructuredCallbackDispatchOrder::None => CallbackDispatchOrder {
            first: None,
            second: None,
        },
        StructuredCallbackDispatchOrder::ReadableOnly => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Readable),
            second: None,
        },
        StructuredCallbackDispatchOrder::WritableOnly => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Writable),
            second: None,
        },
        StructuredCallbackDispatchOrder::ReadableThenWritable => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Readable),
            second: Some(ReadinessCallback::Writable),
        },
        StructuredCallbackDispatchOrder::WritableThenReadable => CallbackDispatchOrder {
            first: Some(ReadinessCallback::Writable),
            second: Some(ReadinessCallback::Readable),
        },
    }
}

fn phase_from_byte(byte: u8) -> EventLoopPhase {
    match byte % 5 {
        0 => EventLoopPhase::BeforeSleep,
        1 => EventLoopPhase::Poll,
        2 => EventLoopPhase::FileDispatch,
        3 => EventLoopPhase::TimeDispatch,
        _ => EventLoopPhase::AfterSleep,
    }
}

fn phase_from_structured(phase: StructuredPhase) -> EventLoopPhase {
    match phase {
        StructuredPhase::BeforeSleep => EventLoopPhase::BeforeSleep,
        StructuredPhase::Poll => EventLoopPhase::Poll,
        StructuredPhase::FileDispatch => EventLoopPhase::FileDispatch,
        StructuredPhase::TimeDispatch => EventLoopPhase::TimeDispatch,
        StructuredPhase::AfterSleep => EventLoopPhase::AfterSleep,
    }
}

fn model_validate_bootstrap(bootstrap: LoopBootstrap) -> Result<(), BootstrapError> {
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

fn model_validate_ae_barrier_order(
    readable_ready: bool,
    writable_ready: bool,
    ae_barrier: bool,
    observed: CallbackDispatchOrder,
) -> Result<(), BarrierOrderError> {
    if readable_ready
        && writable_ready
        && ae_barrier
        && observed
            != (CallbackDispatchOrder {
                first: Some(ReadinessCallback::Writable),
                second: Some(ReadinessCallback::Readable),
            })
    {
        return Err(BarrierOrderError::AeBarrierViolation);
    }
    Ok(())
}

fn model_replay_phase_trace(trace: &[EventLoopPhase]) -> Result<usize, PhaseReplayError> {
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

fn next_phase(phase: EventLoopPhase) -> EventLoopPhase {
    match phase {
        EventLoopPhase::BeforeSleep => EventLoopPhase::Poll,
        EventLoopPhase::Poll => EventLoopPhase::FileDispatch,
        EventLoopPhase::FileDispatch => EventLoopPhase::TimeDispatch,
        EventLoopPhase::TimeDispatch => EventLoopPhase::AfterSleep,
        EventLoopPhase::AfterSleep => EventLoopPhase::BeforeSleep,
    }
}

fn model_plan_fd_setsize_growth(
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

fn model_validate_accept_path(
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

fn model_validate_read_path(
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

fn model_validate_pending_write_delivery(
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
    let mut prev_index = None;
    model_validate_delivery_slice(
        flushed_now,
        &queue_positions,
        &mut seen,
        &mut prev_index,
    )?;
    model_validate_delivery_slice(
        pending_after_flush,
        &queue_positions,
        &mut seen,
        &mut prev_index,
    )?;

    for client_id in queued_before_flush {
        if !seen.contains(client_id) {
            return Err(PendingWriteError::PendingReplyLost {
                client_id: *client_id,
            });
        }
    }

    Ok(())
}

fn model_validate_delivery_slice(
    sequence: &[u64],
    queue_positions: &BTreeMap<u64, usize>,
    seen: &mut BTreeSet<u64>,
    prev_index: &mut Option<usize>,
) -> Result<(), PendingWriteError> {
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
        if let Some(previous) = *prev_index
            && index < previous
        {
            return Err(PendingWriteError::FlushOrderViolation {
                client_id: *client_id,
            });
        }
        *prev_index = Some(index);
    }
    Ok(())
}
