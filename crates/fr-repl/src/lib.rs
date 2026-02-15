#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplState {
    Handshake,
    FullSync,
    Online,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ReplOffset(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplProgress {
    pub state: ReplState,
    pub primary_offset: ReplOffset,
    pub replica_ack_offset: ReplOffset,
}

impl Default for ReplProgress {
    fn default() -> Self {
        Self {
            state: ReplState::Handshake,
            primary_offset: ReplOffset(0),
            replica_ack_offset: ReplOffset(0),
        }
    }
}

impl ReplProgress {
    pub fn on_full_sync_start(&mut self) {
        self.state = ReplState::FullSync;
    }

    pub fn on_online(&mut self) {
        self.state = ReplState::Online;
    }

    pub fn append_primary_bytes(&mut self, bytes: u64) {
        self.primary_offset.0 = self.primary_offset.0.saturating_add(bytes);
    }

    pub fn ack_replica_offset(&mut self, offset: ReplOffset) {
        if offset > self.replica_ack_offset {
            self.replica_ack_offset = offset;
        }
    }

    #[must_use]
    pub fn lag_bytes(&self) -> u64 {
        self.primary_offset
            .0
            .saturating_sub(self.replica_ack_offset.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeStep {
    Ping,
    Auth,
    Replconf,
    Psync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeState {
    Init,
    PingSeen,
    AuthSeen,
    ReplconfSeen,
    PsyncSent,
    Online,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplError {
    HandshakeStateMachineMismatch {
        state: HandshakeState,
        step: HandshakeStep,
    },
    PsyncReplyStateMismatch {
        state: HandshakeState,
    },
}

impl ReplError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::HandshakeStateMachineMismatch { .. } => "repl.handshake_state_machine_mismatch",
            Self::PsyncReplyStateMismatch { .. } => "repl.fullresync_reply_parse_violation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeFsm {
    state: HandshakeState,
    auth_required: bool,
}

impl HandshakeFsm {
    #[must_use]
    pub const fn new(auth_required: bool) -> Self {
        Self {
            state: HandshakeState::Init,
            auth_required,
        }
    }

    #[must_use]
    pub const fn state(self) -> HandshakeState {
        self.state
    }

    pub fn on_step(&mut self, step: HandshakeStep) -> Result<(), ReplError> {
        let transition = match (self.state, step, self.auth_required) {
            (HandshakeState::Init, HandshakeStep::Ping, _) => Some(HandshakeState::PingSeen),
            (HandshakeState::PingSeen, HandshakeStep::Auth, true) => Some(HandshakeState::AuthSeen),
            (HandshakeState::PingSeen, HandshakeStep::Replconf, false) => {
                Some(HandshakeState::ReplconfSeen)
            }
            (HandshakeState::AuthSeen, HandshakeStep::Replconf, _) => {
                Some(HandshakeState::ReplconfSeen)
            }
            (HandshakeState::ReplconfSeen, HandshakeStep::Replconf, _) => {
                Some(HandshakeState::ReplconfSeen)
            }
            (HandshakeState::ReplconfSeen, HandshakeStep::Psync, _) => {
                Some(HandshakeState::PsyncSent)
            }
            _ => None,
        };

        if let Some(state) = transition {
            self.state = state;
            Ok(())
        } else {
            Err(ReplError::HandshakeStateMachineMismatch {
                state: self.state,
                step,
            })
        }
    }

    pub fn on_psync_accepted(&mut self) -> Result<(), ReplError> {
        if self.state != HandshakeState::PsyncSent {
            return Err(ReplError::PsyncReplyStateMismatch { state: self.state });
        }
        self.state = HandshakeState::Online;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklogWindow {
    pub replid: String,
    pub start_offset: ReplOffset,
    pub end_offset: ReplOffset,
}

impl BacklogWindow {
    #[must_use]
    pub fn contains(&self, offset: ReplOffset) -> bool {
        self.start_offset <= offset && offset <= self.end_offset
    }

    pub fn rotate(&mut self, replid: String, start_offset: ReplOffset, end_offset: ReplOffset) {
        self.replid = replid;
        self.start_offset = start_offset;
        self.end_offset = end_offset;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsyncRejection {
    ReplidMismatch,
    OffsetOutOfRange,
}

impl PsyncRejection {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::ReplidMismatch => "repl.psync_replid_or_offset_reject_mismatch",
            Self::OffsetOutOfRange => "repl.psync_fullresync_fallback_mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsyncDecision {
    Continue { requested_offset: ReplOffset },
    FullResync { rejection: PsyncRejection },
}

#[must_use]
pub fn decide_psync(
    backlog: &BacklogWindow,
    requested_replid: &str,
    requested_offset: ReplOffset,
) -> PsyncDecision {
    if requested_replid != backlog.replid {
        return PsyncDecision::FullResync {
            rejection: PsyncRejection::ReplidMismatch,
        };
    }
    if !backlog.contains(requested_offset) {
        return PsyncDecision::FullResync {
            rejection: PsyncRejection::OffsetOutOfRange,
        };
    }
    PsyncDecision::Continue { requested_offset }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitThreshold {
    pub required_offset: ReplOffset,
    pub required_replicas: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitOutcome {
    pub acked_replicas: usize,
    pub satisfied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitAofThreshold {
    pub required_local_offset: ReplOffset,
    pub required_replica_offset: ReplOffset,
    pub required_replicas: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitAofOutcome {
    pub local_satisfied: bool,
    pub acked_replicas: usize,
    pub satisfied: bool,
}

#[must_use]
pub fn count_offsets_at_or_above(offsets: &[ReplOffset], required: ReplOffset) -> usize {
    offsets.iter().filter(|offset| **offset >= required).count()
}

#[must_use]
pub fn evaluate_wait(replica_ack_offsets: &[ReplOffset], threshold: WaitThreshold) -> WaitOutcome {
    let acked_replicas = count_offsets_at_or_above(replica_ack_offsets, threshold.required_offset);
    WaitOutcome {
        acked_replicas,
        satisfied: acked_replicas >= threshold.required_replicas,
    }
}

#[must_use]
pub fn evaluate_waitaof(
    local_fsync_offset: ReplOffset,
    replica_fsync_offsets: &[ReplOffset],
    threshold: WaitAofThreshold,
) -> WaitAofOutcome {
    let local_satisfied = local_fsync_offset >= threshold.required_local_offset;
    let acked_replicas =
        count_offsets_at_or_above(replica_fsync_offsets, threshold.required_replica_offset);
    WaitAofOutcome {
        local_satisfied,
        acked_replicas,
        satisfied: local_satisfied && acked_replicas >= threshold.required_replicas,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BacklogWindow, HandshakeFsm, HandshakeState, HandshakeStep, PsyncDecision, PsyncRejection,
        ReplError, ReplOffset, ReplProgress, ReplState, WaitAofThreshold, WaitThreshold,
        decide_psync, evaluate_wait, evaluate_waitaof,
    };

    #[test]
    fn lag_is_monotonic_with_unacked_primary_writes() {
        let mut repl = ReplProgress::default();
        repl.on_online();
        assert_eq!(repl.state, ReplState::Online);
        repl.append_primary_bytes(128);
        assert_eq!(repl.lag_bytes(), 128);
        repl.ack_replica_offset(ReplOffset(64));
        assert_eq!(repl.lag_bytes(), 64);
    }

    #[test]
    fn fr_p2c_006_u004_replica_ack_offsets_never_regress() {
        let mut repl = ReplProgress::default();
        repl.ack_replica_offset(ReplOffset(100));
        repl.ack_replica_offset(ReplOffset(90));
        assert_eq!(repl.replica_ack_offset, ReplOffset(100));
    }

    #[test]
    fn fr_p2c_006_u001_psync_accepts_partial_resync_inside_window() {
        let backlog = BacklogWindow {
            replid: "replid-a".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };
        let decision = decide_psync(&backlog, "replid-a", ReplOffset(150));
        assert_eq!(
            decision,
            PsyncDecision::Continue {
                requested_offset: ReplOffset(150)
            }
        );
    }

    #[test]
    fn fr_p2c_006_u002_psync_rejects_replid_mismatch() {
        let backlog = BacklogWindow {
            replid: "replid-a".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };
        let decision = decide_psync(&backlog, "replid-b", ReplOffset(150));
        assert_eq!(
            decision,
            PsyncDecision::FullResync {
                rejection: PsyncRejection::ReplidMismatch
            }
        );
        if let PsyncDecision::FullResync { rejection } = decision {
            assert_eq!(
                rejection.reason_code(),
                "repl.psync_replid_or_offset_reject_mismatch"
            );
        }
    }

    #[test]
    fn fr_p2c_006_u002_psync_rejects_offset_outside_window() {
        let backlog = BacklogWindow {
            replid: "replid-a".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };
        let decision = decide_psync(&backlog, "replid-a", ReplOffset(99));
        assert_eq!(
            decision,
            PsyncDecision::FullResync {
                rejection: PsyncRejection::OffsetOutOfRange
            }
        );
        if let PsyncDecision::FullResync { rejection } = decision {
            assert_eq!(
                rejection.reason_code(),
                "repl.psync_fullresync_fallback_mismatch"
            );
        }
    }

    #[test]
    fn fr_p2c_006_u003_handshake_requires_ping_first() {
        let mut fsm = HandshakeFsm::new(false);
        let err = fsm.on_step(HandshakeStep::Replconf).expect_err("must fail");
        assert_eq!(
            err,
            ReplError::HandshakeStateMachineMismatch {
                state: HandshakeState::Init,
                step: HandshakeStep::Replconf,
            }
        );
        assert_eq!(err.reason_code(), "repl.handshake_state_machine_mismatch");
    }

    #[test]
    fn fr_p2c_006_u003_handshake_requires_auth_when_configured() {
        let mut fsm = HandshakeFsm::new(true);
        fsm.on_step(HandshakeStep::Ping).expect("ping");
        let err = fsm
            .on_step(HandshakeStep::Replconf)
            .expect_err("auth required");
        assert_eq!(err.reason_code(), "repl.handshake_state_machine_mismatch");

        fsm.on_step(HandshakeStep::Auth).expect("auth");
        fsm.on_step(HandshakeStep::Replconf).expect("replconf");
        fsm.on_step(HandshakeStep::Psync).expect("psync");
        fsm.on_psync_accepted().expect("psync accepted");
        assert_eq!(fsm.state(), HandshakeState::Online);
    }

    #[test]
    fn fr_p2c_006_u003_handshake_allows_replconf_then_psync_without_auth() {
        let mut fsm = HandshakeFsm::new(false);
        fsm.on_step(HandshakeStep::Ping).expect("ping");
        fsm.on_step(HandshakeStep::Replconf).expect("replconf-1");
        fsm.on_step(HandshakeStep::Replconf).expect("replconf-2");
        fsm.on_step(HandshakeStep::Psync).expect("psync");
        fsm.on_psync_accepted().expect("accepted");
        assert_eq!(fsm.state(), HandshakeState::Online);
    }

    #[test]
    fn fr_p2c_006_u005_wait_threshold_counts_acknowledged_replicas() {
        let outcome = evaluate_wait(
            &[ReplOffset(110), ReplOffset(90), ReplOffset(130)],
            WaitThreshold {
                required_offset: ReplOffset(100),
                required_replicas: 2,
            },
        );
        assert_eq!(outcome.acked_replicas, 2);
        assert!(outcome.satisfied);
    }

    #[test]
    fn fr_p2c_006_u006_waitaof_requires_local_and_replica_thresholds() {
        let threshold = WaitAofThreshold {
            required_local_offset: ReplOffset(100),
            required_replica_offset: ReplOffset(95),
            required_replicas: 2,
        };

        let not_local = evaluate_waitaof(
            ReplOffset(99),
            &[ReplOffset(100), ReplOffset(98)],
            threshold,
        );
        assert!(!not_local.local_satisfied);
        assert!(!not_local.satisfied);

        let not_replicas = evaluate_waitaof(
            ReplOffset(100),
            &[ReplOffset(96), ReplOffset(94)],
            threshold,
        );
        assert!(not_replicas.local_satisfied);
        assert_eq!(not_replicas.acked_replicas, 1);
        assert!(!not_replicas.satisfied);

        let satisfied = evaluate_waitaof(
            ReplOffset(101),
            &[ReplOffset(97), ReplOffset(98), ReplOffset(50)],
            threshold,
        );
        assert!(satisfied.local_satisfied);
        assert_eq!(satisfied.acked_replicas, 2);
        assert!(satisfied.satisfied);
    }
}
