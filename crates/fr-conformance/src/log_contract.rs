#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fr_config::Mode;
use fr_runtime::EvidenceEvent;
use serde::{Deserialize, Serialize};

pub const STRUCTURED_LOG_SCHEMA_VERSION: &str = "fr_testlog_v1";

pub const PACKET_FAMILIES: [&str; 9] = [
    "FR-P2C-001",
    "FR-P2C-002",
    "FR-P2C-003",
    "FR-P2C-004",
    "FR-P2C-005",
    "FR-P2C-006",
    "FR-P2C-007",
    "FR-P2C-008",
    "FR-P2C-009",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPath {
    Unit,
    Property,
    E2e,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutcome {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogMode {
    Strict,
    Hardened,
}

impl LogMode {
    #[must_use]
    pub const fn as_env_value(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Hardened => "hardened",
        }
    }
}

impl From<Mode> for LogMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Strict => Self::Strict,
            Mode::Hardened => Self::Hardened,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredLogEvent {
    pub schema_version: String,
    pub ts_utc: String,
    pub suite_id: String,
    pub test_or_scenario_id: String,
    pub packet_id: String,
    pub mode: LogMode,
    pub verification_path: VerificationPath,
    pub seed: u64,
    pub input_digest: String,
    pub output_digest: String,
    pub duration_ms: u64,
    pub outcome: LogOutcome,
    pub reason_code: String,
    pub replay_cmd: String,
    pub artifact_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeEvidenceContext<'a> {
    pub suite_id: &'a str,
    pub test_or_scenario_id: &'a str,
    pub packet_id: &'a str,
    pub verification_path: VerificationPath,
    pub seed: u64,
    pub duration_ms: u64,
    pub outcome: LogOutcome,
    pub fixture_id: Option<&'a str>,
    pub env_ref: Option<&'a str>,
}

impl StructuredLogEvent {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != STRUCTURED_LOG_SCHEMA_VERSION {
            return Err(format!(
                "schema_version expected '{}', got '{}'",
                STRUCTURED_LOG_SCHEMA_VERSION, self.schema_version
            ));
        }

        require_non_empty("ts_utc", &self.ts_utc)?;
        require_non_empty("suite_id", &self.suite_id)?;
        require_non_empty("test_or_scenario_id", &self.test_or_scenario_id)?;
        require_non_empty("packet_id", &self.packet_id)?;
        require_non_empty("input_digest", &self.input_digest)?;
        require_non_empty("output_digest", &self.output_digest)?;
        require_non_empty("reason_code", &self.reason_code)?;
        require_non_empty("replay_cmd", &self.replay_cmd)?;

        if self.artifact_refs.is_empty() {
            return Err("artifact_refs must not be empty".to_string());
        }
        for (idx, artifact_ref) in self.artifact_refs.iter().enumerate() {
            if artifact_ref.trim().is_empty() {
                return Err(format!("artifact_refs[{idx}] must not be empty"));
            }
        }
        if let Some(fixture_id) = &self.fixture_id {
            require_non_empty("fixture_id", fixture_id)?;
        }
        if let Some(env_ref) = &self.env_ref {
            require_non_empty("env_ref", env_ref)?;
        }

        Ok(())
    }

    pub fn to_json_line(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(self)
            .map_err(|err| format!("failed to serialize structured log event: {err}"))
    }

    pub fn from_runtime_evidence(
        event: &EvidenceEvent,
        context: RuntimeEvidenceContext<'_>,
    ) -> Result<Self, String> {
        let out = Self {
            schema_version: STRUCTURED_LOG_SCHEMA_VERSION.to_string(),
            ts_utc: event.ts_utc.clone(),
            suite_id: context.suite_id.to_string(),
            test_or_scenario_id: context.test_or_scenario_id.to_string(),
            packet_id: context.packet_id.to_string(),
            mode: event.mode.into(),
            verification_path: context.verification_path,
            seed: context.seed,
            input_digest: event.input_digest.clone(),
            output_digest: event.output_digest.clone(),
            duration_ms: context.duration_ms,
            outcome: context.outcome,
            reason_code: event.reason_code.to_string(),
            replay_cmd: event.replay_cmd.clone(),
            artifact_refs: event.artifact_refs.clone(),
            fixture_id: context.fixture_id.map(std::string::ToString::to_string),
            env_ref: context.env_ref.map(std::string::ToString::to_string),
        };
        out.validate()?;
        Ok(out)
    }
}

fn require_non_empty(field_name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field_name} must not be empty"));
    }
    Ok(())
}

#[must_use]
pub fn unit_replay_cmd(
    crate_name: &str,
    test_or_scenario_id: &str,
    mode: LogMode,
    seed: u64,
) -> String {
    format!(
        "FR_MODE={} FR_SEED={} cargo test -p {crate_name} {test_or_scenario_id} -- --nocapture",
        mode.as_env_value(),
        seed
    )
}

#[must_use]
pub fn e2e_replay_cmd(test_or_scenario_id: &str, mode: LogMode, seed: u64) -> String {
    format!(
        "FR_MODE={} FR_SEED={} cargo test -p fr-conformance --test smoke -- --nocapture {test_or_scenario_id}",
        mode.as_env_value(),
        seed
    )
}

pub fn golden_packet_logs(packet_id: &str) -> Result<[StructuredLogEvent; 2], String> {
    if !PACKET_FAMILIES.contains(&packet_id) {
        return Err(format!("unknown packet family '{packet_id}'"));
    }

    let packet_slug = packet_id.to_ascii_lowercase();
    let unit_test_id = format!("{packet_slug}::unit_contract_smoke");
    let e2e_scenario_id = format!("{packet_slug}::e2e_contract_smoke");
    let artifact_file =
        format!("crates/fr-conformance/fixtures/log_contract_v1/{packet_id}.golden.jsonl");

    let unit = StructuredLogEvent {
        schema_version: STRUCTURED_LOG_SCHEMA_VERSION.to_string(),
        ts_utc: "2026-02-14T00:00:00Z".to_string(),
        suite_id: format!("unit::{packet_slug}"),
        test_or_scenario_id: unit_test_id.clone(),
        packet_id: packet_id.to_string(),
        mode: LogMode::Strict,
        verification_path: VerificationPath::Unit,
        seed: 17,
        input_digest: deterministic_digest(&format!("{packet_id}:unit:input")),
        output_digest: deterministic_digest(&format!("{packet_id}:unit:output")),
        duration_ms: 7,
        outcome: LogOutcome::Pass,
        reason_code: "parity_ok".to_string(),
        replay_cmd: unit_replay_cmd("fr-runtime", &unit_test_id, LogMode::Strict, 17),
        artifact_refs: vec![
            "TEST_LOG_SCHEMA_V1.md".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/manifest.json".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/repro.lock".to_string(),
            artifact_file.clone(),
        ],
        fixture_id: Some(format!("{packet_id}::unit_fixture")),
        env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string()),
    };

    let e2e = StructuredLogEvent {
        schema_version: STRUCTURED_LOG_SCHEMA_VERSION.to_string(),
        ts_utc: "2026-02-14T00:00:01Z".to_string(),
        suite_id: format!("e2e::{packet_slug}"),
        test_or_scenario_id: e2e_scenario_id.clone(),
        packet_id: packet_id.to_string(),
        mode: LogMode::Hardened,
        verification_path: VerificationPath::E2e,
        seed: 42,
        input_digest: deterministic_digest(&format!("{packet_id}:e2e:input")),
        output_digest: deterministic_digest(&format!("{packet_id}:e2e:output")),
        duration_ms: 11,
        outcome: LogOutcome::Pass,
        reason_code: "journey_ok".to_string(),
        replay_cmd: e2e_replay_cmd(&e2e_scenario_id, LogMode::Hardened, 42),
        artifact_refs: vec![
            "TEST_LOG_SCHEMA_V1.md".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/manifest.json".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string(),
            "crates/fr-conformance/fixtures/log_contract_v1/repro.lock".to_string(),
            artifact_file,
        ],
        fixture_id: Some(format!("{packet_id}::e2e_fixture")),
        env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string()),
    };

    Ok([unit, e2e])
}

fn deterministic_digest(label: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in label.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[must_use]
pub fn live_log_output_path(root: &Path, suite_id: &str, fixture_name: &str) -> PathBuf {
    let suite_slug = sanitize_path_segment(suite_id);
    let fixture_base = fixture_name.strip_suffix(".json").unwrap_or(fixture_name);
    let fixture_slug = sanitize_path_segment(fixture_base);
    root.join(suite_slug).join(format!("{fixture_slug}.jsonl"))
}

pub fn append_structured_log_jsonl(
    path: &Path,
    events: &[StructuredLogEvent],
) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create log directory {}: {err}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| {
            format!(
                "failed to open structured log file {}: {err}",
                path.display()
            )
        })?;
    for event in events {
        let line = event.to_json_line()?;
        writeln!(file, "{line}").map_err(|err| {
            format!(
                "failed to append structured log line {}: {err}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn sanitize_path_segment(input: &str) -> String {
    let out = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use fr_config::{DecisionAction, DriftSeverity, Mode, ThreatClass};
    use fr_runtime::EvidenceEvent;

    use super::{
        LogMode, LogOutcome, PACKET_FAMILIES, RuntimeEvidenceContext,
        STRUCTURED_LOG_SCHEMA_VERSION, StructuredLogEvent, VerificationPath,
        append_structured_log_jsonl, e2e_replay_cmd, golden_packet_logs, live_log_output_path,
        unit_replay_cmd,
    };

    fn sample_runtime_event() -> EvidenceEvent {
        EvidenceEvent {
            ts_utc: "2026-02-14T00:00:00Z".to_string(),
            ts_ms: 1,
            packet_id: 11,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "input_digest".to_string(),
            output_digest: "output_digest".to_string(),
            state_digest_before: "before".to_string(),
            state_digest_after: "after".to_string(),
            replay_cmd: "cargo test -p fr-runtime parse_failure".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string(),
            ],
            confidence: Some(1.0),
        }
    }

    #[test]
    fn replay_templates_are_deterministic() {
        let unit = unit_replay_cmd("fr-runtime", "packet_1", LogMode::Strict, 7);
        assert_eq!(
            unit,
            "FR_MODE=strict FR_SEED=7 cargo test -p fr-runtime packet_1 -- --nocapture"
        );

        let e2e = e2e_replay_cmd("journey_1", LogMode::Hardened, 9);
        assert_eq!(
            e2e,
            "FR_MODE=hardened FR_SEED=9 cargo test -p fr-conformance --test smoke -- --nocapture journey_1"
        );
    }

    #[test]
    fn golden_packet_logs_validate_for_every_packet_family() {
        for packet_id in PACKET_FAMILIES {
            let logs = golden_packet_logs(packet_id).expect("packet logs");
            assert_eq!(logs[0].verification_path, VerificationPath::Unit);
            assert_eq!(logs[1].verification_path, VerificationPath::E2e);
            logs[0].validate().expect("unit log validates");
            logs[1].validate().expect("e2e log validates");
            assert_eq!(logs[0].schema_version, STRUCTURED_LOG_SCHEMA_VERSION);
            assert_eq!(logs[1].schema_version, STRUCTURED_LOG_SCHEMA_VERSION);
        }
    }

    #[test]
    fn unknown_packet_family_is_rejected() {
        let err = golden_packet_logs("FR-P2C-404").expect_err("must fail");
        assert!(err.contains("unknown packet family"));
    }

    #[test]
    fn runtime_evidence_conversion_emits_valid_structured_log_event() {
        let runtime_event = sample_runtime_event();
        let structured = StructuredLogEvent::from_runtime_evidence(
            &runtime_event,
            RuntimeEvidenceContext {
                suite_id: "e2e::protocol_negative",
                test_or_scenario_id: "protocol_negative::invalid_bulk_len",
                packet_id: "FR-P2C-002",
                verification_path: VerificationPath::E2e,
                seed: 7,
                duration_ms: 3,
                outcome: LogOutcome::Pass,
                fixture_id: Some("protocol_negative.json"),
                env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json"),
            },
        )
        .expect("conversion must succeed");
        assert_eq!(structured.schema_version, STRUCTURED_LOG_SCHEMA_VERSION);
        assert_eq!(structured.mode, LogMode::Strict);
        assert_eq!(structured.packet_id, "FR-P2C-002");
        assert_eq!(structured.reason_code, "protocol_parse_failure");
        structured.validate().expect("event validates");
    }

    #[test]
    fn runtime_evidence_conversion_rejects_empty_artifact_refs() {
        let mut runtime_event = sample_runtime_event();
        runtime_event.artifact_refs.clear();
        let err = StructuredLogEvent::from_runtime_evidence(
            &runtime_event,
            RuntimeEvidenceContext {
                suite_id: "e2e::protocol_negative",
                test_or_scenario_id: "protocol_negative::invalid_bulk_len",
                packet_id: "FR-P2C-002",
                verification_path: VerificationPath::E2e,
                seed: 7,
                duration_ms: 3,
                outcome: LogOutcome::Fail,
                fixture_id: Some("protocol_negative.json"),
                env_ref: None,
            },
        )
        .expect_err("empty artifact refs should fail validation");
        assert!(err.contains("artifact_refs"));
    }

    #[test]
    fn live_log_output_path_is_deterministic() {
        let path = live_log_output_path(
            std::path::Path::new("artifacts/log_contract/live"),
            "live_redis_diff::core/errors",
            "core_errors.json",
        );
        assert_eq!(
            path,
            std::path::Path::new("artifacts/log_contract/live")
                .join("live_redis_diff__core_errors")
                .join("core_errors.jsonl")
        );
    }

    #[test]
    fn append_structured_log_jsonl_appends_lines() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock moved backwards")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("fr_conformance_log_contract_{unique}"));
        let path = dir.join("live/core_errors.jsonl");

        let mut first = StructuredLogEvent::from_runtime_evidence(
            &sample_runtime_event(),
            RuntimeEvidenceContext {
                suite_id: "live_redis_diff::core_errors",
                test_or_scenario_id: "case_one",
                packet_id: "FR-P2C-003",
                verification_path: VerificationPath::E2e,
                seed: 10,
                duration_ms: 1,
                outcome: LogOutcome::Pass,
                fixture_id: Some("core_errors.json"),
                env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json"),
            },
        )
        .expect("first structured event");
        first.reason_code = "first_reason".to_string();
        append_structured_log_jsonl(&path, &[first]).expect("append first line");

        let mut second = StructuredLogEvent::from_runtime_evidence(
            &sample_runtime_event(),
            RuntimeEvidenceContext {
                suite_id: "live_redis_diff::core_errors",
                test_or_scenario_id: "case_two",
                packet_id: "FR-P2C-003",
                verification_path: VerificationPath::E2e,
                seed: 11,
                duration_ms: 1,
                outcome: LogOutcome::Fail,
                fixture_id: Some("core_errors.json"),
                env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json"),
            },
        )
        .expect("second structured event");
        second.reason_code = "second_reason".to_string();
        append_structured_log_jsonl(&path, &[second]).expect("append second line");

        let raw = fs::read_to_string(&path).expect("read appended file");
        let lines = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "two appended records expected");
        let first_event: StructuredLogEvent = serde_json::from_str(lines[0]).expect("parse first");
        let second_event: StructuredLogEvent =
            serde_json::from_str(lines[1]).expect("parse second");
        assert_eq!(first_event.reason_code, "first_reason");
        assert_eq!(second_event.reason_code, "second_reason");

        fs::remove_dir_all(&dir).expect("cleanup temp directory");
    }
}
