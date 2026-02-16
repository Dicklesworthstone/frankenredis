#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use fr_command::{CommandError, dispatch_argv, frame_to_argv};
use fr_config::{
    DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
    TlsCfgError, TlsConfig, TlsListenerTransition, TlsRuntimeState,
    evaluate_tls_hardened_deviation, plan_tls_runtime_apply,
};
use fr_eventloop::{
    BootstrapError, EventLoopMode, EventLoopPhase, LoopBootstrap, PhaseReplayError, TickBudget,
    TickPlan, apply_tls_accept_rate_limit, plan_tick, replay_phase_trace, validate_bootstrap,
};
use fr_protocol::{RespFrame, RespParseError, parse_frame};
use fr_store::Store;

static PACKET_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceEvent {
    pub ts_utc: String,
    pub ts_ms: u64,
    pub packet_id: u64,
    pub mode: Mode,
    pub severity: DriftSeverity,
    pub threat_class: ThreatClass,
    pub decision_action: DecisionAction,
    pub subsystem: &'static str,
    pub action: &'static str,
    pub reason_code: &'static str,
    pub reason: String,
    pub input_digest: String,
    pub output_digest: String,
    pub state_digest_before: String,
    pub state_digest_after: String,
    pub replay_cmd: String,
    pub artifact_refs: Vec<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Default)]
pub struct EvidenceLedger {
    events: Vec<EvidenceEvent>,
}

impl EvidenceLedger {
    pub fn record(&mut self, event: EvidenceEvent) {
        self.events.push(event);
    }

    #[must_use]
    pub fn events(&self) -> &[EvidenceEvent] {
        &self.events
    }
}

#[derive(Debug)]
pub struct Runtime {
    policy: RuntimePolicy,
    store: Store,
    evidence: EvidenceLedger,
    tls_state: TlsRuntimeState,
}

struct ThreatEventInput<'a> {
    now_ms: u64,
    packet_id: u64,
    threat_class: ThreatClass,
    preferred_deviation: Option<HardenedDeviationCategory>,
    subsystem: &'static str,
    action: &'static str,
    reason_code: &'static str,
    reason: String,
    input_digest: String,
    state_before: &'a str,
    output: &'a RespFrame,
}

impl Runtime {
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        Self {
            policy,
            store: Store::new(),
            evidence: EvidenceLedger::default(),
            tls_state: TlsRuntimeState::default(),
        }
    }

    #[must_use]
    pub fn default_strict() -> Self {
        Self::new(RuntimePolicy::default())
    }

    #[must_use]
    pub fn default_hardened() -> Self {
        Self::new(RuntimePolicy::hardened())
    }

    #[must_use]
    pub fn plan_event_loop_tick(
        pending_accepts: usize,
        pending_commands: usize,
        budget: TickBudget,
        mode: EventLoopMode,
    ) -> TickPlan {
        plan_tick(pending_accepts, pending_commands, budget, mode)
    }

    #[must_use]
    pub fn plan_event_loop_tick_with_tls_budget(
        pending_accepts: usize,
        pending_commands: usize,
        pending_tls_accepts: usize,
        max_new_tls_connections_per_cycle: usize,
        budget: TickBudget,
        mode: EventLoopMode,
    ) -> TickPlan {
        let mut plan = plan_tick(pending_accepts, pending_commands, budget, mode);
        let pending_tls_accepts = pending_tls_accepts.min(pending_accepts);
        let pending_non_tls_accepts = pending_accepts.saturating_sub(pending_tls_accepts);
        let tls_accept_plan = apply_tls_accept_rate_limit(
            plan.stats.accepted,
            pending_tls_accepts,
            pending_non_tls_accepts,
            max_new_tls_connections_per_cycle,
        );

        plan.stats.accepted = tls_accept_plan.total_accepted;
        plan.stats.accept_backlog_remaining = pending_accepts.saturating_sub(plan.stats.accepted);
        plan
    }

    pub fn replay_event_loop_phase_trace(
        trace: &[EventLoopPhase],
    ) -> Result<usize, PhaseReplayError> {
        replay_phase_trace(trace)
    }

    pub fn validate_event_loop_bootstrap(bootstrap: LoopBootstrap) -> Result<(), BootstrapError> {
        validate_bootstrap(bootstrap)
    }

    #[must_use]
    pub fn evidence(&self) -> &EvidenceLedger {
        &self.evidence
    }

    #[must_use]
    pub fn tls_runtime_state(&self) -> &TlsRuntimeState {
        &self.tls_state
    }

    pub fn apply_tls_config(
        &mut self,
        candidate: TlsConfig,
        now_ms: u64,
    ) -> Result<(), TlsCfgError> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(format!("{candidate:?}").as_bytes());
        let state_before = self.store.state_digest();

        let plan = match plan_tls_runtime_apply(&self.tls_state, candidate) {
            Ok(plan) => plan,
            Err(error) => {
                let preferred_deviation = preferred_tls_deviation_for_error(&error);
                let gated_error = self.gate_tls_error_for_mode(error, preferred_deviation);
                self.record_tls_config_event(
                    now_ms,
                    packet_id,
                    &input_digest,
                    &state_before,
                    &gated_error,
                    preferred_deviation,
                );
                return Err(gated_error);
            }
        };

        let mut next_state = self.tls_state.clone();
        match plan.listener_transition {
            TlsListenerTransition::Enable => next_state.tls_listener_enabled = true,
            TlsListenerTransition::Disable => next_state.tls_listener_enabled = false,
            TlsListenerTransition::Keep => {}
        }
        if plan.requires_context_swap {
            next_state.active_config = if plan.candidate_config.tls_enabled() {
                Some(plan.candidate_config.clone())
            } else {
                None
            };
        }
        if plan.requires_connection_type_configure {
            next_state.connection_type_configured = true;
        }
        self.tls_state = next_state;
        Ok(())
    }

    pub fn execute_frame(&mut self, frame: RespFrame, now_ms: u64) -> RespFrame {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(&frame.to_bytes());
        let state_before = self.store.state_digest();

        if let Some(reply) =
            self.preflight_gate(&frame, now_ms, packet_id, &input_digest, &state_before)
        {
            return reply;
        }

        let argv = match frame_to_argv(&frame) {
            Ok(argv) => argv,
            Err(_) => {
                let reply =
                    RespFrame::Error("ERR Protocol error: invalid command frame".to_string());
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ParserAbuse,
                    preferred_deviation: Some(HardenedDeviationCategory::BoundedParserDiagnostics),
                    subsystem: "router",
                    action: "reject_frame",
                    reason_code: "invalid_command_frame",
                    reason: "invalid command frame".to_string(),
                    input_digest,
                    state_before: &state_before,
                    output: &reply,
                });
                return reply;
            }
        };

        match dispatch_argv(&argv, &mut self.store, now_ms) {
            Ok(reply) => reply,
            Err(err) => command_error_to_resp(err),
        }
    }

    pub fn execute_bytes(&mut self, input: &[u8], now_ms: u64) -> Vec<u8> {
        let packet_id = next_packet_id();
        let input_digest = digest_bytes(input);
        let state_before = self.store.state_digest();
        match parse_frame(input) {
            Ok(parsed) => self.execute_frame(parsed.frame, now_ms).to_bytes(),
            Err(err) => {
                let reason = err.to_string();
                let reply = protocol_error_to_resp(err);
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ParserAbuse,
                    preferred_deviation: Some(HardenedDeviationCategory::BoundedParserDiagnostics),
                    subsystem: "protocol",
                    action: "parse_failure",
                    reason_code: "protocol_parse_failure",
                    reason,
                    input_digest,
                    state_before: &state_before,
                    output: &reply,
                });
                reply.to_bytes()
            }
        }
    }

    fn preflight_gate(
        &mut self,
        frame: &RespFrame,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
    ) -> Option<RespFrame> {
        let RespFrame::Array(Some(items)) = frame else {
            return None;
        };
        if items.len() > self.policy.gate.max_array_len {
            let reply = RespFrame::Error(
                "ERR Protocol error: command array exceeds compatibility gate".to_string(),
            );
            self.record_threat_event(ThreatEventInput {
                now_ms,
                packet_id,
                threat_class: ThreatClass::ResourceExhaustion,
                preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                subsystem: "compatibility_gate",
                action: "fail_closed_array_len",
                reason_code: "compat_array_len_exceeded",
                reason: format!(
                    "array length {} exceeded {}",
                    items.len(),
                    self.policy.gate.max_array_len
                ),
                input_digest: input_digest.to_string(),
                state_before,
                output: &reply,
            });
            return Some(reply);
        }

        for item in items {
            if let RespFrame::BulkString(Some(bytes)) = item
                && bytes.len() > self.policy.gate.max_bulk_len
            {
                let reply = RespFrame::Error(
                    "ERR Protocol error: bulk payload exceeds compatibility gate".to_string(),
                );
                self.record_threat_event(ThreatEventInput {
                    now_ms,
                    packet_id,
                    threat_class: ThreatClass::ResourceExhaustion,
                    preferred_deviation: Some(HardenedDeviationCategory::ResourceClamp),
                    subsystem: "compatibility_gate",
                    action: "fail_closed_bulk_len",
                    reason_code: "compat_bulk_len_exceeded",
                    reason: format!(
                        "bulk len {} exceeded {}",
                        bytes.len(),
                        self.policy.gate.max_bulk_len
                    ),
                    input_digest: input_digest.to_string(),
                    state_before,
                    output: &reply,
                });
                return Some(reply);
            }
        }
        None
    }

    fn record_threat_event(&mut self, input: ThreatEventInput<'_>) {
        if !self.policy.emit_evidence_ledger {
            return;
        }

        let (decision_action, severity) = self
            .policy
            .decide(input.threat_class, input.preferred_deviation);
        let state_after = self.store.state_digest();
        let output_digest = digest_bytes(&input.output.to_bytes());
        self.evidence.record(EvidenceEvent {
            ts_utc: format_ts_utc(input.now_ms),
            ts_ms: input.now_ms,
            packet_id: input.packet_id,
            mode: self.policy.mode,
            severity,
            threat_class: input.threat_class,
            decision_action,
            subsystem: input.subsystem,
            action: input.action,
            reason_code: input.reason_code,
            reason: input.reason,
            input_digest: input.input_digest,
            output_digest,
            state_digest_before: input.state_before.to_string(),
            state_digest_after: state_after,
            replay_cmd: format!(
                "cargo test -p fr-runtime -- --nocapture packet_{}",
                input.packet_id
            ),
            artifact_refs: vec![
                "SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md".to_string(),
                "PORTING_TO_RUST_ESSENCE_EXTRACTION_LEDGER_V1.md".to_string(),
            ],
            confidence: Some(1.0),
        });
    }

    fn gate_tls_error_for_mode(
        &self,
        error: TlsCfgError,
        preferred_deviation: HardenedDeviationCategory,
    ) -> TlsCfgError {
        if self.policy.mode != Mode::Hardened {
            return error;
        }
        match evaluate_tls_hardened_deviation(&self.policy, preferred_deviation) {
            Ok(_) => error,
            Err(gated_error) => gated_error,
        }
    }

    fn record_tls_config_event(
        &mut self,
        now_ms: u64,
        packet_id: u64,
        input_digest: &str,
        state_before: &str,
        error: &TlsCfgError,
        preferred_deviation: HardenedDeviationCategory,
    ) {
        let reply = RespFrame::Error(format!(
            "ERR TLS/config boundary violation ({})",
            error.reason_code()
        ));
        self.record_threat_event(ThreatEventInput {
            now_ms,
            packet_id,
            threat_class: ThreatClass::ConfigDowngradeAbuse,
            preferred_deviation: Some(preferred_deviation),
            subsystem: "tls_config",
            action: "reject_runtime_apply",
            reason_code: error.reason_code(),
            reason: error.to_string(),
            input_digest: input_digest.to_string(),
            state_before,
            output: &reply,
        });
    }
}

fn preferred_tls_deviation_for_error(error: &TlsCfgError) -> HardenedDeviationCategory {
    match error {
        TlsCfgError::OperationalKnobContractViolation(_) => {
            HardenedDeviationCategory::ResourceClamp
        }
        _ => HardenedDeviationCategory::MetadataSanitization,
    }
}

fn next_packet_id() -> u64 {
    PACKET_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn format_ts_utc(now_ms: u64) -> String {
    format!("unix_ms:{now_ms}")
}

fn digest_bytes(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn command_error_to_resp(error: CommandError) -> RespFrame {
    match error {
        CommandError::InvalidCommandFrame => {
            RespFrame::Error("ERR invalid command frame".to_string())
        }
        CommandError::InvalidUtf8Argument => {
            RespFrame::Error("ERR invalid UTF-8 argument".to_string())
        }
        CommandError::UnknownCommand {
            command,
            args_preview,
        } => {
            let mut out = format!("ERR unknown command '{}'", command);
            if let Some(args_preview) = args_preview {
                out.push_str(", with args beginning with: ");
                out.push_str(&args_preview);
            }
            RespFrame::Error(out)
        }
        CommandError::WrongArity(cmd) => RespFrame::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            cmd
        )),
        CommandError::InvalidInteger => {
            RespFrame::Error("ERR value is not an integer or out of range".to_string())
        }
        CommandError::SyntaxError => RespFrame::Error("ERR syntax error".to_string()),
        CommandError::NoSuchKey => RespFrame::Error("ERR no such key".to_string()),
        CommandError::Store(store_error) => match store_error {
            fr_store::StoreError::ValueNotInteger => {
                RespFrame::Error("ERR value is not an integer or out of range".to_string())
            }
            fr_store::StoreError::IntegerOverflow => {
                RespFrame::Error("ERR increment or decrement would overflow".to_string())
            }
            fr_store::StoreError::KeyNotFound => RespFrame::Error("ERR no such key".to_string()),
        },
    }
}

fn protocol_error_to_resp(error: RespParseError) -> RespFrame {
    match error {
        RespParseError::InvalidBulkLength => {
            RespFrame::Error("ERR Protocol error: invalid bulk length".to_string())
        }
        RespParseError::InvalidMultibulkLength => {
            RespFrame::Error("ERR Protocol error: invalid multibulk length".to_string())
        }
        RespParseError::Incomplete => {
            RespFrame::Error("ERR Protocol error: unexpected EOF while reading request".to_string())
        }
        RespParseError::InvalidPrefix(ch) => RespFrame::Error(format!(
            "ERR Protocol error: invalid RESP type prefix '{}'",
            char::from(ch)
        )),
        RespParseError::UnsupportedResp3Type(ch) => RespFrame::Error(format!(
            "ERR Protocol error: unsupported RESP3 type prefix '{}'",
            char::from(ch)
        )),
        RespParseError::InvalidInteger => {
            RespFrame::Error("ERR Protocol error: invalid integer payload".to_string())
        }
        RespParseError::InvalidUtf8 => {
            RespFrame::Error("ERR Protocol error: invalid UTF-8 payload".to_string())
        }
    }
}

pub mod ecosystem {
    /// Adapter boundary for Asupersync integration.
    /// This keeps `fr-runtime` decoupled while enabling project-level runtime wiring.
    pub trait AsyncRuntimeAdapter {
        fn spawn_named(&self, name: &str, task: Box<dyn FnOnce() + Send>);
    }

    /// Adapter boundary for FrankenTUI evidence and operator dashboards.
    pub trait OperatorUiAdapter {
        fn push_evidence_line(&self, line: &str);
    }
}

#[cfg(test)]
mod tests {
    use fr_config::{
        DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
        TlsAuthClients, TlsConfig, TlsProtocol,
    };
    use fr_eventloop::{
        EVENT_LOOP_PHASE_ORDER, EventLoopMode, EventLoopPhase, LoopBootstrap, TickBudget,
    };
    use fr_protocol::{RespFrame, parse_frame};

    use super::Runtime;

    #[test]
    fn fr_p2c_001_u001_runtime_exposes_deterministic_phase_order() {
        let plan =
            Runtime::plan_event_loop_tick(1, 3, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.phase_order, EVENT_LOOP_PHASE_ORDER);
    }

    #[test]
    fn fr_p2c_001_u003_runtime_no_sleep_when_backlog_present() {
        let plan =
            Runtime::plan_event_loop_tick(0, 1, TickBudget::default(), EventLoopMode::Normal);
        assert_eq!(plan.poll_timeout_ms, 0);
    }

    #[test]
    fn fr_p2c_001_u005_runtime_blocked_mode_is_bounded() {
        let plan = Runtime::plan_event_loop_tick(
            50,
            10_000,
            TickBudget::default(),
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
    fn fr_p2c_009_u011_runtime_tls_accept_limit_clamps_tls_accepts() {
        let plan = Runtime::plan_event_loop_tick_with_tls_budget(
            15,
            50,
            12,
            4,
            TickBudget {
                max_accepts: 10,
                max_commands: 100,
            },
            EventLoopMode::Normal,
        );
        assert_eq!(plan.stats.accepted, 7);
        assert_eq!(plan.stats.accept_backlog_remaining, 8);
    }

    #[test]
    fn fr_p2c_009_u011_runtime_tls_accept_limit_never_exceeds_total_budget() {
        let plan = Runtime::plan_event_loop_tick_with_tls_budget(
            20,
            1,
            20,
            64,
            TickBudget {
                max_accepts: 5,
                max_commands: 10,
            },
            EventLoopMode::Normal,
        );
        assert_eq!(plan.stats.accepted, 5);
        assert_eq!(plan.stats.accept_backlog_remaining, 15);
    }

    #[test]
    fn fr_p2c_001_u011_runtime_phase_replay_accepts_contract_order() {
        let ticks = Runtime::replay_event_loop_phase_trace(&EVENT_LOOP_PHASE_ORDER)
            .expect("valid phase trace");
        assert_eq!(ticks, 1);
    }

    #[test]
    fn fr_p2c_001_u011_runtime_phase_replay_rejects_invalid_start() {
        let err = Runtime::replay_event_loop_phase_trace(&[EventLoopPhase::Poll])
            .expect_err("invalid start");
        assert_eq!(err.reason_code(), "eventloop.main_loop_entry_missing");
    }

    #[test]
    fn fr_p2c_001_u010_runtime_bootstrap_validation_accepts_fully_wired() {
        Runtime::validate_event_loop_bootstrap(LoopBootstrap::fully_wired())
            .expect("fully wired bootstrap");
    }

    #[test]
    fn fr_p2c_001_u010_runtime_bootstrap_validation_rejects_missing_hook() {
        let err = Runtime::validate_event_loop_bootstrap(LoopBootstrap {
            before_sleep_hook_installed: false,
            after_sleep_hook_installed: true,
            server_cron_timer_installed: true,
        })
        .expect_err("missing hook");
        assert_eq!(err.reason_code(), "eventloop.hook_install_missing");
    }

    #[test]
    fn strict_ping_path() {
        let mut rt = Runtime::default_strict();
        let in_frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let out = rt.execute_frame(in_frame, 100);
        assert_eq!(out, RespFrame::SimpleString("PONG".to_string()));
    }

    #[test]
    fn compatibility_gate_trips_on_large_array() {
        let mut policy = RuntimePolicy::default();
        policy.gate.max_array_len = 1;
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let out = rt.execute_frame(in_frame, 100);
        assert!(matches!(out, RespFrame::Error(_)));
        assert_eq!(rt.evidence().events().len(), 1);
        let event = &rt.evidence().events()[0];
        assert_eq!(event.mode, Mode::Strict);
        assert_eq!(event.threat_class, ThreatClass::ResourceExhaustion);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "compat_array_len_exceeded");
        assert!(!event.input_digest.is_empty());
        assert!(!event.output_digest.is_empty());
        assert!(!event.state_digest_before.is_empty());
        assert!(!event.state_digest_after.is_empty());
    }

    #[test]
    fn unknown_command_error_includes_args_preview() {
        let mut rt = Runtime::default_strict();
        let frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"NOPE".to_vec())),
            RespFrame::BulkString(Some(b"a".to_vec())),
            RespFrame::BulkString(Some(b"b".to_vec())),
        ]));
        let out = rt.execute_frame(frame, 0);
        assert_eq!(
            out,
            RespFrame::Error(
                "ERR unknown command 'NOPE', with args beginning with: 'a' 'b' ".to_string()
            )
        );
    }

    #[test]
    fn protocol_invalid_bulk_length_error_string() {
        let mut rt = Runtime::default_strict();
        let raw = b"$-2\r\n";
        let encoded = rt.execute_bytes(raw, 0);
        let parsed = parse_frame(&encoded).expect("parse");
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR Protocol error: invalid bulk length".to_string())
        );
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ParserAbuse);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "protocol_parse_failure");
    }

    #[test]
    fn protocol_unsupported_resp3_type_error_string() {
        let mut rt = Runtime::default_strict();
        let raw = b"~1\r\n";
        let encoded = rt.execute_bytes(raw, 0);
        let parsed = parse_frame(&encoded).expect("parse");
        assert_eq!(
            parsed.frame,
            RespFrame::Error("ERR Protocol error: unsupported RESP3 type prefix '~'".to_string())
        );
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ParserAbuse);
        assert_eq!(event.severity, DriftSeverity::S0);
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.reason_code, "protocol_parse_failure");
    }

    #[test]
    fn hardened_mode_allowlisted_gate_uses_bounded_defense() {
        let mut policy = RuntimePolicy::hardened();
        policy.gate.max_array_len = 1;
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let _ = rt.execute_frame(in_frame, 42);
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.mode, Mode::Hardened);
        assert_eq!(event.decision_action, DecisionAction::BoundedDefense);
        assert_eq!(event.severity, DriftSeverity::S1);
    }

    #[test]
    fn hardened_mode_without_allowlist_rejects_non_allowlisted() {
        let mut policy = RuntimePolicy::hardened();
        policy.gate.max_array_len = 1;
        policy
            .hardened_allowlist
            .retain(|c| *c != HardenedDeviationCategory::ResourceClamp);
        let mut rt = Runtime::new(policy);
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let _ = rt.execute_frame(in_frame, 42);
        let event = rt.evidence().events().last().expect("event");
        assert_eq!(event.mode, Mode::Hardened);
        assert_eq!(event.decision_action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(event.severity, DriftSeverity::S2);
    }

    fn valid_tls_config() -> TlsConfig {
        TlsConfig {
            tls_port: Some(6380),
            cert_file: Some("cert.pem".to_string()),
            key_file: Some("key.pem".to_string()),
            ca_file: Some("ca.pem".to_string()),
            protocols: vec![TlsProtocol::TlsV1_2, TlsProtocol::TlsV1_3],
            ciphers: Some("HIGH:!aNULL".to_string()),
            auth_clients: TlsAuthClients::Required,
            cluster_announce_tls_port: Some(16380),
            max_new_tls_connections_per_cycle: 64,
        }
    }

    #[test]
    fn fr_p2c_009_u010_runtime_apply_updates_tls_state() {
        let mut runtime = Runtime::default_strict();
        runtime
            .apply_tls_config(valid_tls_config(), 123)
            .expect("valid TLS config");
        let tls_state = runtime.tls_runtime_state();
        assert!(tls_state.tls_listener_enabled);
        assert!(tls_state.connection_type_configured);
        assert!(tls_state.active_config.is_some());
    }

    #[test]
    fn fr_p2c_009_u013_strict_mode_rejects_unsafe_tls_config_and_records_event() {
        let mut runtime = Runtime::default_strict();
        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;
        let err = runtime
            .apply_tls_config(invalid, 321)
            .expect_err("must fail closed");
        assert_eq!(err.reason_code(), "tlscfg.safety_gate_contract_violation");

        let event = runtime.evidence().events().last().expect("event");
        assert_eq!(event.threat_class, ThreatClass::ConfigDowngradeAbuse);
        assert_eq!(event.reason_code, "tlscfg.safety_gate_contract_violation");
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.severity, DriftSeverity::S0);
    }

    #[test]
    fn fr_p2c_009_u013_hardened_non_allowlisted_tls_deviation_is_rejected() {
        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::MetadataSanitization);
        let mut runtime = Runtime::new(policy);

        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;
        let err = runtime
            .apply_tls_config(invalid, 456)
            .expect_err("must reject non-allowlisted");
        assert_eq!(err.reason_code(), "tlscfg.hardened_nonallowlisted_rejected");

        let event = runtime.evidence().events().last().expect("event");
        assert_eq!(event.reason_code, "tlscfg.hardened_nonallowlisted_rejected");
        assert_eq!(event.decision_action, DecisionAction::RejectNonAllowlisted);
        assert_eq!(event.severity, DriftSeverity::S2);
    }
}
