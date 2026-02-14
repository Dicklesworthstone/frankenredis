#![forbid(unsafe_code)]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use fr_config::{DecisionAction, DriftSeverity, ThreatClass};
use fr_persist::AofRecord;
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::{EvidenceEvent, Runtime};
use serde::{Deserialize, Serialize};

use crate::log_contract::{
    LogOutcome, RuntimeEvidenceContext, StructuredLogEvent, VerificationPath,
};

pub mod log_contract;
pub mod phase2c_schema;

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub oracle_root: PathBuf,
    pub fixture_root: PathBuf,
    pub strict_mode: bool,
}

impl HarnessConfig {
    #[must_use]
    pub fn default_paths() -> Self {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        Self {
            oracle_root: repo_root.join("legacy_redis_code/redis"),
            fixture_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
            strict_mode: true,
        }
    }
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self::default_paths()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessReport {
    pub suite: &'static str,
    pub oracle_present: bool,
    pub fixture_count: usize,
    pub strict_mode: bool,
}

#[must_use]
pub fn run_smoke(config: &HarnessConfig) -> HarnessReport {
    let fixture_count = fs::read_dir(&config.fixture_root)
        .ok()
        .into_iter()
        .flat_map(|it| it.filter_map(Result::ok))
        .count();

    HarnessReport {
        suite: "smoke",
        oracle_present: config.oracle_root.exists(),
        fixture_count,
        strict_mode: config.strict_mode,
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConformanceFixture {
    pub suite: String,
    pub cases: Vec<ConformanceCase>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConformanceCase {
    pub name: String,
    pub now_ms: u64,
    pub argv: Vec<String>,
    pub expect: ExpectedFrame,
    #[serde(default)]
    pub expect_threat: Option<ExpectedThreat>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpectedFrame {
    Simple { value: String },
    Error { value: String },
    Integer { value: i64 },
    Bulk { value: Option<String> },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExpectedThreat {
    pub threat_class: String,
    pub severity: String,
    pub decision_action: String,
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub subsystem: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseOutcome {
    pub name: String,
    pub passed: bool,
    pub expected: RespFrame,
    pub actual: RespFrame,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialReport {
    pub suite: String,
    pub total: usize,
    pub passed: usize,
    pub failed: Vec<CaseOutcome>,
}

#[derive(Debug, Clone)]
pub struct LiveOracleConfig {
    pub host: String,
    pub port: u16,
    pub io_timeout_ms: u64,
    pub align_timing_from_fixture: bool,
}

impl Default for LiveOracleConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 6379,
            io_timeout_ms: 2_000,
            align_timing_from_fixture: true,
        }
    }
}

pub fn run_fixture(
    config: &HarnessConfig,
    fixture_name: &str,
) -> Result<DifferentialReport, String> {
    let path = config.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read fixture {}: {err}", path.display()))?;
    let fixture: ConformanceFixture = serde_json::from_str(&raw)
        .map_err(|err| format!("invalid fixture JSON {}: {err}", path.display()))?;

    let mut runtime = Runtime::default_strict();
    let mut failed = Vec::new();
    let total = fixture.cases.len();
    for case in fixture.cases {
        let evidence_before = runtime.evidence().events().len();
        let frame = case_to_frame(&case);
        let actual = runtime.execute_frame(frame, case.now_ms);
        let expected = expected_to_frame(&case.expect);
        let new_events = &runtime.evidence().events()[evidence_before..];
        let threat_result = validate_threat_expectation(case.expect_threat.as_ref(), new_events);
        let log_result = validate_structured_log_emission(
            &fixture.suite,
            fixture_name,
            &case.name,
            VerificationPath::E2e,
            case.now_ms,
            new_events,
        );
        let frame_ok = actual == expected;
        let passed = frame_ok && threat_result.is_ok() && log_result.is_ok();
        if !passed {
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected,
                actual,
                detail: build_case_detail(frame_ok, threat_result.err(), log_result.err()),
            });
        }
    }

    Ok(DifferentialReport {
        suite: fixture.suite,
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
    })
}

pub fn run_live_redis_diff(
    config: &HarnessConfig,
    fixture_name: &str,
    oracle: &LiveOracleConfig,
) -> Result<DifferentialReport, String> {
    let fixture = load_conformance_fixture(config, fixture_name)?;
    let mut runtime = Runtime::default_strict();
    let mut stream = connect_live_redis(oracle)?;
    flushall(&mut stream)?;

    let mut failed = Vec::new();
    let total = fixture.cases.len();
    let mut prev_now_ms: Option<u64> = None;
    for case in fixture.cases {
        if oracle.align_timing_from_fixture {
            if let Some(previous) = prev_now_ms {
                let delta_ms = case.now_ms.saturating_sub(previous);
                if delta_ms > 0 {
                    sleep(Duration::from_millis(delta_ms));
                }
            }
            prev_now_ms = Some(case.now_ms);
        }

        let frame = case_to_frame(&case);
        let runtime_actual = runtime.execute_frame(frame.clone(), case.now_ms);
        send_frame(&mut stream, &frame)?;
        let redis_actual = read_resp_frame_from_stream(&mut stream)?;
        if runtime_actual != redis_actual {
            failed.push(CaseOutcome {
                name: case.name,
                passed: false,
                expected: redis_actual,
                actual: runtime_actual,
                detail: Some("frame mismatch against live redis".to_string()),
            });
        }
    }

    Ok(DifferentialReport {
        suite: format!("live_redis_diff::{}", fixture.suite),
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProtocolFixture {
    pub suite: String,
    pub cases: Vec<ProtocolCase>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProtocolCase {
    pub name: String,
    pub now_ms: u64,
    pub raw_request: String,
    pub expect: ExpectedFrame,
    #[serde(default)]
    pub expect_threat: Option<ExpectedThreat>,
}

pub fn run_protocol_fixture(
    config: &HarnessConfig,
    fixture_name: &str,
) -> Result<DifferentialReport, String> {
    let path = config.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read fixture {}: {err}", path.display()))?;
    let fixture: ProtocolFixture = serde_json::from_str(&raw)
        .map_err(|err| format!("invalid fixture JSON {}: {err}", path.display()))?;

    let mut runtime = Runtime::default_strict();
    let mut failed = Vec::new();
    let total = fixture.cases.len();
    for case in fixture.cases {
        let evidence_before = runtime.evidence().events().len();
        let encoded = runtime.execute_bytes(case.raw_request.as_bytes(), case.now_ms);
        let actual = parse_frame(&encoded)
            .map_err(|err| format!("runtime emitted invalid RESP frame in {}: {err}", case.name))?
            .frame;
        let expected = expected_to_frame(&case.expect);
        let new_events = &runtime.evidence().events()[evidence_before..];
        let threat_result = validate_threat_expectation(case.expect_threat.as_ref(), new_events);
        let log_result = validate_structured_log_emission(
            &fixture.suite,
            fixture_name,
            &case.name,
            VerificationPath::E2e,
            case.now_ms,
            new_events,
        );
        let frame_ok = actual == expected;
        let passed = frame_ok && threat_result.is_ok() && log_result.is_ok();
        if !passed {
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected,
                actual,
                detail: build_case_detail(frame_ok, threat_result.err(), log_result.err()),
            });
        }
    }

    Ok(DifferentialReport {
        suite: fixture.suite,
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
    })
}

pub fn run_live_redis_protocol_diff(
    config: &HarnessConfig,
    fixture_name: &str,
    oracle: &LiveOracleConfig,
) -> Result<DifferentialReport, String> {
    let fixture = load_protocol_fixture(config, fixture_name)?;
    let mut runtime = Runtime::default_strict();
    let mut failed = Vec::new();
    let total = fixture.cases.len();

    for case in fixture.cases {
        let mut stream = connect_live_redis(oracle)?;
        let raw = case.raw_request.as_bytes();
        stream
            .write_all(raw)
            .map_err(|err| format!("failed to send protocol payload in {}: {err}", case.name))?;
        stream
            .flush()
            .map_err(|err| format!("failed to flush protocol payload in {}: {err}", case.name))?;

        let redis_actual = read_resp_frame_from_stream(&mut stream)?;
        let runtime_encoded = runtime.execute_bytes(raw, case.now_ms);
        let runtime_actual = parse_frame(&runtime_encoded)
            .map_err(|err| format!("runtime emitted invalid RESP frame in {}: {err}", case.name))?
            .frame;

        if runtime_actual != redis_actual {
            failed.push(CaseOutcome {
                name: case.name,
                passed: false,
                expected: redis_actual,
                actual: runtime_actual,
                detail: Some("frame mismatch against live redis".to_string()),
            });
        }
    }

    Ok(DifferentialReport {
        suite: format!("live_redis_protocol_diff::{}", fixture.suite),
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplayFixture {
    pub suite: String,
    pub cases: Vec<ReplayCase>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplayCase {
    pub name: String,
    pub now_ms: u64,
    pub records: Vec<Vec<String>>,
    pub assertions: Vec<ReplayAssertion>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReplayAssertion {
    pub key: String,
    pub expect: Option<String>,
    pub at_ms: Option<u64>,
}

pub fn run_replay_fixture(
    config: &HarnessConfig,
    fixture_name: &str,
) -> Result<DifferentialReport, String> {
    let path = config.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read fixture {}: {err}", path.display()))?;
    let fixture: ReplayFixture = serde_json::from_str(&raw)
        .map_err(|err| format!("invalid fixture JSON {}: {err}", path.display()))?;

    let mut failed = Vec::new();
    let mut total = 0_usize;
    for case in fixture.cases {
        let mut runtime = Runtime::default_strict();
        for record in case.records {
            let aof = AofRecord {
                argv: record
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
            };
            let frame = aof.to_resp_frame();
            let _ = runtime.execute_frame(frame, case.now_ms);
        }

        for assertion in case.assertions {
            total = total.saturating_add(1);
            let at_ms = assertion.at_ms.unwrap_or(case.now_ms);
            let get = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(assertion.key.as_bytes().to_vec())),
            ]));
            let actual = runtime.execute_frame(get, at_ms);
            let expected = RespFrame::BulkString(assertion.expect.map(|v| v.as_bytes().to_vec()));
            let passed = actual == expected;
            if !passed {
                failed.push(CaseOutcome {
                    name: format!("{}::{}", case.name, assertion.key),
                    passed,
                    expected,
                    actual,
                    detail: Some("replay assertion mismatch".to_string()),
                });
            }
        }
    }

    Ok(DifferentialReport {
        suite: fixture.suite,
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
    })
}

fn load_conformance_fixture(
    config: &HarnessConfig,
    fixture_name: &str,
) -> Result<ConformanceFixture, String> {
    let path = config.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read fixture {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid fixture JSON {}: {err}", path.display()))
}

fn load_protocol_fixture(
    config: &HarnessConfig,
    fixture_name: &str,
) -> Result<ProtocolFixture, String> {
    let path = config.fixture_root.join(fixture_name);
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read fixture {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid fixture JSON {}: {err}", path.display()))
}

fn connect_live_redis(oracle: &LiveOracleConfig) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", oracle.host, oracle.port);
    let stream = TcpStream::connect(&addr)
        .map_err(|err| format!("failed to connect to redis {}: {err}", addr))?;
    let timeout = Duration::from_millis(oracle.io_timeout_ms);
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|err| format!("failed to set read timeout: {err}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("failed to set write timeout: {err}"))?;
    Ok(stream)
}

fn flushall(stream: &mut TcpStream) -> Result<(), String> {
    let frame = RespFrame::Array(Some(vec![RespFrame::BulkString(Some(
        b"FLUSHALL".to_vec(),
    ))]));
    send_frame(stream, &frame)?;
    let reply = read_resp_frame_from_stream(stream)?;
    match reply {
        RespFrame::SimpleString(ref s) if s == "OK" => Ok(()),
        other => Err(format!("unexpected FLUSHALL reply: {other:?}")),
    }
}

fn send_frame(stream: &mut TcpStream, frame: &RespFrame) -> Result<(), String> {
    let encoded = frame.to_bytes();
    stream
        .write_all(&encoded)
        .map_err(|err| format!("failed to write frame: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("failed to flush frame: {err}"))
}

fn read_resp_frame_from_stream(stream: &mut TcpStream) -> Result<RespFrame, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|err| format!("failed to read response: {err}"))?;
        if n == 0 {
            return Err("redis closed connection before reply was complete".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
        match parse_frame(&buf) {
            Ok(parsed) => return Ok(parsed.frame),
            Err(fr_protocol::RespParseError::Incomplete) => {}
            Err(err) => {
                return Err(format!("invalid RESP from redis: {err}"));
            }
        }
        if buf.len() > 16 * 1024 * 1024 {
            return Err("response exceeded max read bound".to_string());
        }
    }
}

fn case_to_frame(case: &ConformanceCase) -> RespFrame {
    let args = case
        .argv
        .iter()
        .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
        .collect();
    RespFrame::Array(Some(args))
}

fn expected_to_frame(expected: &ExpectedFrame) -> RespFrame {
    match expected {
        ExpectedFrame::Simple { value } => RespFrame::SimpleString(value.clone()),
        ExpectedFrame::Error { value } => RespFrame::Error(value.clone()),
        ExpectedFrame::Integer { value } => RespFrame::Integer(*value),
        ExpectedFrame::Bulk { value } => {
            RespFrame::BulkString(value.as_ref().map(|v| v.as_bytes().to_vec()))
        }
    }
}

fn validate_threat_expectation(
    expected: Option<&ExpectedThreat>,
    new_events: &[EvidenceEvent],
) -> Result<(), String> {
    match expected {
        Some(expected_threat) => {
            if new_events.is_empty() {
                return Err("expected threat event but none recorded".to_string());
            }
            let mut first_mismatch: Option<String> = None;
            for (idx, event) in new_events.iter().enumerate() {
                match validate_single_threat_expectation(expected_threat, event) {
                    Ok(()) => return Ok(()),
                    Err(err) => {
                        if first_mismatch.is_none() {
                            first_mismatch = Some(format!("event[{idx}] {err}"));
                        }
                    }
                }
            }
            Err(format!(
                "no matching threat event found across {} event(s); first mismatch: {}",
                new_events.len(),
                first_mismatch.unwrap_or_else(|| "none".to_string())
            ))
        }
        None => {
            if let Some(event) = new_events.first() {
                return Err(format!(
                    "unexpected threat event recorded (class={}, severity={}, action={}, reason_code={})",
                    threat_class_label(event.threat_class),
                    drift_severity_label(event.severity),
                    decision_action_label(event.decision_action),
                    event.reason_code
                ));
            }
            Ok(())
        }
    }
}

fn validate_single_threat_expectation(
    expected_threat: &ExpectedThreat,
    event: &EvidenceEvent,
) -> Result<(), String> {
    let got_threat_class = threat_class_label(event.threat_class);
    if got_threat_class != expected_threat.threat_class {
        return Err(format!(
            "threat_class mismatch: expected '{}', got '{}'",
            expected_threat.threat_class, got_threat_class
        ));
    }

    let got_severity = drift_severity_label(event.severity);
    if got_severity != expected_threat.severity {
        return Err(format!(
            "severity mismatch: expected '{}', got '{}'",
            expected_threat.severity, got_severity
        ));
    }

    let got_decision = decision_action_label(event.decision_action);
    if got_decision != expected_threat.decision_action {
        return Err(format!(
            "decision_action mismatch: expected '{}', got '{}'",
            expected_threat.decision_action, got_decision
        ));
    }

    if let Some(reason_code) = expected_threat.reason_code.as_deref()
        && event.reason_code != reason_code
    {
        return Err(format!(
            "reason_code mismatch: expected '{reason_code}', got '{}'",
            event.reason_code
        ));
    }

    if let Some(subsystem) = expected_threat.subsystem.as_deref()
        && event.subsystem != subsystem
    {
        return Err(format!(
            "subsystem mismatch: expected '{subsystem}', got '{}'",
            event.subsystem
        ));
    }

    if let Some(action) = expected_threat.action.as_deref()
        && event.action != action
    {
        return Err(format!(
            "action mismatch: expected '{action}', got '{}'",
            event.action
        ));
    }

    Ok(())
}

fn build_case_detail(
    frame_ok: bool,
    threat_err: Option<String>,
    log_err: Option<String>,
) -> Option<String> {
    let mut parts = Vec::new();
    if !frame_ok {
        parts.push("frame mismatch".to_string());
    }
    if let Some(err) = threat_err {
        parts.push(err);
    }
    if let Some(err) = log_err {
        parts.push(format!("structured log emission failed: {err}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn validate_structured_log_emission(
    suite_id: &str,
    fixture_name: &str,
    case_name: &str,
    verification_path: VerificationPath,
    now_ms: u64,
    new_events: &[EvidenceEvent],
) -> Result<(), String> {
    let packet_id = packet_family_for_fixture(fixture_name);
    for event in new_events {
        let _ = StructuredLogEvent::from_runtime_evidence(
            event,
            RuntimeEvidenceContext {
                suite_id,
                test_or_scenario_id: case_name,
                packet_id,
                verification_path,
                seed: now_ms,
                duration_ms: 0,
                outcome: LogOutcome::Pass,
                fixture_id: Some(fixture_name),
                env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json"),
            },
        )
        .map_err(|err| format!("runtime evidence conversion error: {err}"))?;
    }
    Ok(())
}

fn packet_family_for_fixture(fixture_name: &str) -> &'static str {
    match fixture_name {
        "protocol_negative.json" => "FR-P2C-002",
        "persist_replay.json" => "FR-P2C-005",
        _ => "FR-P2C-003",
    }
}

fn threat_class_label(threat_class: ThreatClass) -> &'static str {
    match threat_class {
        ThreatClass::ParserAbuse => "parser_abuse",
        ThreatClass::MetadataAmbiguity => "metadata_ambiguity",
        ThreatClass::VersionSkew => "version_skew",
        ThreatClass::ResourceExhaustion => "resource_exhaustion",
        ThreatClass::PersistenceTampering => "persistence_tampering",
        ThreatClass::ReplicationOrderAttack => "replication_order_attack",
        ThreatClass::AuthPolicyConfusion => "auth_policy_confusion",
        ThreatClass::ConfigDowngradeAbuse => "config_downgrade_abuse",
    }
}

fn drift_severity_label(severity: DriftSeverity) -> &'static str {
    match severity {
        DriftSeverity::S0 => "s0",
        DriftSeverity::S1 => "s1",
        DriftSeverity::S2 => "s2",
        DriftSeverity::S3 => "s3",
    }
}

fn decision_action_label(decision_action: DecisionAction) -> &'static str {
    match decision_action {
        DecisionAction::FailClosed => "fail_closed",
        DecisionAction::BoundedDefense => "bounded_defense",
        DecisionAction::RejectNonAllowlisted => "reject_non_allowlisted",
    }
}

#[cfg(test)]
mod tests {
    use fr_config::{DecisionAction, DriftSeverity, Mode, ThreatClass};

    use super::{
        EvidenceEvent, ExpectedThreat, HarnessConfig, LiveOracleConfig, run_fixture,
        run_live_redis_diff, run_live_redis_protocol_diff, run_protocol_fixture,
        run_replay_fixture, run_smoke, validate_structured_log_emission,
        validate_threat_expectation,
    };
    use crate::log_contract::VerificationPath;

    #[test]
    fn smoke_harness_finds_oracle_and_fixtures() {
        let cfg = HarnessConfig::default_paths();
        let report = run_smoke(&cfg);
        assert!(report.oracle_present, "oracle repo should be present");
        assert!(report.fixture_count >= 1, "expected at least one fixture");
        assert!(report.strict_mode);
    }

    #[test]
    fn conformance_fixture_core_passes() {
        let cfg = HarnessConfig::default_paths();
        let report = run_fixture(&cfg, "core_strings.json").expect("fixture run");
        assert_eq!(
            report.total, report.passed,
            "all conformance cases should pass; mismatches: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
    }

    #[test]
    fn conformance_errors_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report = run_fixture(&cfg, "core_errors.json").expect("errors fixture run");
        assert_eq!(
            report.total, report.passed,
            "mismatch details: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
    }

    #[test]
    fn conformance_protocol_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report =
            run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture run");
        assert_eq!(report.total, report.passed);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn conformance_replay_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture run");
        assert_eq!(report.total, report.passed);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn threat_expectation_rejects_unexpected_event() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:0".to_string(),
            ts_ms: 0,
            packet_id: 1,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };

        let err = validate_threat_expectation(None, &[event]).expect_err("must fail");
        assert!(err.contains("unexpected threat event"));
    }

    #[test]
    fn threat_expectation_accepts_matching_event() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:0".to_string(),
            ts_ms: 0,
            packet_id: 1,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let expected = ExpectedThreat {
            threat_class: "parser_abuse".to_string(),
            severity: "s0".to_string(),
            decision_action: "fail_closed".to_string(),
            reason_code: Some("protocol_parse_failure".to_string()),
            subsystem: Some("protocol".to_string()),
            action: Some("parse_failure".to_string()),
        };

        validate_threat_expectation(Some(&expected), &[event]).expect("must match");
    }

    #[test]
    fn threat_expectation_rejects_severity_mismatch() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:0".to_string(),
            ts_ms: 0,
            packet_id: 1,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let expected = ExpectedThreat {
            threat_class: "parser_abuse".to_string(),
            severity: "s1".to_string(),
            decision_action: "fail_closed".to_string(),
            reason_code: Some("protocol_parse_failure".to_string()),
            subsystem: Some("protocol".to_string()),
            action: Some("parse_failure".to_string()),
        };

        let err = validate_threat_expectation(Some(&expected), &[event]).expect_err("must fail");
        assert!(err.contains("severity mismatch"));
    }

    #[test]
    fn threat_expectation_matches_any_event_in_batch() {
        let mismatched = EvidenceEvent {
            ts_utc: "unix_ms:0".to_string(),
            ts_ms: 0,
            packet_id: 1,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ResourceExhaustion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "compatibility_gate",
            action: "fail_closed_array_len",
            reason_code: "compat_array_len_exceeded",
            reason: "array length exceeded".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let matching = EvidenceEvent {
            ts_utc: "unix_ms:1".to_string(),
            ts_ms: 1,
            packet_id: 2,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let expected = ExpectedThreat {
            threat_class: "parser_abuse".to_string(),
            severity: "s0".to_string(),
            decision_action: "fail_closed".to_string(),
            reason_code: Some("protocol_parse_failure".to_string()),
            subsystem: Some("protocol".to_string()),
            action: Some("parse_failure".to_string()),
        };

        validate_threat_expectation(Some(&expected), &[mismatched, matching]).expect("must match");
    }

    #[test]
    fn threat_expectation_allows_omitted_optional_fields() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:1".to_string(),
            ts_ms: 1,
            packet_id: 2,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let expected = ExpectedThreat {
            threat_class: "parser_abuse".to_string(),
            severity: "s0".to_string(),
            decision_action: "fail_closed".to_string(),
            reason_code: None,
            subsystem: None,
            action: None,
        };

        validate_threat_expectation(Some(&expected), &[event]).expect("must match");
    }

    #[test]
    fn structured_log_enforcement_accepts_valid_runtime_event() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:1".to_string(),
            ts_ms: 1,
            packet_id: 2,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec!["TEST_LOG_SCHEMA_V1.md".to_string()],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            "protocol_negative",
            "protocol_negative.json",
            "invalid_bulk_len",
            VerificationPath::E2e,
            7,
            &[event],
        )
        .expect("conversion should pass");
    }

    #[test]
    fn structured_log_enforcement_rejects_invalid_runtime_event() {
        let event = EvidenceEvent {
            ts_utc: "unix_ms:1".to_string(),
            ts_ms: 1,
            packet_id: 2,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ParserAbuse,
            decision_action: DecisionAction::FailClosed,
            subsystem: "protocol",
            action: "parse_failure",
            reason_code: "protocol_parse_failure",
            reason: "invalid bulk length".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec![],
            confidence: Some(1.0),
        };
        let err = validate_structured_log_emission(
            "protocol_negative",
            "protocol_negative.json",
            "invalid_bulk_len",
            VerificationPath::E2e,
            7,
            &[event],
        )
        .expect_err("empty artifact_refs should fail conversion");
        assert!(err.contains("artifact_refs"));
    }

    #[test]
    #[ignore = "requires running redis-server on localhost:6379"]
    fn live_redis_core_errors_matches_runtime() {
        let cfg = HarnessConfig::default_paths();
        let oracle = LiveOracleConfig::default();
        let report = run_live_redis_diff(&cfg, "core_errors.json", &oracle).expect("live diff");
        assert_eq!(
            report.total, report.passed,
            "mismatches: {:?}",
            report.failed
        );
    }

    #[test]
    #[ignore = "requires running redis-server on localhost:6379"]
    fn live_redis_protocol_negative_matches_runtime() {
        let cfg = HarnessConfig::default_paths();
        let oracle = LiveOracleConfig::default();
        let report = run_live_redis_protocol_diff(&cfg, "protocol_negative.json", &oracle)
            .expect("live protocol diff");
        assert_eq!(
            report.total, report.passed,
            "mismatches: {:?}",
            report.failed
        );
    }
}
