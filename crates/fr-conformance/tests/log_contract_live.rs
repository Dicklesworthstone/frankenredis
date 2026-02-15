#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fr_conformance::log_contract::{
    LogMode, LogOutcome, STRUCTURED_LOG_SCHEMA_VERSION, StructuredLogEvent, VerificationPath,
    append_structured_log_jsonl, live_log_output_path,
};

fn sample_event(case_id: &str, outcome: LogOutcome) -> StructuredLogEvent {
    StructuredLogEvent {
        schema_version: STRUCTURED_LOG_SCHEMA_VERSION.to_string(),
        ts_utc: "2026-02-15T00:00:00Z".to_string(),
        suite_id: "live_redis_diff::core_errors".to_string(),
        test_or_scenario_id: case_id.to_string(),
        packet_id: "FR-P2C-003".to_string(),
        mode: LogMode::Strict,
        verification_path: VerificationPath::E2e,
        seed: 7,
        input_digest: format!("input_{case_id}"),
        output_digest: format!("output_{case_id}"),
        duration_ms: 1,
        outcome,
        reason_code: format!("reason_{case_id}"),
        replay_cmd: "rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_003"
            .to_string(),
        artifact_refs: vec!["TEST_LOG_SCHEMA_V1.md".to_string()],
        fixture_id: Some("core_errors.json".to_string()),
        env_ref: Some("crates/fr-conformance/fixtures/log_contract_v1/env.json".to_string()),
    }
}

#[test]
fn live_path_builder_is_stable() {
    let root = Path::new("artifacts/log_contract/live");
    let path = live_log_output_path(root, "live_redis_diff::core/errors", "core_errors.json");
    assert_eq!(
        path,
        root.join("live_redis_diff__core_errors")
            .join("core_errors.jsonl")
    );
}

#[test]
fn append_jsonl_creates_and_appends_lines() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock moved backwards")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("fr_conformance_log_contract_live_{unique}"));
    let output = root.join("suite/core_errors.jsonl");

    append_structured_log_jsonl(&output, &[sample_event("case_1", LogOutcome::Pass)])
        .expect("append first");
    append_structured_log_jsonl(&output, &[sample_event("case_2", LogOutcome::Fail)])
        .expect("append second");

    let raw = fs::read_to_string(&output).expect("read output");
    let lines = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "expected two jsonl records");

    let first: StructuredLogEvent = serde_json::from_str(lines[0]).expect("parse first");
    let second: StructuredLogEvent = serde_json::from_str(lines[1]).expect("parse second");
    assert_eq!(first.test_or_scenario_id, "case_1");
    assert_eq!(second.test_or_scenario_id, "case_2");

    let _ = fs::remove_dir_all(PathBuf::from(&root));
}
