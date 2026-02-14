#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use fr_command::{CommandError, dispatch_argv, frame_to_argv};
use fr_config::{
    DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
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
    pub fn evidence(&self) -> &EvidenceLedger {
        &self.evidence
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
        CommandError::Store(store_error) => match store_error {
            fr_store::StoreError::ValueNotInteger => {
                RespFrame::Error("ERR value is not an integer or out of range".to_string())
            }
            fr_store::StoreError::IntegerOverflow => {
                RespFrame::Error("ERR increment or decrement would overflow".to_string())
            }
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
    };
    use fr_protocol::{RespFrame, parse_frame};

    use super::Runtime;

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
}
