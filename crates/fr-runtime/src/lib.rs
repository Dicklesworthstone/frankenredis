#![forbid(unsafe_code)]

use fr_command::{CommandError, dispatch_argv, frame_to_argv};
use fr_protocol::{RespFrame, RespParseError, parse_frame};
use fr_store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityMode {
    Strict,
    Hardened,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub mode: CompatibilityMode,
    pub max_array_len: usize,
    pub max_bulk_len: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            mode: CompatibilityMode::Strict,
            max_array_len: 1024,
            max_bulk_len: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceEvent {
    pub ts_ms: u64,
    pub subsystem: &'static str,
    pub action: String,
    pub reason: String,
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
    config: RuntimeConfig,
    store: Store,
    evidence: EvidenceLedger,
}

impl Runtime {
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            store: Store::new(),
            evidence: EvidenceLedger::default(),
        }
    }

    #[must_use]
    pub fn default_strict() -> Self {
        Self::new(RuntimeConfig::default())
    }

    #[must_use]
    pub fn evidence(&self) -> &EvidenceLedger {
        &self.evidence
    }

    pub fn execute_frame(&mut self, frame: RespFrame, now_ms: u64) -> RespFrame {
        if let Some(reply) = self.preflight_gate(&frame, now_ms) {
            return reply;
        }

        let argv = match frame_to_argv(&frame) {
            Ok(argv) => argv,
            Err(_) => {
                self.evidence.record(EvidenceEvent {
                    ts_ms: now_ms,
                    subsystem: "router",
                    action: "reject_frame".to_string(),
                    reason: "invalid command frame".to_string(),
                    confidence: Some(1.0),
                });
                return RespFrame::Error("ERR Protocol error: invalid command frame".to_string());
            }
        };

        match dispatch_argv(&argv, &mut self.store, now_ms) {
            Ok(reply) => reply,
            Err(err) => command_error_to_resp(err),
        }
    }

    pub fn execute_bytes(&mut self, input: &[u8], now_ms: u64) -> Vec<u8> {
        match parse_frame(input) {
            Ok(parsed) => self.execute_frame(parsed.frame, now_ms).to_bytes(),
            Err(err) => {
                self.evidence.record(EvidenceEvent {
                    ts_ms: now_ms,
                    subsystem: "protocol",
                    action: "parse_failure".to_string(),
                    reason: err.to_string(),
                    confidence: Some(1.0),
                });
                protocol_error_to_resp(err).to_bytes()
            }
        }
    }

    fn preflight_gate(&mut self, frame: &RespFrame, now_ms: u64) -> Option<RespFrame> {
        let RespFrame::Array(Some(items)) = frame else {
            return None;
        };
        if items.len() > self.config.max_array_len {
            self.evidence.record(EvidenceEvent {
                ts_ms: now_ms,
                subsystem: "compatibility_gate",
                action: "fail_closed_array_len".to_string(),
                reason: format!(
                    "array length {} exceeded {}",
                    items.len(),
                    self.config.max_array_len
                ),
                confidence: Some(1.0),
            });
            return Some(RespFrame::Error(
                "ERR Protocol error: command array exceeds compatibility gate".to_string(),
            ));
        }

        for item in items {
            if let RespFrame::BulkString(Some(bytes)) = item
                && bytes.len() > self.config.max_bulk_len
            {
                self.evidence.record(EvidenceEvent {
                    ts_ms: now_ms,
                    subsystem: "compatibility_gate",
                    action: "fail_closed_bulk_len".to_string(),
                    reason: format!(
                        "bulk len {} exceeded {}",
                        bytes.len(),
                        self.config.max_bulk_len
                    ),
                    confidence: Some(1.0),
                });
                return Some(RespFrame::Error(
                    "ERR Protocol error: bulk payload exceeds compatibility gate".to_string(),
                ));
            }
        }
        None
    }
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
    use fr_protocol::{RespFrame, parse_frame};

    use super::{Runtime, RuntimeConfig};

    #[test]
    fn strict_ping_path() {
        let mut rt = Runtime::default_strict();
        let in_frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"PING".to_vec()))]));
        let out = rt.execute_frame(in_frame, 100);
        assert_eq!(out, RespFrame::SimpleString("PONG".to_string()));
    }

    #[test]
    fn compatibility_gate_trips_on_large_array() {
        let mut rt = Runtime::new(RuntimeConfig {
            max_array_len: 1,
            ..RuntimeConfig::default()
        });
        let in_frame = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"PING".to_vec())),
            RespFrame::BulkString(Some(b"x".to_vec())),
        ]));
        let out = rt.execute_frame(in_frame, 100);
        assert!(matches!(out, RespFrame::Error(_)));
        assert_eq!(rt.evidence().events().len(), 1);
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
    }
}
