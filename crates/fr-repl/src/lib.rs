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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsyncReply {
    Continue,
    FullResync { replid: String, offset: ReplOffset },
}

const REDIS_RUN_ID_BYTES: usize = 40;

pub fn parse_psync_reply(line: &str) -> Result<PsyncReply, ReplError> {
    let mut parts = line.split_ascii_whitespace();
    let Some(kind) = parts.next() else {
        return Err(ReplError::PsyncReplyStateMismatch {
            state: HandshakeState::PsyncSent,
        });
    };
    if kind == "CONTINUE" {
        return match (parts.next(), parts.next()) {
            (None, None) | (Some(_), None) => Ok(PsyncReply::Continue),
            _ => Err(ReplError::PsyncReplyStateMismatch {
                state: HandshakeState::PsyncSent,
            }),
        };
    }
    if kind != "FULLRESYNC" {
        return Err(ReplError::PsyncReplyStateMismatch {
            state: HandshakeState::PsyncSent,
        });
    }
    let (Some(replid), Some(offset_text), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ReplError::PsyncReplyStateMismatch {
            state: HandshakeState::PsyncSent,
        });
    };
    if replid.len() != REDIS_RUN_ID_BYTES {
        return Err(ReplError::PsyncReplyStateMismatch {
            state: HandshakeState::PsyncSent,
        });
    }
    let offset = offset_text
        .parse::<u64>()
        .map_err(|_| ReplError::PsyncReplyStateMismatch {
            state: HandshakeState::PsyncSent,
        })?;
    Ok(PsyncReply::FullResync {
        replid: replid.to_string(),
        offset: ReplOffset(offset),
    })
}

#[must_use]
pub fn decide_psync(
    backlog: &BacklogWindow,
    requested_replid: &str,
    requested_offset: ReplOffset,
) -> PsyncDecision {
    if !requested_replid.eq_ignore_ascii_case(&backlog.replid) {
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
        PsyncReply, ReplError, ReplOffset, ReplProgress, ReplState, WaitAofThreshold,
        WaitThreshold, decide_psync, evaluate_wait, evaluate_waitaof, parse_psync_reply,
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
        // Test lower bound (inclusive)
        assert_eq!(
            decide_psync(&backlog, "replid-a", ReplOffset(100)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(100)
            }
        );
        // Test upper bound (inclusive)
        assert_eq!(
            decide_psync(&backlog, "replid-a", ReplOffset(200)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(200)
            }
        );
    }

    #[test]
    fn psync_replid_match_is_ascii_case_insensitive_like_redis() {
        let backlog = BacklogWindow {
            replid: "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };
        assert_eq!(
            decide_psync(
                &backlog,
                "ABCDEF1234567890ABCDEF1234567890ABCDEF12",
                ReplOffset(150),
            ),
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
    fn fr_p2c_006_u005_psync_accepts_offset_at_backlog_end() {
        let backlog = BacklogWindow {
            replid: "replid-a".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };
        // End offset is inclusive — a replica at the exact end should continue.
        let decision = decide_psync(&backlog, "replid-a", ReplOffset(200));
        assert_eq!(
            decision,
            PsyncDecision::Continue {
                requested_offset: ReplOffset(200)
            }
        );
        // But one past the end should be rejected.
        let decision_past = decide_psync(&backlog, "replid-a", ReplOffset(201));
        assert_eq!(
            decision_past,
            PsyncDecision::FullResync {
                rejection: PsyncRejection::OffsetOutOfRange
            }
        );
    }

    #[test]
    fn parse_psync_reply_accepts_continue_and_fullresync() {
        let replid = "1111111111111111111111111111111111111111";
        assert_eq!(parse_psync_reply("CONTINUE"), Ok(PsyncReply::Continue));
        assert_eq!(
            parse_psync_reply(&format!("CONTINUE {replid}")),
            Ok(PsyncReply::Continue)
        );
        assert_eq!(
            parse_psync_reply(&format!("FULLRESYNC {replid} 42")),
            Ok(PsyncReply::FullResync {
                replid: replid.to_string(),
                offset: ReplOffset(42),
            })
        );
        assert_eq!(
            parse_psync_reply(&format!("\tFULLRESYNC \t  {replid}\t\t100\r\n")),
            Ok(PsyncReply::FullResync {
                replid: replid.to_string(),
                offset: ReplOffset(100),
            })
        );
        assert_eq!(
            parse_psync_reply("FULLRESYNC repl-id_with.dashes123456789012345678901 7"),
            Ok(PsyncReply::FullResync {
                replid: "repl-id_with.dashes123456789012345678901".to_string(),
                offset: ReplOffset(7),
            })
        );
    }

    /// Lock the contract for the structured corpus seeds in
    /// `fuzz/corpus/fuzz_psync_reply/`. The fuzz harness dispatches
    /// the first byte two ways (`% 2`):
    ///
    ///   0 → fuzz_raw_psync_reply (body fed to parse_psync_reply)
    ///   1 → fuzz_structured_psync_reply (arbitrary)
    ///
    /// The seed generator (`fuzz/scripts/gen_psync_reply_seeds.py`)
    /// writes mode-0 seeds covering each shape boundary
    /// `parse_psync_reply` cares about. This test asserts:
    ///
    ///   1. Every "accept" seed produces the expected `PsyncReply`
    ///      enum value, AND the round-trip-via-canonical-form
    ///      property the fuzz target asserts also holds.
    ///   2. Every "reject" seed fails with
    ///      `ReplError::PsyncReplyStateMismatch` so libfuzzer
    ///      doesn't get stuck on phantom successes if the parser
    ///      ever drifts.
    #[test]
    fn fuzz_psync_reply_corpus_matches_documented_contract() -> Result<(), String> {
        use std::path::Path;

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_psync_reply");
        if !corpus_root.exists() {
            // Corpus is committed; skip if a packaged checkout
            // strips the fuzz tree.
            return Ok(());
        }

        // Each seed file is `<mode-byte><body>`. Strip the leading
        // mode byte (always 0x00 for these seeds — selects the raw
        // path) before feeding to parse_psync_reply.
        fn read_body(corpus_root: &Path, name: &str) -> Result<String, String> {
            let bytes = std::fs::read(corpus_root.join(name))
                .map_err(|err| format!("read seed {name}: {err}"))?;
            if bytes.is_empty() {
                return Err(format!("seed {name} is empty"));
            }
            if bytes[0] != 0x00 {
                return Err(format!(
                    "seed {name} mode byte must be 0x00 (raw_psync_reply path)"
                ));
            }
            Ok(String::from_utf8_lossy(&bytes[1..]).into_owned())
        }

        // ── Accept-class seeds: must produce the listed reply.
        // Each pair is (seed_name, expected_reply).
        let accepts: &[(&str, PsyncReply)] = &[
            ("continue_canonical.txt", PsyncReply::Continue),
            ("continue_with_psync2_replid.txt", PsyncReply::Continue),
            ("continue_leading_tab.txt", PsyncReply::Continue),
            ("continue_trailing_crlf.txt", PsyncReply::Continue),
            (
                "fullresync_canonical_offset_zero.txt",
                PsyncReply::FullResync {
                    replid: "1234567890abcdef1234567890abcdef12345678".to_string(),
                    offset: ReplOffset(0),
                },
            ),
            (
                "fullresync_canonical_offset_typical.txt",
                PsyncReply::FullResync {
                    replid: "1234567890abcdef1234567890abcdef12345678".to_string(),
                    offset: ReplOffset(4096),
                },
            ),
            (
                "fullresync_canonical_offset_max_u64.txt",
                PsyncReply::FullResync {
                    replid: "1234567890abcdef1234567890abcdef12345678".to_string(),
                    offset: ReplOffset(u64::MAX),
                },
            ),
        ];

        // Mirror the harness' canonical-round-trip property: any
        // accepted reply must parse back from its canonical form.
        fn canonical(reply: &PsyncReply) -> String {
            match reply {
                PsyncReply::Continue => "CONTINUE".to_string(),
                PsyncReply::FullResync { replid, offset } => {
                    format!("FULLRESYNC {replid} {}", offset.0)
                }
            }
        }

        for (name, expected) in accepts {
            let body = read_body(&corpus_root, name)?;
            let parsed = parse_psync_reply(&body).map_err(|err| format!("seed {name}: {err:?}"))?;
            assert_eq!(&parsed, expected, "seed {name} parse mismatch");
            // Canonical round-trip property the fuzz target asserts.
            assert_eq!(
                parse_psync_reply(&canonical(&parsed)).as_ref(),
                Ok(expected),
                "seed {name} canonical round-trip broke",
            );
        }

        // ── Reject-class seeds: must produce
        // ReplError::PsyncReplyStateMismatch.
        let rejects: &[&str] = &[
            "empty.txt",
            "whitespace_only_space.txt",
            "whitespace_only_tab.txt",
            "whitespace_only_crlf.txt",
            "continue_lowercase.txt",
            "continue_mixedcase.txt",
            "fullresync_lowercase.txt",
            "fullresync_multi_whitespace_separators.txt",
            "fullresync_with_newlines_between_tokens.txt",
            "fullresync_one_char_replid.txt",
            "fullresync_long_replid.txt",
            "fullresync_numeric_replid.txt",
            "fullresync_replid_with_punctuation.txt",
            "fullresync_missing_replid_and_offset.txt",
            "fullresync_missing_offset.txt",
            "fullresync_offset_negative.txt",
            "fullresync_offset_overflow_u64.txt",
            "fullresync_offset_nonnumeric.txt",
            "fullresync_extra_trailing_token.txt",
            "fullresync_extra_double_trailing_tokens.txt",
            "continue_with_extra_token.txt",
            "continue_with_three_extras.txt",
            "first_token_unknown.txt",
            "first_token_empty_after_resp_prefix.txt",
        ];
        for name in rejects {
            let body = read_body(&corpus_root, name)?;
            let err = match parse_psync_reply(&body) {
                Ok(reply) => return Err(format!("seed {name} must reject: got Ok({reply:?})")),
                Err(err) => err,
            };
            assert!(
                matches!(err, ReplError::PsyncReplyStateMismatch { .. }),
                "seed {name} surfaced unexpected error variant: {err:?}",
            );
        }
        Ok(())
    }

    #[test]
    fn parse_psync_reply_rejects_invalid_shape() {
        assert!(parse_psync_reply("CONTINUE replid extra").is_err());
        assert!(parse_psync_reply("FULLRESYNC only-two").is_err());
        assert!(parse_psync_reply("FULLRESYNC short 42").is_err());
        assert!(parse_psync_reply("FULLRESYNC replid nope").is_err());
        assert!(parse_psync_reply("FULLRESYNC replid 42 extra").is_err());
        assert!(parse_psync_reply("ERR nope").is_err());
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

    #[test]
    fn backlog_window_zero_width_accepts_exact_offset() {
        let backlog = BacklogWindow {
            replid: "r".to_string(),
            start_offset: ReplOffset(50),
            end_offset: ReplOffset(50),
        };
        // Exact match on zero-width window should succeed.
        assert_eq!(
            decide_psync(&backlog, "r", ReplOffset(50)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(50)
            }
        );
        // One before and one after should fail.
        assert!(matches!(
            decide_psync(&backlog, "r", ReplOffset(49)),
            PsyncDecision::FullResync { .. }
        ));
        assert!(matches!(
            decide_psync(&backlog, "r", ReplOffset(51)),
            PsyncDecision::FullResync { .. }
        ));
    }

    #[test]
    fn backlog_window_at_offset_zero() {
        let backlog = BacklogWindow {
            replid: "r".to_string(),
            start_offset: ReplOffset(0),
            end_offset: ReplOffset(10),
        };
        assert_eq!(
            decide_psync(&backlog, "r", ReplOffset(0)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(0)
            }
        );
        assert_eq!(
            decide_psync(&backlog, "r", ReplOffset(10)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(10)
            }
        );
    }

    #[test]
    fn backlog_window_rotate_replaces_all_fields() {
        let mut backlog = BacklogWindow {
            replid: "old".to_string(),
            start_offset: ReplOffset(0),
            end_offset: ReplOffset(100),
        };
        backlog.rotate("new".to_string(), ReplOffset(200), ReplOffset(300));
        assert_eq!(backlog.replid, "new");
        assert_eq!(backlog.start_offset, ReplOffset(200));
        assert_eq!(backlog.end_offset, ReplOffset(300));
        // Old offsets should no longer be accepted.
        assert!(matches!(
            decide_psync(&backlog, "new", ReplOffset(100)),
            PsyncDecision::FullResync { .. }
        ));
        // New offsets should work.
        assert_eq!(
            decide_psync(&backlog, "new", ReplOffset(250)),
            PsyncDecision::Continue {
                requested_offset: ReplOffset(250)
            }
        );
    }

    #[test]
    fn wait_with_no_replicas_never_satisfied() {
        let outcome = evaluate_wait(
            &[],
            WaitThreshold {
                required_offset: ReplOffset(1),
                required_replicas: 1,
            },
        );
        assert_eq!(outcome.acked_replicas, 0);
        assert!(!outcome.satisfied);
    }

    #[test]
    fn wait_with_zero_required_replicas_always_satisfied() {
        let outcome = evaluate_wait(
            &[],
            WaitThreshold {
                required_offset: ReplOffset(100),
                required_replicas: 0,
            },
        );
        assert_eq!(outcome.acked_replicas, 0);
        assert!(outcome.satisfied);
    }

    #[test]
    fn repl_progress_saturating_add_handles_overflow() {
        let mut repl = ReplProgress::default();
        repl.append_primary_bytes(u64::MAX);
        assert_eq!(repl.primary_offset, ReplOffset(u64::MAX));
        // Should not overflow.
        repl.append_primary_bytes(1);
        assert_eq!(repl.primary_offset, ReplOffset(u64::MAX));
    }

    #[test]
    fn psync_with_question_mark_replid_triggers_fullresync() {
        let backlog = BacklogWindow {
            replid: "abc123".to_string(),
            start_offset: ReplOffset(0),
            end_offset: ReplOffset(100),
        };
        // "?" is the initial PSYNC replid for first sync.
        assert!(matches!(
            decide_psync(&backlog, "?", ReplOffset(0)),
            PsyncDecision::FullResync {
                rejection: PsyncRejection::ReplidMismatch
            }
        ));
    }
}
