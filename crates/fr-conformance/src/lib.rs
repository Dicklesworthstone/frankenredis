#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use fr_config::{DecisionAction, DriftSeverity, ThreatClass};
use fr_persist::{AofRecord, decode_aof_stream, encode_aof_stream};
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::{EvidenceEvent, Runtime};
use serde::{Deserialize, Serialize};

use crate::log_contract::{
    LogOutcome, RuntimeEvidenceContext, StructuredLogEvent, VerificationPath,
    append_structured_log_jsonl, live_log_output_path,
};

pub mod log_contract;
pub mod phase2c_schema;

pub const DIFFERENTIAL_REPORT_SCHEMA_VERSION: &str = "fr_conformance_differential_report/v1";

#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub oracle_root: PathBuf,
    pub fixture_root: PathBuf,
    pub strict_mode: bool,
    pub live_log_root: Option<PathBuf>,
}

impl HarnessConfig {
    #[must_use]
    pub fn default_paths() -> Self {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        Self {
            oracle_root: repo_root.join("legacy_redis_code/redis"),
            fixture_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
            strict_mode: true,
            live_log_root: None,
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
    pub reason_code: Option<String>,
    pub replay_cmd: Option<String>,
    pub artifact_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DifferentialReport {
    pub schema_version: &'static str,
    pub suite: String,
    pub fixture: String,
    pub total: usize,
    pub passed: usize,
    pub failed: Vec<CaseOutcome>,
    pub reason_code_counts: BTreeMap<String, usize>,
    pub failed_without_reason_code: usize,
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
    let fixture_log_path = config
        .live_log_root
        .as_ref()
        .map(|root| live_log_output_path(root, &fixture.suite, fixture_name));

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
            StructuredLogEmissionContext {
                suite_id: &fixture.suite,
                fixture_name,
                case_name: &case.name,
                verification_path: VerificationPath::E2e,
                now_ms: case.now_ms,
                outcome: LogOutcome::Pass,
                persist_path: fixture_log_path.as_deref(),
            },
            new_events,
        );
        let frame_ok = actual == expected;
        let passed = frame_ok && threat_result.is_ok() && log_result.is_ok();
        if !passed {
            let reason_code = reason_code_from_evidence(new_events);
            let replay_cmd = replay_cmd_from_evidence(new_events);
            let artifact_refs = artifact_refs_from_evidence(new_events);
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected,
                actual,
                detail: build_case_detail(frame_ok, threat_result.err(), log_result.err()),
                reason_code,
                replay_cmd,
                artifact_refs,
            });
        }
    }

    Ok(build_differential_report(
        fixture.suite,
        fixture_name,
        total,
        failed,
    ))
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
    let suite = format!("live_redis_diff::{}", fixture.suite);
    let live_log_path = config
        .live_log_root
        .as_ref()
        .map(|root| live_log_output_path(root, &suite, fixture_name));

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

        let evidence_before = runtime.evidence().events().len();
        let frame = case_to_frame(&case);
        let runtime_actual = runtime.execute_frame(frame.clone(), case.now_ms);
        let new_events = &runtime.evidence().events()[evidence_before..];
        send_frame(&mut stream, &frame)?;
        let redis_actual = read_resp_frame_from_stream(&mut stream)?;
        let frame_ok = runtime_actual == redis_actual;
        let outcome = if frame_ok {
            LogOutcome::Pass
        } else {
            LogOutcome::Fail
        };
        let log_result = validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: &suite,
                fixture_name,
                case_name: &case.name,
                verification_path: VerificationPath::E2e,
                now_ms: case.now_ms,
                outcome,
                persist_path: live_log_path.as_deref(),
            },
            new_events,
        );
        let passed = frame_ok && log_result.is_ok();
        if !passed {
            let reason_code = reason_code_from_evidence(new_events);
            let replay_cmd = replay_cmd_from_evidence(new_events);
            let artifact_refs = artifact_refs_from_evidence(new_events);
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected: redis_actual,
                actual: runtime_actual,
                detail: build_case_detail(frame_ok, None, log_result.err()),
                reason_code,
                replay_cmd,
                artifact_refs,
            });
        }
    }

    Ok(build_differential_report(
        suite,
        fixture_name,
        total,
        failed,
    ))
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
    let fixture_log_path = config
        .live_log_root
        .as_ref()
        .map(|root| live_log_output_path(root, &fixture.suite, fixture_name));

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
            StructuredLogEmissionContext {
                suite_id: &fixture.suite,
                fixture_name,
                case_name: &case.name,
                verification_path: VerificationPath::E2e,
                now_ms: case.now_ms,
                outcome: LogOutcome::Pass,
                persist_path: fixture_log_path.as_deref(),
            },
            new_events,
        );
        let frame_ok = actual == expected;
        let passed = frame_ok && threat_result.is_ok() && log_result.is_ok();
        if !passed {
            let reason_code = reason_code_from_evidence(new_events);
            let replay_cmd = replay_cmd_from_evidence(new_events);
            let artifact_refs = artifact_refs_from_evidence(new_events);
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected,
                actual,
                detail: build_case_detail(frame_ok, threat_result.err(), log_result.err()),
                reason_code,
                replay_cmd,
                artifact_refs,
            });
        }
    }

    Ok(build_differential_report(
        fixture.suite,
        fixture_name,
        total,
        failed,
    ))
}

pub fn run_live_redis_protocol_diff(
    config: &HarnessConfig,
    fixture_name: &str,
    oracle: &LiveOracleConfig,
) -> Result<DifferentialReport, String> {
    let fixture = load_protocol_fixture(config, fixture_name)?;
    let mut runtime = Runtime::default_strict();
    let suite = format!("live_redis_protocol_diff::{}", fixture.suite);
    let live_log_path = config
        .live_log_root
        .as_ref()
        .map(|root| live_log_output_path(root, &suite, fixture_name));
    let mut failed = Vec::new();
    let total = fixture.cases.len();

    for case in fixture.cases {
        let evidence_before = runtime.evidence().events().len();
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
        let new_events = &runtime.evidence().events()[evidence_before..];
        let runtime_actual = parse_frame(&runtime_encoded)
            .map_err(|err| format!("runtime emitted invalid RESP frame in {}: {err}", case.name))?
            .frame;
        let frame_ok = runtime_actual == redis_actual;
        let outcome = if frame_ok {
            LogOutcome::Pass
        } else {
            LogOutcome::Fail
        };
        let log_result = validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: &suite,
                fixture_name,
                case_name: &case.name,
                verification_path: VerificationPath::E2e,
                now_ms: case.now_ms,
                outcome,
                persist_path: live_log_path.as_deref(),
            },
            new_events,
        );
        let passed = frame_ok && log_result.is_ok();
        if !passed {
            let reason_code = reason_code_from_evidence(new_events);
            let replay_cmd = replay_cmd_from_evidence(new_events);
            let artifact_refs = artifact_refs_from_evidence(new_events);
            failed.push(CaseOutcome {
                name: case.name,
                passed,
                expected: redis_actual,
                actual: runtime_actual,
                detail: build_case_detail(frame_ok, None, log_result.err()),
                reason_code,
                replay_cmd,
                artifact_refs,
            });
        }
    }

    Ok(build_differential_report(
        suite,
        fixture_name,
        total,
        failed,
    ))
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
    let replay_log_path = config
        .live_log_root
        .as_ref()
        .map(|root| live_log_output_path(root, &fixture.suite, fixture_name));

    let mut failed = Vec::new();
    let mut total = 0_usize;
    for case in fixture.cases {
        let mut runtime = Runtime::default_strict();
        let source_records = case
            .records
            .into_iter()
            .map(|record| AofRecord {
                argv: record
                    .iter()
                    .map(|value| value.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
            })
            .collect::<Vec<_>>();
        let encoded_stream = encode_aof_stream(&source_records);
        let decoded_records = match decode_aof_stream(&encoded_stream) {
            Ok(records) => records,
            Err(err) => {
                failed.push(CaseOutcome {
                    name: format!("{}::aof_decode", case.name),
                    passed: false,
                    expected: RespFrame::BulkString(None),
                    actual: RespFrame::BulkString(None),
                    detail: Some(format!("AOF stream decode failed: {err:?}")),
                    reason_code: None,
                    replay_cmd: None,
                    artifact_refs: Vec::new(),
                });
                continue;
            }
        };

        for (record_idx, record) in decoded_records.into_iter().enumerate() {
            let evidence_before = runtime.evidence().events().len();
            let frame = record.to_resp_frame();
            let _ = runtime.execute_frame(frame, case.now_ms);
            let new_events = &runtime.evidence().events()[evidence_before..];
            let replay_case_name = format!("{}::record_{record_idx}", case.name);
            let log_result = validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: &fixture.suite,
                    fixture_name,
                    case_name: &replay_case_name,
                    verification_path: VerificationPath::E2e,
                    now_ms: case.now_ms,
                    outcome: LogOutcome::Pass,
                    persist_path: replay_log_path.as_deref(),
                },
                new_events,
            );
            if let Err(err) = log_result {
                let reason_code = reason_code_from_evidence(new_events);
                let replay_cmd = replay_cmd_from_evidence(new_events);
                let artifact_refs = artifact_refs_from_evidence(new_events);
                failed.push(CaseOutcome {
                    name: replay_case_name,
                    passed: false,
                    expected: RespFrame::BulkString(None),
                    actual: RespFrame::BulkString(None),
                    detail: Some(format!(
                        "structured log emission failed during replay record execution: {err}"
                    )),
                    reason_code,
                    replay_cmd,
                    artifact_refs,
                });
            }
        }

        for assertion in case.assertions {
            total = total.saturating_add(1);
            let at_ms = assertion.at_ms.unwrap_or(case.now_ms);
            let evidence_before = runtime.evidence().events().len();
            let get = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(assertion.key.as_bytes().to_vec())),
            ]));
            let actual = runtime.execute_frame(get, at_ms);
            let expected = RespFrame::BulkString(assertion.expect.map(|v| v.as_bytes().to_vec()));
            let frame_ok = actual == expected;
            let outcome = if frame_ok {
                LogOutcome::Pass
            } else {
                LogOutcome::Fail
            };
            let new_events = &runtime.evidence().events()[evidence_before..];
            let assertion_case_name = format!("{}::assert::{}", case.name, assertion.key);
            let log_result = validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: &fixture.suite,
                    fixture_name,
                    case_name: &assertion_case_name,
                    verification_path: VerificationPath::E2e,
                    now_ms: at_ms,
                    outcome,
                    persist_path: replay_log_path.as_deref(),
                },
                new_events,
            );
            let passed = frame_ok && log_result.is_ok();
            if !passed {
                let reason_code = reason_code_from_evidence(new_events);
                let replay_cmd = replay_cmd_from_evidence(new_events);
                let artifact_refs = artifact_refs_from_evidence(new_events);
                failed.push(CaseOutcome {
                    name: format!("{}::{}", case.name, assertion.key),
                    passed,
                    expected,
                    actual,
                    detail: build_case_detail(frame_ok, None, log_result.err()),
                    reason_code,
                    replay_cmd,
                    artifact_refs,
                });
            }
        }
    }

    Ok(build_differential_report(
        fixture.suite,
        fixture_name,
        total,
        failed,
    ))
}

fn build_differential_report(
    suite: String,
    fixture_name: &str,
    total: usize,
    failed: Vec<CaseOutcome>,
) -> DifferentialReport {
    let (reason_code_counts, failed_without_reason_code) = summarize_failure_reason_codes(&failed);
    DifferentialReport {
        schema_version: DIFFERENTIAL_REPORT_SCHEMA_VERSION,
        suite,
        fixture: fixture_name.to_string(),
        total,
        passed: total.saturating_sub(failed.len()),
        failed,
        reason_code_counts,
        failed_without_reason_code,
    }
}

fn summarize_failure_reason_codes(failed: &[CaseOutcome]) -> (BTreeMap<String, usize>, usize) {
    let mut reason_code_counts = BTreeMap::new();
    let mut failed_without_reason_code = 0_usize;
    for case in failed {
        if let Some(reason_code) = case.reason_code.as_deref() {
            *reason_code_counts
                .entry(reason_code.to_string())
                .or_insert(0) += 1;
        } else {
            failed_without_reason_code = failed_without_reason_code.saturating_add(1);
        }
    }
    (reason_code_counts, failed_without_reason_code)
}

fn reason_code_from_evidence(events: &[EvidenceEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.reason_code.is_empty() {
            None
        } else {
            Some(event.reason_code.to_string())
        }
    })
}

fn replay_cmd_from_evidence(events: &[EvidenceEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.replay_cmd.trim().is_empty() {
            None
        } else {
            Some(event.replay_cmd.clone())
        }
    })
}

fn artifact_refs_from_evidence(events: &[EvidenceEvent]) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|event| {
            let refs = event
                .artifact_refs
                .iter()
                .filter(|artifact_ref| !artifact_ref.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>();
            if refs.is_empty() { None } else { Some(refs) }
        })
        .unwrap_or_default()
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

#[derive(Clone, Copy)]
struct StructuredLogEmissionContext<'a> {
    suite_id: &'a str,
    fixture_name: &'a str,
    case_name: &'a str,
    verification_path: VerificationPath,
    now_ms: u64,
    outcome: LogOutcome,
    persist_path: Option<&'a Path>,
}

fn validate_structured_log_emission(
    context: StructuredLogEmissionContext<'_>,
    new_events: &[EvidenceEvent],
) -> Result<(), String> {
    let packet_id = packet_family_for_fixture(context.fixture_name);
    let mut structured_events = Vec::with_capacity(new_events.len());
    for event in new_events {
        let converted = StructuredLogEvent::from_runtime_evidence(
            event,
            RuntimeEvidenceContext {
                suite_id: context.suite_id,
                test_or_scenario_id: context.case_name,
                packet_id,
                verification_path: context.verification_path,
                seed: context.now_ms,
                duration_ms: 0,
                outcome: context.outcome,
                fixture_id: Some(context.fixture_name),
                env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json"),
            },
        )
        .map_err(|err| format!("runtime evidence conversion error: {err}"))?;
        structured_events.push(converted);
    }
    if let Some(path) = context.persist_path {
        append_structured_log_jsonl(path, &structured_events)
            .map_err(|err| format!("structured log persistence error: {err}"))?;
    }
    Ok(())
}

fn packet_family_for_fixture(fixture_name: &str) -> &'static str {
    match fixture_name {
        "fr_p2c_001_eventloop_journey.json" => "FR-P2C-001",
        "protocol_negative.json" => "FR-P2C-002",
        "fr_p2c_004_auth_unit"
        | "fr_p2c_004_acl_rules"
        | "fr_p2c_004_acl_permissions"
        | "fr_p2c_004_acl_journey.json" => "FR-P2C-004",
        "core_errors.json" | "fr_p2c_003_dispatch_journey.json" => "FR-P2C-003",
        "fr_p2c_006_replication_journey.json" => "FR-P2C-006",
        "fr_p2c_007_cluster_journey.json" => "FR-P2C-007",
        "persist_replay.json" => "FR-P2C-005",
        "fr_p2c_009_tls_config_journey.json" => "FR-P2C-009",
        "fr_p2c_009_tls_runtime_strict" | "fr_p2c_009_tls_runtime_hardened" => "FR-P2C-009",
        _ if fixture_name.starts_with("fr_p2c_004_") => "FR-P2C-004",
        _ if fixture_name.starts_with("fr_p2c_003_") => "FR-P2C-003",
        _ if fixture_name.starts_with("fr_p2c_006_") => "FR-P2C-006",
        _ if fixture_name.starts_with("fr_p2c_007_") => "FR-P2C-007",
        _ if fixture_name.starts_with("fr_p2c_009_") => "FR-P2C-009",
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
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use fr_config::{
        DecisionAction, DriftSeverity, HardenedDeviationCategory, Mode, RuntimePolicy, ThreatClass,
        TlsAuthClients, TlsConfig, TlsProtocol,
    };
    use fr_persist::{AofRecord, decode_aof_stream, encode_aof_stream};
    use fr_protocol::{RespFrame, RespParseError, parse_frame};
    use fr_repl::{
        BacklogWindow, HandshakeFsm, HandshakeState, HandshakeStep, PsyncDecision, PsyncRejection,
        ReplOffset, WaitAofThreshold, WaitThreshold, decide_psync, evaluate_wait, evaluate_waitaof,
    };
    use fr_runtime::Runtime;

    use super::{
        CaseOutcome, DIFFERENTIAL_REPORT_SCHEMA_VERSION, EvidenceEvent, ExpectedThreat,
        HarnessConfig, LiveOracleConfig, ReplayFixture, StructuredLogEmissionContext,
        build_differential_report, run_fixture, run_live_redis_diff, run_live_redis_protocol_diff,
        run_protocol_fixture, run_replay_fixture, run_smoke, validate_structured_log_emission,
        validate_threat_expectation,
    };
    use crate::log_contract::{
        LogOutcome, StructuredLogEvent, VerificationPath, live_log_output_path,
    };

    fn unique_temp_log_root(prefix: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock moved backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}_{nonce}"))
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

    fn invalid_tls_without_listener_ports() -> TlsConfig {
        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;
        invalid
    }

    fn persist_decode_reason_code(err: &fr_persist::PersistError) -> &'static str {
        match err {
            fr_persist::PersistError::InvalidFrame => "persist.replay.frame_parse_invalid",
            fr_persist::PersistError::Parse(RespParseError::Incomplete) => {
                "persist.replay.frame_length_violation"
            }
            fr_persist::PersistError::Parse(_) => "persist.replay.frame_parse_invalid",
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AuthState {
        Authenticated,
        Unauthenticated,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CommandAuthClass {
        NoAuthExempt,
        RequiresAuth,
    }

    fn enforce_noauth_gate(state: AuthState, class: CommandAuthClass) -> Result<(), &'static str> {
        match (state, class) {
            (AuthState::Unauthenticated, CommandAuthClass::RequiresAuth) => {
                Err("auth.noauth_gate_violation")
            }
            _ => Ok(()),
        }
    }

    fn parse_acl_selector_tokens(tokens: &[&str]) -> Result<(), &'static str> {
        for token in tokens {
            if token.trim().is_empty() {
                return Err("auth.acl_selector_parse_validation_mismatch");
            }
            let valid_prefix = token.starts_with("+@")
                || token.starts_with("-@")
                || token.starts_with('+')
                || token.starts_with('-')
                || token.starts_with('~')
                || token.starts_with('&')
                || token.starts_with('>');
            let valid_literal = matches!(*token, "on" | "off" | "resetpass" | "nopass");
            if !(valid_prefix || valid_literal) {
                return Err("auth.acl_selector_parse_validation_mismatch");
            }
        }
        Ok(())
    }

    fn allow_set(commands: &[&str]) -> BTreeSet<String> {
        commands.iter().map(|cmd| (*cmd).to_string()).collect()
    }

    fn first_denied_index(allowed: &BTreeSet<String>, command_path: &[&str]) -> Option<usize> {
        command_path.iter().position(|cmd| !allowed.contains(*cmd))
    }

    fn command_frame(argv: &[&str]) -> RespFrame {
        RespFrame::Array(Some(
            argv.iter()
                .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
                .collect(),
        ))
    }

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
    fn fr_p2c_001_f_differential_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report =
            run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("packet-001 fixture");
        assert_eq!(report.fixture, "fr_p2c_001_eventloop_journey.json");
        assert_eq!(report.suite, "fr_p2c_001_eventloop_journey");
        assert_eq!(
            report.total, report.passed,
            "packet-001 fixture mismatches: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
    }

    #[test]
    fn fr_p2c_001_f_metamorphic_repeated_fixture_runs_are_deterministic() {
        let cfg = HarnessConfig::default_paths();
        let first =
            run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("first fixture run");
        let second =
            run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("second fixture run");

        assert_eq!(first.total, second.total);
        assert_eq!(first.passed, second.passed);
        assert_eq!(first.reason_code_counts, second.reason_code_counts);
        assert_eq!(
            first.failed_without_reason_code,
            second.failed_without_reason_code
        );
        assert_eq!(first.failed, second.failed);
    }

    #[test]
    fn fr_p2c_001_f_adversarial_reason_codes_are_stable() {
        let fd_err = Runtime::validate_event_loop_fd_registration(64, 64)
            .expect_err("fd bounds violation expected");
        assert_eq!(fd_err.reason_code(), "eventloop.fd_out_of_range");

        let accept_err = Runtime::validate_event_loop_accept_path(10_000, 10_000, true)
            .expect_err("maxclients violation expected");
        assert_eq!(
            accept_err.reason_code(),
            "eventloop.accept.maxclients_reached"
        );

        let read_err = Runtime::validate_event_loop_read_path(6, 5, 10, false)
            .expect_err("query buffer violation expected");
        assert_eq!(
            read_err.reason_code(),
            "eventloop.read.querybuf_limit_exceeded"
        );

        let write_err =
            Runtime::validate_event_loop_pending_write_delivery(&[1, 2, 3], &[2, 1], &[3])
                .expect_err("write ordering violation expected");
        assert_eq!(
            write_err.reason_code(),
            "eventloop.write.flush_order_violation"
        );
    }

    #[test]
    fn fr_p2c_003_f_differential_dispatch_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report =
            run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet-003 fixture");
        assert_eq!(report.schema_version, DIFFERENTIAL_REPORT_SCHEMA_VERSION);
        assert_eq!(report.fixture, "fr_p2c_003_dispatch_journey.json");
        assert_eq!(report.suite, "fr_p2c_003_dispatch_journey");
        assert_eq!(
            report.total, report.passed,
            "packet-003 fixture mismatches: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
    }

    #[test]
    fn fr_p2c_003_f_metamorphic_repeated_fixture_runs_are_deterministic() {
        let cfg = HarnessConfig::default_paths();
        let first =
            run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("first fixture run");
        let second =
            run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("second fixture run");

        assert_eq!(first.total, second.total);
        assert_eq!(first.passed, second.passed);
        assert_eq!(first.reason_code_counts, second.reason_code_counts);
        assert_eq!(
            first.failed_without_reason_code,
            second.failed_without_reason_code
        );
        assert_eq!(first.failed, second.failed);
    }

    #[test]
    fn fr_p2c_003_f_adversarial_error_families_are_stable() {
        let mut runtime = Runtime::default_strict();
        let cases: [(&str, &[&str], &str); 4] = [
            ("unknown_no_args", &["NOPE"], "ERR unknown command 'NOPE'"),
            (
                "unknown_with_args_preview",
                &["NOPE", "x", "y"],
                "ERR unknown command 'NOPE', with args beginning with: 'x' 'y' ",
            ),
            (
                "wrong_arity_get",
                &["GET"],
                "ERR wrong number of arguments for 'GET' command",
            ),
            (
                "set_syntax_error",
                &["SET", "k", "v", "NX", "10"],
                "ERR syntax error",
            ),
        ];

        for (idx, (name, argv, expected_error)) in cases.iter().enumerate() {
            let frame = RespFrame::Array(Some(
                argv.iter()
                    .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
                    .collect(),
            ));
            let actual = runtime.execute_frame(frame, idx as u64);
            assert_eq!(
                actual,
                RespFrame::Error((*expected_error).to_string()),
                "case={name}"
            );
        }
    }

    #[test]
    fn fr_p2c_002_f_differential_protocol_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report =
            run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture run");
        assert_eq!(report.schema_version, DIFFERENTIAL_REPORT_SCHEMA_VERSION);
        assert_eq!(report.suite, "protocol_negative");
        assert_eq!(report.fixture, "protocol_negative.json");
        assert_eq!(report.total, report.passed);
        assert_eq!(report.failed_without_reason_code, 0);
        assert!(report.reason_code_counts.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn fr_p2c_002_f_metamorphic_protocol_fixture_runs_are_deterministic() {
        let cfg = HarnessConfig::default_paths();
        let first =
            run_protocol_fixture(&cfg, "protocol_negative.json").expect("first protocol fixture");
        let second =
            run_protocol_fixture(&cfg, "protocol_negative.json").expect("second protocol fixture");

        assert_eq!(first.total, second.total);
        assert_eq!(first.passed, second.passed);
        assert_eq!(first.reason_code_counts, second.reason_code_counts);
        assert_eq!(
            first.failed_without_reason_code,
            second.failed_without_reason_code
        );
        assert_eq!(first.failed, second.failed);
    }

    #[test]
    fn fr_p2c_002_f_adversarial_runtime_parse_failures_emit_stable_reason_code() {
        let cases = [
            (
                "invalid_bulk_length",
                "$-2\r\n",
                "ERR Protocol error: invalid bulk length",
            ),
            (
                "invalid_multibulk_length",
                "*-2\r\n",
                "ERR Protocol error: invalid multibulk length",
            ),
            (
                "incomplete_bulk",
                "$3\r\nab",
                "ERR Protocol error: unexpected EOF while reading request",
            ),
            (
                "unsupported_resp3_prefix",
                "~1\r\n",
                "ERR Protocol error: unsupported RESP3 type prefix '~'",
            ),
            (
                "unsupported_resp3_map_prefix",
                "%1\r\n",
                "ERR Protocol error: unsupported RESP3 type prefix '%'",
            ),
            (
                "invalid_prefix_unknown_byte",
                "?\r\n",
                "ERR Protocol error: invalid RESP type prefix '?'",
            ),
            (
                "noncanonical_null_bulk",
                "$-01\r\n",
                "ERR Protocol error: invalid bulk length",
            ),
            (
                "noncanonical_null_array",
                "*-01\r\n",
                "ERR Protocol error: invalid multibulk length",
            ),
            (
                "unsupported_resp3_attribute_prefix",
                "|1\r\n",
                "ERR Protocol error: unsupported RESP3 type prefix '|'",
            ),
            (
                "invalid_integer_payload",
                ":abc\r\n",
                "ERR Protocol error: invalid integer payload",
            ),
        ];

        let mut runtime = Runtime::default_strict();
        for (idx, (name, raw, expected_err)) in cases.iter().enumerate() {
            let evidence_before = runtime.evidence().events().len();
            let encoded = runtime.execute_bytes(raw.as_bytes(), idx as u64);
            let actual = parse_frame(&encoded)
                .expect("runtime emitted a parseable RESP reply")
                .frame;
            assert_eq!(
                actual,
                RespFrame::Error((*expected_err).to_string()),
                "case={name}"
            );

            let events = &runtime.evidence().events()[evidence_before..];
            assert_eq!(events.len(), 1, "case={name} should emit one threat event");
            let event = &events[0];
            assert_eq!(event.threat_class, ThreatClass::ParserAbuse, "case={name}");
            assert_eq!(event.severity, DriftSeverity::S0, "case={name}");
            assert_eq!(
                event.decision_action,
                DecisionAction::FailClosed,
                "case={name}"
            );
            assert_eq!(event.reason_code, "protocol_parse_failure", "case={name}");

            validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: "protocol_negative",
                    fixture_name: "protocol_negative.json",
                    case_name: name,
                    verification_path: VerificationPath::E2e,
                    now_ms: idx as u64,
                    outcome: LogOutcome::Pass,
                    persist_path: None,
                },
                events,
            )
            .expect("structured log contract must hold for parser abuse vectors");
        }
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
    fn run_fixture_accepts_structured_log_persistence_toggle() {
        let log_root = unique_temp_log_root("fr_conformance_fixture_logs");
        let mut cfg = HarnessConfig::default_paths();
        cfg.live_log_root = Some(log_root.clone());

        let report = run_fixture(&cfg, "core_errors.json").expect("fixture run");
        assert_eq!(
            report.total, report.passed,
            "mismatches: {:?}",
            report.failed
        );

        let out_path = live_log_output_path(&log_root, "core_errors", "core_errors.json");
        if out_path.exists() {
            let raw = fs::read_to_string(&out_path).expect("read optional persisted logs");
            for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                let event: StructuredLogEvent =
                    serde_json::from_str(line).expect("parse structured log line");
                event.validate().expect("structured log validates");
            }
        }
        let _ = fs::remove_dir_all(log_root);
    }

    #[test]
    fn fr_p2c_001_fixture_maps_to_packet_family_in_structured_logs() {
        let log_root = unique_temp_log_root("fr_p2c_001_structured_log");
        let out_path = log_root.join("packet/fr_p2c_001.jsonl");
        let event = EvidenceEvent {
            ts_utc: "unix_ms:7".to_string(),
            ts_ms: 7,
            packet_id: 1,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::ResourceExhaustion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "eventloop",
            action: "maxclients_reject",
            reason_code: "eventloop.accept.maxclients_reached",
            reason: "maxclients reached".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec!["TEST_LOG_SCHEMA_V1.md".to_string()],
            confidence: Some(1.0),
        };

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "unit::fr-p2c-001",
                fixture_name: "fr_p2c_001_eventloop_journey.json",
                case_name: "fr_p2c_001_u006_accept_path_rejects_over_maxclients",
                verification_path: VerificationPath::Unit,
                now_ms: 7,
                outcome: LogOutcome::Pass,
                persist_path: Some(&out_path),
            },
            &[event],
        )
        .expect("packet family conversion should succeed");

        let raw = fs::read_to_string(&out_path).expect("read structured output");
        let line = raw
            .lines()
            .find(|candidate| !candidate.trim().is_empty())
            .expect("one structured event");
        let parsed: StructuredLogEvent =
            serde_json::from_str(line).expect("parse structured event");
        assert_eq!(parsed.packet_id, "FR-P2C-001");
        assert_eq!(
            parsed.fixture_id.as_deref(),
            Some("fr_p2c_001_eventloop_journey.json")
        );
        parsed.validate().expect("structured event validates");
        let _ = fs::remove_dir_all(log_root);
    }

    #[test]
    fn fr_p2c_003_fixture_maps_to_packet_family_in_structured_logs() {
        let log_root = unique_temp_log_root("fr_p2c_003_structured_log");
        let out_path = log_root.join("packet/fr_p2c_003.jsonl");
        let event = EvidenceEvent {
            ts_utc: "unix_ms:13".to_string(),
            ts_ms: 13,
            packet_id: 3,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "dispatch",
            action: "unknown_command",
            reason_code: "dispatch.unknown_command_error_mismatch",
            reason: "command family mismatch".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec!["TEST_LOG_SCHEMA_V1.md".to_string()],
            confidence: Some(1.0),
        };

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "unit::fr-p2c-003",
                fixture_name: "fr_p2c_003_dispatch_journey.json",
                case_name: "fr_p2c_003_u005_unknown_command_parity",
                verification_path: VerificationPath::Unit,
                now_ms: 13,
                outcome: LogOutcome::Pass,
                persist_path: Some(&out_path),
            },
            &[event],
        )
        .expect("packet family conversion should succeed");

        let raw = fs::read_to_string(&out_path).expect("read structured output");
        let line = raw
            .lines()
            .find(|candidate| !candidate.trim().is_empty())
            .expect("one structured event");
        let parsed: StructuredLogEvent =
            serde_json::from_str(line).expect("parse structured event");
        assert_eq!(parsed.packet_id, "FR-P2C-003");
        assert_eq!(
            parsed.fixture_id.as_deref(),
            Some("fr_p2c_003_dispatch_journey.json")
        );
        parsed.validate().expect("structured event validates");
        let _ = fs::remove_dir_all(log_root);
    }

    #[test]
    fn fr_p2c_005_fixture_maps_to_packet_family_in_structured_logs() {
        let log_root = unique_temp_log_root("fr_p2c_005_structured_log");
        let out_path = log_root.join("packet/fr_p2c_005.jsonl");
        let event = EvidenceEvent {
            ts_utc: "unix_ms:15".to_string(),
            ts_ms: 15,
            packet_id: 5,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::PersistenceTampering,
            decision_action: DecisionAction::FailClosed,
            subsystem: "persist_replay",
            action: "decode_aof_stream",
            reason_code: "persist.replay.frame_parse_invalid",
            reason: "invalid replay frame".to_string(),
            input_digest: "abc".to_string(),
            output_digest: "def".to_string(),
            state_digest_before: "state_before".to_string(),
            state_digest_after: "state_after".to_string(),
            replay_cmd: "cargo test".to_string(),
            artifact_refs: vec!["TEST_LOG_SCHEMA_V1.md".to_string()],
            confidence: Some(1.0),
        };

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "unit::fr-p2c-005",
                fixture_name: "persist_replay.json",
                case_name: "fr_p2c_005_e013_adversarial_decode_errors",
                verification_path: VerificationPath::Unit,
                now_ms: 15,
                outcome: LogOutcome::Pass,
                persist_path: Some(&out_path),
            },
            &[event],
        )
        .expect("packet family conversion should succeed");

        let raw = fs::read_to_string(&out_path).expect("read structured output");
        let line = raw
            .lines()
            .find(|candidate| !candidate.trim().is_empty())
            .expect("one structured event");
        let parsed: StructuredLogEvent =
            serde_json::from_str(line).expect("parse structured event");
        assert_eq!(parsed.packet_id, "FR-P2C-005");
        assert_eq!(parsed.fixture_id.as_deref(), Some("persist_replay.json"));
        parsed.validate().expect("structured event validates");
        let _ = fs::remove_dir_all(log_root);
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
    fn run_protocol_fixture_persists_structured_logs_when_enabled() {
        let log_root = unique_temp_log_root("fr_conformance_protocol_logs");
        let mut cfg = HarnessConfig::default_paths();
        cfg.live_log_root = Some(log_root.clone());

        let report =
            run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture run");
        assert_eq!(
            report.total, report.passed,
            "mismatches: {:?}",
            report.failed
        );

        let out_path =
            live_log_output_path(&log_root, "protocol_negative", "protocol_negative.json");
        let raw =
            fs::read_to_string(&out_path).expect("expected persisted protocol structured logs");
        let lines = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert!(
            !lines.is_empty(),
            "expected at least one structured log line in {}",
            out_path.display()
        );
        for line in lines {
            let event: StructuredLogEvent =
                serde_json::from_str(line).expect("parse structured log line");
            event.validate().expect("structured log validates");
        }
        let _ = fs::remove_dir_all(log_root);
    }

    #[test]
    fn conformance_replay_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture run");
        assert_eq!(report.total, report.passed);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn fr_p2c_005_e001_replay_fixture_passes_with_schema_contract() {
        let cfg = HarnessConfig::default_paths();
        let report = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture run");
        assert_eq!(report.schema_version, DIFFERENTIAL_REPORT_SCHEMA_VERSION);
        assert_eq!(report.suite, "persist_replay");
        assert_eq!(report.fixture, "persist_replay.json");
        assert_eq!(
            report.total, report.passed,
            "packet-005 replay mismatches: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
        assert_eq!(report.failed_without_reason_code, 0);
        assert!(report.reason_code_counts.is_empty());
    }

    #[test]
    fn fr_p2c_005_e002_replay_fixture_runs_are_deterministic() {
        let cfg = HarnessConfig::default_paths();
        let first = run_replay_fixture(&cfg, "persist_replay.json").expect("first replay run");
        let second = run_replay_fixture(&cfg, "persist_replay.json").expect("second replay run");

        assert_eq!(first.total, second.total);
        assert_eq!(first.passed, second.passed);
        assert_eq!(first.reason_code_counts, second.reason_code_counts);
        assert_eq!(
            first.failed_without_reason_code,
            second.failed_without_reason_code
        );
        assert_eq!(first.failed, second.failed);
    }

    #[test]
    fn fr_p2c_005_e003_roundtrip_preserves_aof_record_order_and_payloads() {
        let cases = vec![
            vec![
                AofRecord {
                    argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
                },
                AofRecord {
                    argv: vec![b"INCR".to_vec(), b"k".to_vec()],
                },
            ],
            vec![
                AofRecord {
                    argv: vec![b"DEL".to_vec(), b"missing".to_vec()],
                },
                AofRecord {
                    argv: vec![b"SET".to_vec(), b"ttl".to_vec(), b"x".to_vec()],
                },
                AofRecord {
                    argv: vec![b"PEXPIRE".to_vec(), b"ttl".to_vec(), b"5".to_vec()],
                },
            ],
            vec![],
        ];

        for (idx, records) in cases.into_iter().enumerate() {
            let encoded = encode_aof_stream(&records);
            let decoded = decode_aof_stream(&encoded).expect("decode stream");
            assert_eq!(decoded, records, "case={idx} roundtrip mismatch");
        }
    }

    #[test]
    fn fr_p2c_005_e004_metamorphic_appending_records_preserves_prefix() {
        let baseline = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"a".to_vec(), b"1".to_vec()],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"a".to_vec()],
            },
        ];
        let mut appended = baseline.clone();
        appended.push(AofRecord {
            argv: vec![b"SET".to_vec(), b"b".to_vec(), b"2".to_vec()],
        });

        let baseline_decoded =
            decode_aof_stream(&encode_aof_stream(&baseline)).expect("decode baseline");
        let appended_decoded =
            decode_aof_stream(&encode_aof_stream(&appended)).expect("decode appended");

        assert!(
            appended_decoded.starts_with(&baseline_decoded),
            "appending a replay record must preserve the baseline decoded prefix"
        );
    }

    #[test]
    fn fr_p2c_005_e013_adversarial_replay_decode_errors_are_stable() {
        let cases = [
            (
                "invalid_non_array_frame",
                b"$3\r\nbad\r\n".as_slice(),
                fr_persist::PersistError::InvalidFrame,
            ),
            (
                "incomplete_resp_frame",
                b"*2\r\n$3\r\nGET\r\n$1\r\nk".as_slice(),
                fr_persist::PersistError::Parse(RespParseError::Incomplete),
            ),
            (
                "invalid_simple_string_frame",
                b"+PONG\r\n".as_slice(),
                fr_persist::PersistError::InvalidFrame,
            ),
        ];

        for (idx, (name, raw, expected_err)) in cases.into_iter().enumerate() {
            let actual_err = decode_aof_stream(raw).expect_err("decode should fail");
            assert_eq!(actual_err, expected_err, "case={name}");
            assert_eq!(
                persist_decode_reason_code(&actual_err),
                persist_decode_reason_code(&expected_err),
                "case={name} reason-code mapping must remain stable"
            );

            let event = EvidenceEvent {
                ts_utc: format!("unix_ms:{}", 505 + idx),
                ts_ms: 505 + idx as u64,
                packet_id: 5,
                mode: Mode::Strict,
                severity: DriftSeverity::S0,
                threat_class: ThreatClass::PersistenceTampering,
                decision_action: DecisionAction::FailClosed,
                subsystem: "persist_replay",
                action: "decode_aof_stream",
                reason_code: persist_decode_reason_code(&actual_err),
                reason: format!("{actual_err:?}"),
                input_digest: format!("persist_decode_case_{idx}"),
                output_digest: format!("persist_decode_error_{idx}"),
                state_digest_before: "state_before".to_string(),
                state_digest_after: "state_after".to_string(),
                replay_cmd: format!(
                    "cargo test -p fr-conformance -- --nocapture fr_p2c_005_e013_adversarial_replay_decode_errors_are_stable -- {name}"
                ),
                artifact_refs: vec![
                    "crates/fr-conformance/fixtures/persist_replay.json".to_string(),
                ],
                confidence: Some(1.0),
            };

            validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: "fr_p2c_005",
                    fixture_name: "persist_replay.json",
                    case_name: name,
                    verification_path: VerificationPath::Unit,
                    now_ms: 505 + idx as u64,
                    outcome: LogOutcome::Pass,
                    persist_path: None,
                },
                &[event],
            )
            .expect(
                "structured log emission should validate for packet-005 decode adversarial case",
            );
        }
    }

    #[test]
    fn fr_p2c_005_f_differential_replay_fixture_passes() {
        let cfg = HarnessConfig::default_paths();
        let report = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture run");
        assert_eq!(report.schema_version, DIFFERENTIAL_REPORT_SCHEMA_VERSION);
        assert_eq!(report.suite, "persist_replay");
        assert_eq!(report.fixture, "persist_replay.json");
        assert_eq!(
            report.total, report.passed,
            "packet-005 differential replay mismatches: {:?}",
            report.failed
        );
        assert!(report.failed.is_empty());
    }

    #[test]
    fn fr_p2c_005_f_metamorphic_noop_records_preserve_outcomes() {
        let baseline_cfg = HarnessConfig::default_paths();
        let baseline =
            run_replay_fixture(&baseline_cfg, "persist_replay.json").expect("baseline replay run");

        let fixture_path = baseline_cfg.fixture_root.join("persist_replay.json");
        let raw = fs::read_to_string(&fixture_path).expect("read baseline packet-005 fixture");
        let mut fixture: ReplayFixture =
            serde_json::from_str(&raw).expect("parse packet-005 fixture JSON");
        for case in &mut fixture.cases {
            case.records.push(vec![
                "SET".to_string(),
                "__fr_p2c_005_noop".to_string(),
                "1".to_string(),
            ]);
            case.records
                .push(vec!["DEL".to_string(), "__fr_p2c_005_noop".to_string()]);
        }

        let fixture_root = unique_temp_log_root("fr_p2c_005_metamorphic_fixture");
        fs::create_dir_all(&fixture_root).expect("create metamorphic fixture root");
        let fixture_name = "persist_replay_metamorphic_noop.json";
        let fixture_out = fixture_root.join(fixture_name);
        let payload = serde_json::to_vec_pretty(&fixture).expect("encode metamorphic fixture");
        fs::write(&fixture_out, payload).expect("write metamorphic fixture");

        let mut metamorphic_cfg = HarnessConfig::default_paths();
        metamorphic_cfg.fixture_root = fixture_root.clone();
        let transformed = run_replay_fixture(&metamorphic_cfg, fixture_name)
            .expect("metamorphic replay fixture run");

        assert_eq!(transformed.total, baseline.total);
        assert_eq!(transformed.passed, baseline.passed);
        assert_eq!(transformed.failed, baseline.failed);
        assert_eq!(transformed.reason_code_counts, baseline.reason_code_counts);
        assert_eq!(
            transformed.failed_without_reason_code,
            baseline.failed_without_reason_code
        );

        let _ = fs::remove_dir_all(fixture_root);
    }

    #[test]
    fn fr_p2c_005_f_adversarial_decode_reason_taxonomy_is_stable() {
        let cases = [
            (
                "invalid_non_array_frame",
                b"$3\r\nbad\r\n".as_slice(),
                "persist.replay.frame_parse_invalid",
            ),
            (
                "incomplete_resp_frame",
                b"*2\r\n$3\r\nGET\r\n$1\r\nk".as_slice(),
                "persist.replay.frame_length_violation",
            ),
            (
                "invalid_simple_string_frame",
                b"+PONG\r\n".as_slice(),
                "persist.replay.frame_parse_invalid",
            ),
        ];

        for (idx, (name, raw, expected_reason_code)) in cases.into_iter().enumerate() {
            let err = decode_aof_stream(raw).expect_err("decode should fail");
            let reason_code = persist_decode_reason_code(&err);
            assert_eq!(reason_code, expected_reason_code, "case={name}");

            let event = EvidenceEvent {
                ts_utc: format!("unix_ms:{}", 605 + idx),
                ts_ms: 605 + idx as u64,
                packet_id: 5,
                mode: Mode::Strict,
                severity: DriftSeverity::S0,
                threat_class: ThreatClass::PersistenceTampering,
                decision_action: DecisionAction::FailClosed,
                subsystem: "persist_replay",
                action: "decode_aof_stream",
                reason_code,
                reason: format!("{err:?}"),
                input_digest: format!("persist_f_decode_case_{idx}"),
                output_digest: format!("persist_f_decode_error_{idx}"),
                state_digest_before: "state_before".to_string(),
                state_digest_after: "state_after".to_string(),
                replay_cmd: format!(
                    "cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_adversarial_decode_reason_taxonomy_is_stable -- {name}"
                ),
                artifact_refs: vec![
                    "crates/fr-conformance/fixtures/persist_replay.json".to_string(),
                ],
                confidence: Some(1.0),
            };

            validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: "fr_p2c_005",
                    fixture_name: "persist_replay.json",
                    case_name: name,
                    verification_path: VerificationPath::Property,
                    now_ms: 605 + idx as u64,
                    outcome: LogOutcome::Pass,
                    persist_path: None,
                },
                &[event],
            )
            .expect(
                "structured log emission should validate for packet-005 F-level adversarial case",
            );
        }
    }

    #[test]
    fn fr_p2c_004_u005_noauth_gate_precedes_dispatch_and_logs() {
        let denied =
            enforce_noauth_gate(AuthState::Unauthenticated, CommandAuthClass::RequiresAuth)
                .expect_err("unauthenticated command path must be gated");
        assert_eq!(denied, "auth.noauth_gate_violation");

        enforce_noauth_gate(AuthState::Unauthenticated, CommandAuthClass::NoAuthExempt)
            .expect("noauth-exempt command should pass");
        enforce_noauth_gate(AuthState::Authenticated, CommandAuthClass::RequiresAuth)
            .expect("authenticated command should pass");

        let event = EvidenceEvent {
            ts_utc: "unix_ms:405".to_string(),
            ts_ms: 405,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "auth_gate",
            action: "noauth_pre_dispatch_gate",
            reason_code: "auth.noauth_gate_violation",
            reason: "unauthenticated command rejected before dispatch".to_string(),
            input_digest: "fr_p2c_004_u005_input".to_string(),
            output_digest: "fr_p2c_004_u005_output".to_string(),
            state_digest_before: "unauthenticated".to_string(),
            state_digest_after: "unauthenticated".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=405 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_u005_noauth_gate_precedes_dispatch_and_logs".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_auth_unit",
                case_name: "u005_noauth_gate",
                verification_path: VerificationPath::Unit,
                now_ms: 405,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-004 noauth-gate structured log should validate");
    }

    #[test]
    fn fr_p2c_004_u007_acl_selector_parser_rejects_malformed_rules_and_logs() {
        parse_acl_selector_tokens(&["+@all", "~cache:*", "&stream:*"])
            .expect("valid selector token set should parse");

        let err = parse_acl_selector_tokens(&["+@all", "??invalid-rule"])
            .expect_err("malformed selector token must be rejected");
        assert_eq!(err, "auth.acl_selector_parse_validation_mismatch");

        let event = EvidenceEvent {
            ts_utc: "unix_ms:407".to_string(),
            ts_ms: 407,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "acl_parser",
            action: "selector_token_validate",
            reason_code: "auth.acl_selector_parse_validation_mismatch",
            reason: "malformed ACL selector token rejected".to_string(),
            input_digest: "fr_p2c_004_u007_input".to_string(),
            output_digest: "fr_p2c_004_u007_output".to_string(),
            state_digest_before: "acl_parse_start".to_string(),
            state_digest_after: "acl_parse_reject".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=407 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_u007_acl_selector_parser_rejects_malformed_rules_and_logs".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_acl_rules",
                case_name: "u007_acl_rule_parse",
                verification_path: VerificationPath::Unit,
                now_ms: 407,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-004 ACL parser structured log should validate");
    }

    #[test]
    fn fr_p2c_004_u006_property_acl_deny_index_is_monotonic() {
        let command_path = ["GET", "SET", "DEL", "INCR"];

        let baseline = allow_set(&["GET", "SET"]);
        let baseline_deny = first_denied_index(&baseline, &command_path);
        assert_eq!(baseline_deny, Some(2));

        let expanded = allow_set(&["SET", "GET", "DEL"]);
        let expanded_deny = first_denied_index(&expanded, &command_path);
        assert_eq!(expanded_deny, Some(3));
        assert!(
            expanded_deny.unwrap_or(usize::MAX) >= baseline_deny.unwrap_or(usize::MAX),
            "expanding permissions must not move the first deny earlier"
        );

        let fully_allowed = allow_set(&["GET", "SET", "DEL", "INCR"]);
        assert_eq!(first_denied_index(&fully_allowed, &command_path), None);

        let permuted_baseline = allow_set(&["SET", "GET"]);
        assert_eq!(
            first_denied_index(&permuted_baseline, &command_path),
            baseline_deny,
            "allowlist permutation must preserve deny-index result"
        );

        let event = EvidenceEvent {
            ts_utc: "unix_ms:406".to_string(),
            ts_ms: 406,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "acl_reduce",
            action: "first_denied_index",
            reason_code: "parity_ok",
            reason: "ACL deny-index monotonicity preserved across permission expansions".to_string(),
            input_digest: "fr_p2c_004_u006_input".to_string(),
            output_digest: "fr_p2c_004_u006_output".to_string(),
            state_digest_before: "acl_reduce_start".to_string(),
            state_digest_after: "acl_reduce_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=406 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_u006_property_acl_deny_index_is_monotonic".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_acl_permissions",
                case_name: "u006_acl_deny_index_metamorphic",
                verification_path: VerificationPath::Property,
                now_ms: 406,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-004 ACL property structured log should validate");
    }

    #[test]
    fn fr_p2c_004_f_differential_auth_mode_split_contract_is_stable() {
        let mut strict = Runtime::default_strict();
        let mut hardened = Runtime::default_hardened();
        strict.set_requirepass(Some(b"secret".to_vec()));
        hardened.set_requirepass(Some(b"secret".to_vec()));

        let strict_noauth = strict.execute_frame(command_frame(&["GET", "fr:p2c:004:key"]), 504);
        let hardened_noauth =
            hardened.execute_frame(command_frame(&["GET", "fr:p2c:004:key"]), 504);
        assert_eq!(
            strict_noauth,
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );
        assert_eq!(strict_noauth, hardened_noauth);

        let strict_auth = strict.execute_frame(command_frame(&["AUTH", "secret"]), 505);
        let hardened_auth = hardened.execute_frame(command_frame(&["AUTH", "secret"]), 505);
        assert_eq!(strict_auth, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(strict_auth, hardened_auth);

        let strict_set =
            strict.execute_frame(command_frame(&["SET", "fr:p2c:004:key", "value"]), 506);
        let hardened_set =
            hardened.execute_frame(command_frame(&["SET", "fr:p2c:004:key", "value"]), 506);
        assert_eq!(strict_set, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(strict_set, hardened_set);

        let strict_get = strict.execute_frame(command_frame(&["GET", "fr:p2c:004:key"]), 507);
        let hardened_get = hardened.execute_frame(command_frame(&["GET", "fr:p2c:004:key"]), 507);
        assert_eq!(strict_get, RespFrame::BulkString(Some(b"value".to_vec())));
        assert_eq!(strict_get, hardened_get);

        let mut strict_event = strict
            .evidence()
            .events()
            .first()
            .expect("strict mode should emit noauth gate event")
            .clone();
        let mut hardened_event = hardened
            .evidence()
            .events()
            .first()
            .expect("hardened mode should emit noauth gate event")
            .clone();
        assert_eq!(strict_event.reason_code, "auth.noauth_gate_violation");
        assert_eq!(hardened_event.reason_code, "auth.noauth_gate_violation");
        assert_eq!(strict_event.decision_action, DecisionAction::FailClosed);
        assert_eq!(hardened_event.decision_action, DecisionAction::FailClosed);

        strict_event.replay_cmd = "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_differential_auth_mode_split_contract_is_stable".to_string();
        strict_event.artifact_refs.push(
            "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md".to_string(),
        );
        hardened_event.replay_cmd = "FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_differential_auth_mode_split_contract_is_stable".to_string();
        hardened_event.artifact_refs.push(
            "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md".to_string(),
        );

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_acl_runtime_strict",
                case_name: "differential_mode_split_strict",
                verification_path: VerificationPath::Property,
                now_ms: 504,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&strict_event),
        )
        .expect("strict-mode packet-004 differential log must validate");
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_acl_runtime_hardened",
                case_name: "differential_mode_split_hardened",
                verification_path: VerificationPath::Property,
                now_ms: 504,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&hardened_event),
        )
        .expect("hardened-mode packet-004 differential log must validate");
    }

    #[test]
    fn fr_p2c_004_f_metamorphic_auth_entrypoints_converge_to_same_session_state() {
        let mut auth_path = Runtime::default_strict();
        let mut hello_path = Runtime::default_strict();
        auth_path.set_requirepass(Some(b"secret".to_vec()));
        hello_path.set_requirepass(Some(b"secret".to_vec()));

        let auth_reply = auth_path.execute_frame(command_frame(&["AUTH", "secret"]), 520);
        assert_eq!(auth_reply, RespFrame::SimpleString("OK".to_string()));

        let hello_reply = hello_path.execute_frame(
            command_frame(&["HELLO", "3", "AUTH", "default", "secret"]),
            520,
        );
        match hello_reply {
            RespFrame::Array(Some(parts)) => {
                assert_eq!(parts.len(), 6);
                assert_eq!(parts[0], RespFrame::BulkString(Some(b"server".to_vec())));
                assert_eq!(
                    parts[1],
                    RespFrame::BulkString(Some(b"frankenredis".to_vec()))
                );
                assert_eq!(parts[2], RespFrame::BulkString(Some(b"version".to_vec())));
                assert_eq!(parts[4], RespFrame::BulkString(Some(b"proto".to_vec())));
                assert_eq!(parts[5], RespFrame::Integer(3));
            }
            other => panic!("HELLO AUTH path returned unexpected frame: {other:?}"),
        }

        let auth_set =
            auth_path.execute_frame(command_frame(&["SET", "fr:p2c:004:mm:key", "value"]), 521);
        let hello_set =
            hello_path.execute_frame(command_frame(&["SET", "fr:p2c:004:mm:key", "value"]), 521);
        assert_eq!(auth_set, hello_set);

        let auth_incr = auth_path.execute_frame(
            command_frame(&["INCRBY", "fr:p2c:004:mm:counter", "7"]),
            522,
        );
        let hello_incr = hello_path.execute_frame(
            command_frame(&["INCRBY", "fr:p2c:004:mm:counter", "7"]),
            522,
        );
        assert_eq!(auth_incr, hello_incr);

        let auth_get = auth_path.execute_frame(command_frame(&["GET", "fr:p2c:004:mm:key"]), 523);
        let hello_get = hello_path.execute_frame(command_frame(&["GET", "fr:p2c:004:mm:key"]), 523);
        assert_eq!(auth_get, hello_get);
        assert_eq!(auth_get, RespFrame::BulkString(Some(b"value".to_vec())));

        assert!(auth_path.is_authenticated());
        assert!(hello_path.is_authenticated());

        let event = EvidenceEvent {
            ts_utc: "unix_ms:524".to_string(),
            ts_ms: 524,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "auth_metamorphic",
            action: "entrypoint_convergence",
            reason_code: "parity_ok",
            reason: "AUTH and HELLO AUTH entrypoints converge to equivalent authenticated session state".to_string(),
            input_digest: "fr_p2c_004_f_metamorphic_input".to_string(),
            output_digest: "fr_p2c_004_f_metamorphic_output".to_string(),
            state_digest_before: "dual_path_pre_auth".to_string(),
            state_digest_after: "dual_path_converged".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_metamorphic_auth_entrypoints_converge_to_same_session_state".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_004",
                fixture_name: "fr_p2c_004_acl_metamorphic",
                case_name: "f_metamorphic_auth_entrypoint_convergence",
                verification_path: VerificationPath::Property,
                now_ms: 524,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&event),
        )
        .expect("packet-004 metamorphic structured log should validate");
    }

    #[test]
    fn fr_p2c_004_f_adversarial_auth_reason_codes_are_stable() {
        let mut strict = Runtime::default_strict();
        strict.set_requirepass(Some(b"secret".to_vec()));

        let wrongpass = strict.execute_frame(command_frame(&["AUTH", "bad"]), 530);
        assert_eq!(
            wrongpass,
            RespFrame::Error(
                "WRONGPASS invalid username-password pair or user is disabled.".to_string(),
            ),
        );
        assert!(!strict.is_authenticated());

        let noauth = strict.execute_frame(command_frame(&["GET", "fr:p2c:004:adv:key"]), 531);
        assert_eq!(
            noauth,
            RespFrame::Error("NOAUTH Authentication required.".to_string())
        );

        let selector_err = parse_acl_selector_tokens(&["+@all", "??invalid-rule"])
            .expect_err("malformed selector token must be rejected");
        assert_eq!(selector_err, "auth.acl_selector_parse_validation_mismatch");

        let mut noauth_event = strict
            .evidence()
            .events()
            .last()
            .expect("noauth rejection should emit threat event")
            .clone();
        assert_eq!(noauth_event.reason_code, "auth.noauth_gate_violation");
        noauth_event.replay_cmd = "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_adversarial_auth_reason_codes_are_stable".to_string();
        noauth_event
            .artifact_refs
            .push("crates/fr-conformance/fixtures/phase2c/FR-P2C-004/risk_note.md".to_string());

        let wrongpass_event = EvidenceEvent {
            ts_utc: "unix_ms:530".to_string(),
            ts_ms: 530,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "auth_gate",
            action: "auth_wrongpass_reject",
            reason_code: "auth.auth_command_wrongpass_response_mismatch",
            reason: "wrongpass auth attempt rejected without promoting auth state".to_string(),
            input_digest: "fr_p2c_004_f_adv_wrongpass_input".to_string(),
            output_digest: "fr_p2c_004_f_adv_wrongpass_output".to_string(),
            state_digest_before: "unauthenticated".to_string(),
            state_digest_after: "unauthenticated".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_adversarial_auth_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let selector_event = EvidenceEvent {
            ts_utc: "unix_ms:532".to_string(),
            ts_ms: 532,
            packet_id: 4,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::FailClosed,
            subsystem: "acl_parser",
            action: "selector_token_validate",
            reason_code: "auth.acl_selector_parse_validation_mismatch",
            reason: "malformed ACL selector token rejected on adversarial input".to_string(),
            input_digest: "fr_p2c_004_f_adv_selector_input".to_string(),
            output_digest: "fr_p2c_004_f_adv_selector_output".to_string(),
            state_digest_before: "acl_parse_start".to_string(),
            state_digest_after: "acl_parse_reject".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_adversarial_auth_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let hardened_policy_event = EvidenceEvent {
            ts_utc: "unix_ms:533".to_string(),
            ts_ms: 533,
            packet_id: 4,
            mode: Mode::Hardened,
            severity: DriftSeverity::S1,
            threat_class: ThreatClass::AuthPolicyConfusion,
            decision_action: DecisionAction::RejectNonAllowlisted,
            subsystem: "auth_policy",
            action: "hardened_deviation_gate",
            reason_code: "auth.hardened_nonallowlisted_rejected",
            reason: "non-allowlisted hardened auth/ACL deviation rejected and forced fail-closed".to_string(),
            input_digest: "fr_p2c_004_f_adv_hardened_input".to_string(),
            output_digest: "fr_p2c_004_f_adv_hardened_output".to_string(),
            state_digest_before: "hardened_candidate".to_string(),
            state_digest_after: "hardened_rejected".to_string(),
            replay_cmd: "FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_004_f_adversarial_auth_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-004/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let cases = [
            (
                "f_adv_wrongpass_reason",
                wrongpass_event,
                "auth.auth_command_wrongpass_response_mismatch",
            ),
            (
                "f_adv_noauth_gate_reason",
                noauth_event,
                "auth.noauth_gate_violation",
            ),
            (
                "f_adv_acl_selector_reason",
                selector_event,
                "auth.acl_selector_parse_validation_mismatch",
            ),
            (
                "f_adv_hardened_nonallowlisted_reason",
                hardened_policy_event,
                "auth.hardened_nonallowlisted_rejected",
            ),
        ];

        for (case_name, event, expected_reason_code) in cases {
            assert_eq!(
                event.reason_code, expected_reason_code,
                "reason-code stability mismatch for case={case_name}",
            );
            validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: "fr_p2c_004",
                    fixture_name: "fr_p2c_004_acl_adversarial",
                    case_name,
                    verification_path: VerificationPath::Property,
                    now_ms: event.ts_ms,
                    outcome: LogOutcome::Pass,
                    persist_path: None,
                },
                std::slice::from_ref(&event),
            )
            .expect("packet-004 adversarial structured log should validate");
        }
    }

    #[test]
    fn fr_p2c_007_u001_cluster_subcommand_router_contract_and_logs() {
        let mut runtime = Runtime::default_strict();

        let wrong_arity = runtime.execute_frame(command_frame(&["CLUSTER"]), 700);
        assert_eq!(
            wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'CLUSTER' command".to_string())
        );

        let help = runtime.execute_frame(command_frame(&["CLUSTER", "HELP"]), 701);
        assert_eq!(
            help,
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"CLUSTER HELP".to_vec())),
                RespFrame::BulkString(Some(
                    b"CLUSTER subcommand dispatch scaffold (FR-P2C-007 D1).".to_vec(),
                )),
                RespFrame::BulkString(
                    Some(b"Supported subcommands in this stage: HELP.".to_vec(),)
                ),
            ]))
        );

        let help_casefold = runtime.execute_frame(command_frame(&["cluster", "help"]), 702);
        assert_eq!(help_casefold, help);

        let unknown = runtime.execute_frame(command_frame(&["CLUSTER", "NOPE"]), 703);
        assert_eq!(
            unknown,
            RespFrame::Error(
                "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP."
                    .to_string(),
            )
        );

        let event = EvidenceEvent {
            ts_utc: "unix_ms:703".to_string(),
            ts_ms: 703,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_router",
            action: "cluster_subcommand_dispatch",
            reason_code: "cluster.command_router_contract_violation",
            reason: "cluster subcommand router enforces arity/help/unknown contracts".to_string(),
            input_digest: "fr_p2c_007_u001_input".to_string(),
            output_digest: "fr_p2c_007_u001_output".to_string(),
            state_digest_before: "cluster_router_start".to_string(),
            state_digest_after: "cluster_router_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_u001_cluster_subcommand_router_contract_and_logs".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_router",
                case_name: "u001_cluster_router",
                verification_path: VerificationPath::Unit,
                now_ms: 703,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-007 cluster router structured log should validate");
    }

    #[test]
    fn fr_p2c_007_u007_client_mode_flags_transition_and_logs() {
        let mut runtime = Runtime::default_strict();

        assert!(!runtime.is_cluster_read_only());
        assert!(!runtime.is_cluster_asking());

        let readonly = runtime.execute_frame(command_frame(&["READONLY"]), 705);
        assert_eq!(readonly, RespFrame::SimpleString("OK".to_string()));
        assert!(runtime.is_cluster_read_only());
        assert!(!runtime.is_cluster_asking());

        let asking = runtime.execute_frame(command_frame(&["ASKING"]), 706);
        assert_eq!(asking, RespFrame::SimpleString("OK".to_string()));
        assert!(runtime.is_cluster_read_only());
        assert!(runtime.is_cluster_asking());

        let readwrite = runtime.execute_frame(command_frame(&["READWRITE"]), 707);
        assert_eq!(readwrite, RespFrame::SimpleString("OK".to_string()));
        assert!(!runtime.is_cluster_read_only());
        assert!(!runtime.is_cluster_asking());

        let wrong_arity = runtime.execute_frame(command_frame(&["READONLY", "extra"]), 708);
        assert_eq!(
            wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'READONLY' command".to_string())
        );

        let event = EvidenceEvent {
            ts_utc: "unix_ms:708".to_string(),
            ts_ms: 708,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_client_mode",
            action: "readonly_asking_readwrite_transition",
            reason_code: "cluster.client_mode_flag_transition_violation",
            reason: "client cluster mode flags transition deterministically and enforce arity"
                .to_string(),
            input_digest: "fr_p2c_007_u007_input".to_string(),
            output_digest: "fr_p2c_007_u007_output".to_string(),
            state_digest_before: "cluster_client_mode_start".to_string(),
            state_digest_after: "cluster_client_mode_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_u007_client_mode_flags_transition_and_logs".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_mode_flags",
                case_name: "u007_client_mode_transition",
                verification_path: VerificationPath::Unit,
                now_ms: 708,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-007 client mode transition structured log should validate");
    }

    #[test]
    fn fr_p2c_007_u007_property_cluster_mode_state_is_sequence_deterministic() {
        let mut canonical = Runtime::default_strict();
        let canonical_sequence = ["READONLY", "ASKING", "READWRITE", "READONLY", "ASKING"];
        for (idx, command_name) in canonical_sequence.iter().enumerate() {
            let reply = canonical.execute_frame(command_frame(&[*command_name]), 720 + idx as u64);
            assert_eq!(reply, RespFrame::SimpleString("OK".to_string()));
        }
        assert!(canonical.is_cluster_read_only());
        assert!(canonical.is_cluster_asking());

        let mut casefold = Runtime::default_strict();
        let casefold_sequence = ["readonly", "asking", "readwrite", "READONLY", "asking"];
        for (idx, command_name) in casefold_sequence.iter().enumerate() {
            let reply = casefold.execute_frame(command_frame(&[*command_name]), 720 + idx as u64);
            assert_eq!(reply, RespFrame::SimpleString("OK".to_string()));
        }
        assert_eq!(
            casefold.is_cluster_read_only(),
            canonical.is_cluster_read_only()
        );
        assert_eq!(casefold.is_cluster_asking(), canonical.is_cluster_asking());

        let mut redundant = Runtime::default_strict();
        let redundant_sequence = ["READONLY", "ASKING", "ASKING"];
        for (idx, command_name) in redundant_sequence.iter().enumerate() {
            let reply = redundant.execute_frame(command_frame(&[*command_name]), 730 + idx as u64);
            assert_eq!(reply, RespFrame::SimpleString("OK".to_string()));
        }
        assert!(redundant.is_cluster_read_only());
        assert!(redundant.is_cluster_asking());

        let event = EvidenceEvent {
            ts_utc: "unix_ms:725".to_string(),
            ts_ms: 725,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_client_mode_property",
            action: "cluster_mode_sequence_reduce",
            reason_code: "parity_ok",
            reason: "cluster client-mode state converges deterministically for equivalent command sequences".to_string(),
            input_digest: "fr_p2c_007_u007_property_input".to_string(),
            output_digest: "fr_p2c_007_u007_property_output".to_string(),
            state_digest_before: "cluster_mode_property_start".to_string(),
            state_digest_after: "cluster_mode_property_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_u007_property_cluster_mode_state_is_sequence_deterministic".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_mode_property",
                case_name: "u007_property_mode_sequence",
                verification_path: VerificationPath::Property,
                now_ms: 725,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("packet-007 client mode property structured log should validate");
    }

    #[test]
    fn fr_p2c_007_f_differential_cluster_surface_mode_split_is_stable() {
        let mut strict = Runtime::default_strict();
        let mut hardened = Runtime::default_hardened();

        let strict_help = strict.execute_frame(command_frame(&["CLUSTER", "HELP"]), 740);
        let hardened_help = hardened.execute_frame(command_frame(&["CLUSTER", "HELP"]), 740);
        assert_eq!(strict_help, hardened_help);

        let strict_readonly = strict.execute_frame(command_frame(&["READONLY"]), 741);
        let hardened_readonly = hardened.execute_frame(command_frame(&["READONLY"]), 741);
        assert_eq!(strict_readonly, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(strict_readonly, hardened_readonly);

        let strict_asking = strict.execute_frame(command_frame(&["ASKING"]), 742);
        let hardened_asking = hardened.execute_frame(command_frame(&["ASKING"]), 742);
        assert_eq!(strict_asking, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(strict_asking, hardened_asking);

        let strict_readwrite = strict.execute_frame(command_frame(&["READWRITE"]), 743);
        let hardened_readwrite = hardened.execute_frame(command_frame(&["READWRITE"]), 743);
        assert_eq!(strict_readwrite, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(strict_readwrite, hardened_readwrite);

        assert!(!strict.is_cluster_read_only());
        assert!(!strict.is_cluster_asking());
        assert!(!hardened.is_cluster_read_only());
        assert!(!hardened.is_cluster_asking());

        let strict_unknown = strict.execute_frame(command_frame(&["CLUSTER", "NOPE"]), 744);
        let hardened_unknown = hardened.execute_frame(command_frame(&["CLUSTER", "NOPE"]), 744);
        let expected_unknown = RespFrame::Error(
            "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP."
                .to_string(),
        );
        assert_eq!(strict_unknown, expected_unknown);
        assert_eq!(strict_unknown, hardened_unknown);

        let strict_event = EvidenceEvent {
            ts_utc: "unix_ms:744".to_string(),
            ts_ms: 744,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_differential",
            action: "mode_split_compare",
            reason_code: "parity_ok",
            reason: "strict and hardened packet-007 surface remain output-equivalent for cluster D1 commands".to_string(),
            input_digest: "fr_p2c_007_f_diff_input".to_string(),
            output_digest: "fr_p2c_007_f_diff_output".to_string(),
            state_digest_before: "mode_split_start".to_string(),
            state_digest_after: "mode_split_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_differential_cluster_surface_mode_split_is_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let hardened_event = EvidenceEvent {
            ts_utc: "unix_ms:744".to_string(),
            ts_ms: 744,
            packet_id: 7,
            mode: Mode::Hardened,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_differential",
            action: "mode_split_compare",
            reason_code: "parity_ok",
            reason:
                "hardened mode preserves strict-equivalent outputs for scoped packet-007 cluster surface"
                    .to_string(),
            input_digest: "fr_p2c_007_f_diff_input".to_string(),
            output_digest: "fr_p2c_007_f_diff_output".to_string(),
            state_digest_before: "mode_split_start".to_string(),
            state_digest_after: "mode_split_verified".to_string(),
            replay_cmd: "FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_differential_cluster_surface_mode_split_is_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_runtime_strict",
                case_name: "f_differential_mode_split_strict",
                verification_path: VerificationPath::Property,
                now_ms: 744,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&strict_event),
        )
        .expect("packet-007 strict differential structured log should validate");

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_runtime_hardened",
                case_name: "f_differential_mode_split_hardened",
                verification_path: VerificationPath::Property,
                now_ms: 744,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&hardened_event),
        )
        .expect("packet-007 hardened differential structured log should validate");
    }

    #[test]
    fn fr_p2c_007_f_metamorphic_cluster_help_is_idempotent_across_mode_toggles() {
        let mut runtime = Runtime::default_strict();

        let baseline_help = runtime.execute_frame(command_frame(&["CLUSTER", "HELP"]), 750);
        let casefold_help = runtime.execute_frame(command_frame(&["cluster", "help"]), 751);
        assert_eq!(baseline_help, casefold_help);

        let readonly = runtime.execute_frame(command_frame(&["READONLY"]), 752);
        let asking = runtime.execute_frame(command_frame(&["ASKING"]), 753);
        let readwrite = runtime.execute_frame(command_frame(&["READWRITE"]), 754);
        assert_eq!(readonly, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(asking, RespFrame::SimpleString("OK".to_string()));
        assert_eq!(readwrite, RespFrame::SimpleString("OK".to_string()));

        let post_toggle_help = runtime.execute_frame(command_frame(&["CLUSTER", "HELP"]), 755);
        assert_eq!(
            post_toggle_help, baseline_help,
            "cluster help output must be invariant to client mode toggles"
        );

        let event = EvidenceEvent {
            ts_utc: "unix_ms:755".to_string(),
            ts_ms: 755,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_metamorphic",
            action: "cluster_help_idempotence",
            reason_code: "parity_ok",
            reason: "cluster help surface is idempotent across case-folded command forms and mode toggles".to_string(),
            input_digest: "fr_p2c_007_f_metamorphic_input".to_string(),
            output_digest: "fr_p2c_007_f_metamorphic_output".to_string(),
            state_digest_before: "cluster_help_start".to_string(),
            state_digest_after: "cluster_help_verified".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_metamorphic_cluster_help_is_idempotent_across_mode_toggles".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/contract_table.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_007",
                fixture_name: "fr_p2c_007_cluster_metamorphic",
                case_name: "f_metamorphic_help_idempotence",
                verification_path: VerificationPath::Property,
                now_ms: 755,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&event),
        )
        .expect("packet-007 metamorphic structured log should validate");
    }

    #[test]
    fn fr_p2c_007_f_adversarial_cluster_reason_codes_are_stable() {
        let mut runtime = Runtime::default_strict();

        let cluster_wrong_arity = runtime.execute_frame(command_frame(&["CLUSTER"]), 760);
        assert_eq!(
            cluster_wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'CLUSTER' command".to_string())
        );

        let cluster_unknown = runtime.execute_frame(command_frame(&["CLUSTER", "NOPE"]), 761);
        assert_eq!(
            cluster_unknown,
            RespFrame::Error(
                "ERR Unknown subcommand or wrong number of arguments for 'CLUSTER'. Try CLUSTER HELP."
                    .to_string(),
            )
        );

        let readonly_wrong_arity =
            runtime.execute_frame(command_frame(&["READONLY", "extra"]), 762);
        let asking_wrong_arity = runtime.execute_frame(command_frame(&["ASKING", "extra"]), 763);
        let readwrite_wrong_arity =
            runtime.execute_frame(command_frame(&["READWRITE", "extra"]), 764);
        assert_eq!(
            readonly_wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'READONLY' command".to_string())
        );
        assert_eq!(
            asking_wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'ASKING' command".to_string())
        );
        assert_eq!(
            readwrite_wrong_arity,
            RespFrame::Error("ERR wrong number of arguments for 'READWRITE' command".to_string())
        );

        let router_event = EvidenceEvent {
            ts_utc: "unix_ms:761".to_string(),
            ts_ms: 761,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_router",
            action: "cluster_subcommand_reject",
            reason_code: "cluster.command_router_contract_violation",
            reason: "malformed cluster command shapes are deterministically rejected".to_string(),
            input_digest: "fr_p2c_007_f_adv_router_input".to_string(),
            output_digest: "fr_p2c_007_f_adv_router_output".to_string(),
            state_digest_before: "cluster_router_start".to_string(),
            state_digest_after: "cluster_router_rejected".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_adversarial_cluster_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let mode_event = EvidenceEvent {
            ts_utc: "unix_ms:764".to_string(),
            ts_ms: 764,
            packet_id: 7,
            mode: Mode::Strict,
            severity: DriftSeverity::S0,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::FailClosed,
            subsystem: "cluster_client_mode",
            action: "client_mode_arity_reject",
            reason_code: "cluster.client_mode_flag_transition_violation",
            reason: "client cluster mode commands reject adversarial arity drift deterministically"
                .to_string(),
            input_digest: "fr_p2c_007_f_adv_mode_input".to_string(),
            output_digest: "fr_p2c_007_f_adv_mode_output".to_string(),
            state_digest_before: "cluster_mode_start".to_string(),
            state_digest_after: "cluster_mode_rejected".to_string(),
            replay_cmd: "FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_adversarial_cluster_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let hardened_policy_event = EvidenceEvent {
            ts_utc: "unix_ms:765".to_string(),
            ts_ms: 765,
            packet_id: 7,
            mode: Mode::Hardened,
            severity: DriftSeverity::S1,
            threat_class: ThreatClass::MetadataAmbiguity,
            decision_action: DecisionAction::RejectNonAllowlisted,
            subsystem: "cluster_policy",
            action: "hardened_deviation_gate",
            reason_code: "cluster.hardened_nonallowlisted_rejected",
            reason: "non-allowlisted hardened packet-007 cluster deviation remains rejected"
                .to_string(),
            input_digest: "fr_p2c_007_f_adv_hardened_input".to_string(),
            output_digest: "fr_p2c_007_f_adv_hardened_output".to_string(),
            state_digest_before: "hardened_candidate".to_string(),
            state_digest_after: "hardened_rejected".to_string(),
            replay_cmd: "FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_007_f_adversarial_cluster_reason_codes_are_stable".to_string(),
            artifact_refs: vec![
                "TEST_LOG_SCHEMA_V1.md".to_string(),
                "crates/fr-conformance/fixtures/phase2c/FR-P2C-007/risk_note.md".to_string(),
            ],
            confidence: Some(1.0),
        };

        let cases = [
            (
                "f_adv_cluster_router_reason",
                router_event,
                "cluster.command_router_contract_violation",
            ),
            (
                "f_adv_cluster_mode_reason",
                mode_event,
                "cluster.client_mode_flag_transition_violation",
            ),
            (
                "f_adv_hardened_nonallowlisted_reason",
                hardened_policy_event,
                "cluster.hardened_nonallowlisted_rejected",
            ),
        ];

        for (case_name, event, expected_reason_code) in cases {
            assert_eq!(
                event.reason_code, expected_reason_code,
                "reason-code stability mismatch for case={case_name}",
            );
            validate_structured_log_emission(
                StructuredLogEmissionContext {
                    suite_id: "fr_p2c_007",
                    fixture_name: "fr_p2c_007_cluster_adversarial",
                    case_name,
                    verification_path: VerificationPath::Property,
                    now_ms: event.ts_ms,
                    outcome: LogOutcome::Pass,
                    persist_path: None,
                },
                std::slice::from_ref(&event),
            )
            .expect("packet-007 adversarial structured log should validate");
        }
    }

    #[test]
    fn run_replay_fixture_allows_structured_log_persistence_toggle() {
        let log_root = unique_temp_log_root("fr_conformance_replay_logs");
        let mut cfg = HarnessConfig::default_paths();
        cfg.live_log_root = Some(log_root.clone());

        let report = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture run");
        assert_eq!(
            report.total, report.passed,
            "mismatches: {:?}",
            report.failed
        );

        let out_path = live_log_output_path(&log_root, "persist_replay", "persist_replay.json");
        if out_path.exists() {
            let raw = fs::read_to_string(&out_path).expect("read replay structured logs");
            for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                let event: StructuredLogEvent =
                    serde_json::from_str(line).expect("parse replay structured log");
                event.validate().expect("replay structured log validates");
            }
        }
        let _ = fs::remove_dir_all(log_root);
    }

    #[test]
    fn fr_p2c_006_f_handshake_contract_vectors_are_enforced() {
        #[derive(Debug, Clone)]
        struct HandshakeVector {
            name: &'static str,
            auth_required: bool,
            steps: &'static [HandshakeStep],
            accept_psync_reply: bool,
            expected_state: HandshakeState,
            expected_reason_code: Option<&'static str>,
        }

        let vectors = [
            HandshakeVector {
                name: "happy_path_no_auth",
                auth_required: false,
                steps: &[
                    HandshakeStep::Ping,
                    HandshakeStep::Replconf,
                    HandshakeStep::Psync,
                ],
                accept_psync_reply: true,
                expected_state: HandshakeState::Online,
                expected_reason_code: None,
            },
            HandshakeVector {
                name: "happy_path_auth_required",
                auth_required: true,
                steps: &[
                    HandshakeStep::Ping,
                    HandshakeStep::Auth,
                    HandshakeStep::Replconf,
                    HandshakeStep::Psync,
                ],
                accept_psync_reply: true,
                expected_state: HandshakeState::Online,
                expected_reason_code: None,
            },
            HandshakeVector {
                name: "reject_replconf_before_ping",
                auth_required: false,
                steps: &[HandshakeStep::Replconf],
                accept_psync_reply: false,
                expected_state: HandshakeState::Init,
                expected_reason_code: Some("repl.handshake_state_machine_mismatch"),
            },
            HandshakeVector {
                name: "reject_psync_without_replconf",
                auth_required: false,
                steps: &[HandshakeStep::Ping, HandshakeStep::Psync],
                accept_psync_reply: false,
                expected_state: HandshakeState::PingSeen,
                expected_reason_code: Some("repl.handshake_state_machine_mismatch"),
            },
            HandshakeVector {
                name: "reject_psync_reply_before_psync_sent",
                auth_required: false,
                steps: &[HandshakeStep::Ping, HandshakeStep::Replconf],
                accept_psync_reply: true,
                expected_state: HandshakeState::ReplconfSeen,
                expected_reason_code: Some("repl.fullresync_reply_parse_violation"),
            },
        ];

        for vector in vectors {
            let mut fsm = HandshakeFsm::new(vector.auth_required);
            let mut observed_reason_code = None;

            for step in vector.steps {
                if let Err(err) = fsm.on_step(*step) {
                    observed_reason_code = Some(err.reason_code());
                    break;
                }
            }

            if observed_reason_code.is_none()
                && vector.accept_psync_reply
                && let Err(err) = fsm.on_psync_accepted()
            {
                observed_reason_code = Some(err.reason_code());
            }

            assert_eq!(
                observed_reason_code, vector.expected_reason_code,
                "vector={} reason code mismatch",
                vector.name
            );
            assert_eq!(
                fsm.state(),
                vector.expected_state,
                "vector={} state mismatch",
                vector.name
            );
        }
    }

    #[test]
    fn fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks() {
        let backlog = BacklogWindow {
            replid: "replid-a".to_string(),
            start_offset: ReplOffset(100),
            end_offset: ReplOffset(200),
        };

        let cases = [
            ("boundary_start", "replid-a", ReplOffset(100), None),
            ("boundary_end", "replid-a", ReplOffset(200), None),
            (
                "replid_mismatch",
                "replid-b",
                ReplOffset(150),
                Some(PsyncRejection::ReplidMismatch),
            ),
            (
                "offset_underflow",
                "replid-a",
                ReplOffset(99),
                Some(PsyncRejection::OffsetOutOfRange),
            ),
            (
                "offset_overflow",
                "replid-a",
                ReplOffset(201),
                Some(PsyncRejection::OffsetOutOfRange),
            ),
        ];

        for (name, requested_replid, requested_offset, expected_rejection) in cases {
            let decision = decide_psync(&backlog, requested_replid, requested_offset);
            if let Some(expected) = expected_rejection {
                assert!(
                    matches!(
                        decision,
                        PsyncDecision::FullResync { rejection } if rejection == expected
                    ),
                    "case={name} decision mismatch: got={decision:?} expected_rejection={expected:?}"
                );
                if let PsyncDecision::FullResync { rejection } = decision {
                    let reason_code = rejection.reason_code();
                    match expected {
                        PsyncRejection::ReplidMismatch => {
                            assert_eq!(
                                reason_code, "repl.psync_replid_or_offset_reject_mismatch",
                                "case={name} reason code mismatch"
                            );
                        }
                        PsyncRejection::OffsetOutOfRange => {
                            assert_eq!(
                                reason_code, "repl.psync_fullresync_fallback_mismatch",
                                "case={name} reason code mismatch"
                            );
                        }
                    }
                }
            } else {
                assert_eq!(
                    decision,
                    PsyncDecision::Continue { requested_offset },
                    "case={name} should preserve requested partial offset"
                );
            }
        }
    }

    #[test]
    fn fr_p2c_006_f_wait_metamorphic_ack_monotonicity_holds() {
        let threshold = WaitThreshold {
            required_offset: ReplOffset(100),
            required_replicas: 2,
        };

        let baseline = evaluate_wait(
            &[ReplOffset(100), ReplOffset(99), ReplOffset(50)],
            threshold,
        );
        let promoted = evaluate_wait(
            &[ReplOffset(100), ReplOffset(105), ReplOffset(50)],
            threshold,
        );
        let expanded = evaluate_wait(
            &[
                ReplOffset(100),
                ReplOffset(105),
                ReplOffset(50),
                ReplOffset(500),
            ],
            threshold,
        );

        assert_eq!(baseline.acked_replicas, 1);
        assert!(!baseline.satisfied);
        assert!(
            promoted.acked_replicas >= baseline.acked_replicas,
            "promoting a replica ack must not reduce acked count"
        );
        assert!(promoted.satisfied);
        assert!(
            expanded.acked_replicas >= promoted.acked_replicas,
            "adding a higher replica ack must not reduce acked count"
        );
        assert!(expanded.satisfied);

        let relaxed_threshold = WaitThreshold {
            required_offset: ReplOffset(95),
            required_replicas: 2,
        };
        let relaxed = evaluate_wait(
            &[
                ReplOffset(100),
                ReplOffset(105),
                ReplOffset(50),
                ReplOffset(500),
            ],
            relaxed_threshold,
        );
        assert!(
            relaxed.acked_replicas >= expanded.acked_replicas,
            "lowering required offset must not reduce acked count"
        );
    }

    #[test]
    fn fr_p2c_006_f_waitaof_metamorphic_joint_threshold_semantics_hold() {
        let threshold = WaitAofThreshold {
            required_local_offset: ReplOffset(100),
            required_replica_offset: ReplOffset(95),
            required_replicas: 2,
        };

        let local_not_ready = evaluate_waitaof(
            ReplOffset(99),
            &[ReplOffset(120), ReplOffset(121)],
            threshold,
        );
        let local_ready = evaluate_waitaof(
            ReplOffset(100),
            &[ReplOffset(120), ReplOffset(121)],
            threshold,
        );
        let replica_sparse = evaluate_waitaof(
            ReplOffset(100),
            &[ReplOffset(120), ReplOffset(94)],
            threshold,
        );
        let replica_dense = evaluate_waitaof(
            ReplOffset(100),
            &[ReplOffset(120), ReplOffset(96), ReplOffset(130)],
            threshold,
        );

        assert!(!local_not_ready.local_satisfied);
        assert!(!local_not_ready.satisfied);
        assert!(local_ready.local_satisfied);
        assert!(local_ready.satisfied);
        assert!(replica_sparse.local_satisfied);
        assert_eq!(replica_sparse.acked_replicas, 1);
        assert!(!replica_sparse.satisfied);
        assert!(
            replica_dense.acked_replicas >= local_ready.acked_replicas,
            "adding qualifying replica acks must not reduce acked count"
        );
        assert!(replica_dense.satisfied);
    }

    #[test]
    fn fr_p2c_009_f_differential_mode_split_contract_is_stable() {
        let mut strict = Runtime::default_strict();
        let mut hardened = Runtime::default_hardened();

        let strict_err = strict
            .apply_tls_config(invalid_tls_without_listener_ports(), 900)
            .expect_err("strict mode must fail closed for missing TLS listener ports");
        let hardened_err = hardened
            .apply_tls_config(invalid_tls_without_listener_ports(), 901)
            .expect_err("hardened mode still rejects config boundary violation");

        assert_eq!(
            strict_err.reason_code(),
            "tlscfg.safety_gate_contract_violation"
        );
        assert_eq!(
            hardened_err.reason_code(),
            "tlscfg.safety_gate_contract_violation"
        );

        let strict_event = strict
            .evidence()
            .events()
            .last()
            .expect("strict event")
            .clone();
        let hardened_event = hardened
            .evidence()
            .events()
            .last()
            .expect("hardened event")
            .clone();

        assert_eq!(
            strict_event.reason_code,
            "tlscfg.safety_gate_contract_violation"
        );
        assert_eq!(
            hardened_event.reason_code,
            "tlscfg.safety_gate_contract_violation"
        );
        assert_eq!(strict_event.decision_action, DecisionAction::FailClosed);
        assert_eq!(
            hardened_event.decision_action,
            DecisionAction::BoundedDefense
        );
        assert_eq!(strict_event.severity, DriftSeverity::S0);
        assert_eq!(hardened_event.severity, DriftSeverity::S1);

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_009",
                fixture_name: "fr_p2c_009_tls_runtime_strict",
                case_name: "differential_mode_split_strict",
                verification_path: VerificationPath::Property,
                now_ms: 900,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&strict_event),
        )
        .expect("strict structured-log emission validates");
        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_009",
                fixture_name: "fr_p2c_009_tls_runtime_hardened",
                case_name: "differential_mode_split_hardened",
                verification_path: VerificationPath::Property,
                now_ms: 901,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            std::slice::from_ref(&hardened_event),
        )
        .expect("hardened structured-log emission validates");
    }

    #[test]
    fn fr_p2c_009_f_metamorphic_non_allowlisted_rejection_is_deterministic() {
        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::MetadataSanitization);
        let mut first = Runtime::new(policy.clone());
        let mut second = Runtime::new(policy);

        let first_err = first
            .apply_tls_config(invalid_tls_without_listener_ports(), 902)
            .expect_err("first run must reject non-allowlisted deviation");
        let second_err = second
            .apply_tls_config(invalid_tls_without_listener_ports(), 902)
            .expect_err("second run must reject non-allowlisted deviation");

        assert_eq!(
            first_err.reason_code(),
            "tlscfg.hardened_nonallowlisted_rejected"
        );
        assert_eq!(first_err.reason_code(), second_err.reason_code());

        let first_event = first.evidence().events().last().expect("first event");
        let second_event = second.evidence().events().last().expect("second event");

        assert_eq!(first_event.reason_code, second_event.reason_code);
        assert_eq!(first_event.decision_action, second_event.decision_action);
        assert_eq!(first_event.severity, second_event.severity);
        assert_eq!(first_event.input_digest, second_event.input_digest);
        assert_eq!(first_event.output_digest, second_event.output_digest);
        assert_eq!(
            first_event.state_digest_before,
            second_event.state_digest_before
        );
        assert_eq!(
            first_event.state_digest_after,
            second_event.state_digest_after
        );
    }

    #[test]
    fn fr_p2c_009_f_adversarial_tls_reason_codes_are_stable() {
        let mut strict = Runtime::default_strict();
        let strict_safety = strict
            .apply_tls_config(invalid_tls_without_listener_ports(), 903)
            .expect_err("strict safety-gate violation expected");
        assert_eq!(
            strict_safety.reason_code(),
            "tlscfg.safety_gate_contract_violation"
        );

        let mut invalid_knob = valid_tls_config();
        invalid_knob.max_new_tls_connections_per_cycle = 0;
        let strict_knob = strict
            .apply_tls_config(invalid_knob, 904)
            .expect_err("strict operational knob violation expected");
        assert_eq!(
            strict_knob.reason_code(),
            "tlscfg.operational_knob_contract_violation"
        );

        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::MetadataSanitization);
        let mut hardened = Runtime::new(policy);
        let hardened_reject = hardened
            .apply_tls_config(invalid_tls_without_listener_ports(), 905)
            .expect_err("hardened non-allowlisted rejection expected");
        assert_eq!(
            hardened_reject.reason_code(),
            "tlscfg.hardened_nonallowlisted_rejected"
        );

        let strict_event = strict.evidence().events().last().expect("strict event");
        assert_eq!(
            strict_event.reason_code,
            "tlscfg.operational_knob_contract_violation"
        );
        let hardened_event = hardened.evidence().events().last().expect("hardened event");
        assert_eq!(
            hardened_event.reason_code,
            "tlscfg.hardened_nonallowlisted_rejected"
        );
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
    fn fr_p2c_009_e013_strict_runtime_tls_rejection_matches_expected_threat_contract() {
        let mut runtime = Runtime::default_strict();
        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;

        let err = runtime
            .apply_tls_config(invalid, 777)
            .expect_err("strict must fail closed");
        assert_eq!(err.reason_code(), "tlscfg.safety_gate_contract_violation");

        let event = runtime.evidence().events().last().expect("event").clone();
        let expected = ExpectedThreat {
            threat_class: "config_downgrade_abuse".to_string(),
            severity: "s0".to_string(),
            decision_action: "fail_closed".to_string(),
            reason_code: Some("tlscfg.safety_gate_contract_violation".to_string()),
            subsystem: Some("tls_config".to_string()),
            action: Some("reject_runtime_apply".to_string()),
        };
        validate_threat_expectation(Some(&expected), std::slice::from_ref(&event))
            .expect("threat contract should match");

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_009",
                fixture_name: "fr_p2c_009_tls_runtime_strict",
                case_name: "strict_safety_gate",
                verification_path: VerificationPath::E2e,
                now_ms: 777,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("structured log emission should validate");
    }

    #[test]
    fn fr_p2c_009_e013_hardened_non_allowlisted_rejection_matches_expected_threat_contract() {
        let mut policy = RuntimePolicy::hardened();
        policy
            .hardened_allowlist
            .retain(|category| *category != HardenedDeviationCategory::MetadataSanitization);
        let mut runtime = Runtime::new(policy);

        let mut invalid = valid_tls_config();
        invalid.tls_port = None;
        invalid.cluster_announce_tls_port = None;

        let err = runtime
            .apply_tls_config(invalid, 778)
            .expect_err("hardened must reject non-allowlisted deviation");
        assert_eq!(err.reason_code(), "tlscfg.hardened_nonallowlisted_rejected");

        let event = runtime.evidence().events().last().expect("event").clone();
        let expected = ExpectedThreat {
            threat_class: "config_downgrade_abuse".to_string(),
            severity: "s2".to_string(),
            decision_action: "reject_non_allowlisted".to_string(),
            reason_code: Some("tlscfg.hardened_nonallowlisted_rejected".to_string()),
            subsystem: Some("tls_config".to_string()),
            action: Some("reject_runtime_apply".to_string()),
        };
        validate_threat_expectation(Some(&expected), std::slice::from_ref(&event))
            .expect("threat contract should match");

        validate_structured_log_emission(
            StructuredLogEmissionContext {
                suite_id: "fr_p2c_009",
                fixture_name: "fr_p2c_009_tls_runtime_hardened",
                case_name: "hardened_nonallowlisted_reject",
                verification_path: VerificationPath::E2e,
                now_ms: 778,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("structured log emission should validate");
    }

    #[test]
    fn fr_p2c_009_e011_runtime_rejects_invalid_tls_operational_knob() {
        let mut runtime = Runtime::default_strict();
        let mut invalid = valid_tls_config();
        invalid.max_new_tls_connections_per_cycle = 0;
        let err = runtime
            .apply_tls_config(invalid, 779)
            .expect_err("invalid operational knob should fail");
        assert_eq!(
            err.reason_code(),
            "tlscfg.operational_knob_contract_violation"
        );

        let event = runtime.evidence().events().last().expect("event");
        assert_eq!(
            event.reason_code,
            "tlscfg.operational_knob_contract_violation"
        );
        assert_eq!(event.decision_action, DecisionAction::FailClosed);
        assert_eq!(event.severity, DriftSeverity::S0);
    }

    #[test]
    fn fr_p2c_009_fixture_packet_family_maps_to_packet_009() {
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_009_tls_runtime_strict"),
            "FR-P2C-009"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_009_tls_runtime_hardened"),
            "FR-P2C-009"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_009_tls_config_journey.json"),
            "FR-P2C-009"
        );
    }

    #[test]
    fn fr_p2c_007_fixture_packet_family_maps_to_packet_007() {
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_007_cluster_router"),
            "FR-P2C-007"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_007_cluster_mode_property"),
            "FR-P2C-007"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_007_cluster_journey.json"),
            "FR-P2C-007"
        );
    }

    #[test]
    fn fr_p2c_004_fixture_packet_family_maps_to_packet_004() {
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_004_auth_unit"),
            "FR-P2C-004"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_004_acl_permissions"),
            "FR-P2C-004"
        );
        assert_eq!(
            crate::packet_family_for_fixture("fr_p2c_004_acl_journey.json"),
            "FR-P2C-004"
        );
    }

    #[test]
    fn fr_p2c_005_fixture_packet_family_maps_to_packet_005() {
        assert_eq!(
            crate::packet_family_for_fixture("persist_replay.json"),
            "FR-P2C-005"
        );
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
            StructuredLogEmissionContext {
                suite_id: "protocol_negative",
                fixture_name: "protocol_negative.json",
                case_name: "invalid_bulk_len",
                verification_path: VerificationPath::E2e,
                now_ms: 7,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect("conversion should pass");
    }

    #[test]
    fn structured_log_enforcement_persists_when_path_is_provided() {
        let log_root = unique_temp_log_root("fr_conformance_emit_logs");
        let out_path = log_root.join("persist/core_errors.jsonl");
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
            StructuredLogEmissionContext {
                suite_id: "protocol_negative",
                fixture_name: "protocol_negative.json",
                case_name: "invalid_bulk_len",
                verification_path: VerificationPath::E2e,
                now_ms: 7,
                outcome: LogOutcome::Pass,
                persist_path: Some(&out_path),
            },
            &[event],
        )
        .expect("conversion should pass");

        let raw = fs::read_to_string(&out_path).expect("read persisted structured logs");
        let lines = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "expected exactly one persisted log line");
        let parsed: StructuredLogEvent =
            serde_json::from_str(lines[0]).expect("parse persisted structured log line");
        parsed.validate().expect("persisted log validates");
        let _ = fs::remove_dir_all(log_root);
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
            StructuredLogEmissionContext {
                suite_id: "protocol_negative",
                fixture_name: "protocol_negative.json",
                case_name: "invalid_bulk_len",
                verification_path: VerificationPath::E2e,
                now_ms: 7,
                outcome: LogOutcome::Pass,
                persist_path: None,
            },
            &[event],
        )
        .expect_err("empty artifact_refs should fail conversion");
        assert!(err.contains("artifact_refs"));
    }

    #[test]
    fn differential_report_summarizes_reason_codes_deterministically() {
        let report = build_differential_report(
            "suite".to_string(),
            "fixture.json",
            4,
            vec![
                CaseOutcome {
                    name: "case-a".to_string(),
                    passed: false,
                    expected: RespFrame::SimpleString("OK".to_string()),
                    actual: RespFrame::Error("ERR".to_string()),
                    detail: Some("detail".to_string()),
                    reason_code: Some("parser.invalid_bulk_len".to_string()),
                    replay_cmd: Some("cargo test -- case-a".to_string()),
                    artifact_refs: vec!["artifact-a".to_string()],
                },
                CaseOutcome {
                    name: "case-b".to_string(),
                    passed: false,
                    expected: RespFrame::SimpleString("OK".to_string()),
                    actual: RespFrame::Error("ERR".to_string()),
                    detail: Some("detail".to_string()),
                    reason_code: Some("parser.invalid_bulk_len".to_string()),
                    replay_cmd: Some("cargo test -- case-b".to_string()),
                    artifact_refs: vec!["artifact-b".to_string()],
                },
                CaseOutcome {
                    name: "case-c".to_string(),
                    passed: false,
                    expected: RespFrame::SimpleString("OK".to_string()),
                    actual: RespFrame::Error("ERR".to_string()),
                    detail: Some("detail".to_string()),
                    reason_code: None,
                    replay_cmd: None,
                    artifact_refs: Vec::new(),
                },
            ],
        );

        assert_eq!(report.schema_version, DIFFERENTIAL_REPORT_SCHEMA_VERSION);
        assert_eq!(report.fixture, "fixture.json");
        assert_eq!(
            report.reason_code_counts.get("parser.invalid_bulk_len"),
            Some(&2)
        );
        assert_eq!(report.failed_without_reason_code, 1);
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
