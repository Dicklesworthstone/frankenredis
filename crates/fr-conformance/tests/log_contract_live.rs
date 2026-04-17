#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fr_conformance::log_contract::{
    LIVE_LOG_GOLDEN_FIXTURES, LogOutcome, StructuredLogEvent, append_structured_log_jsonl,
    golden_live_log_events, live_log_golden_file_name, live_log_output_path,
};

fn render_jsonl(events: &[StructuredLogEvent]) -> String {
    let mut rendered = String::new();
    for event in events {
        rendered.push_str(&event.to_json_line().expect("serialize live golden"));
        rendered.push('\n');
    }
    rendered
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
fn live_goldens_exist_and_match_expected_payloads() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let log_root = repo_root.join("crates/fr-conformance/fixtures/log_contract_v1");

    for fixture in LIVE_LOG_GOLDEN_FIXTURES {
        let path = log_root.join(live_log_golden_file_name(fixture));
        assert!(
            path.exists(),
            "missing live golden file for {}: {}",
            fixture.fixture_name,
            path.display()
        );

        let expected = render_jsonl(&golden_live_log_events(fixture).expect("live golden events"));
        let raw = fs::read_to_string(&path).expect("read live golden");
        assert_eq!(
            raw,
            expected,
            "checked-in live golden drifted for {}",
            path.display()
        );

        for line in raw.lines().filter(|line| !line.trim().is_empty()) {
            let event: StructuredLogEvent = serde_json::from_str(line).expect("parse line");
            event.validate().expect("event validates");
        }
    }
}

#[test]
fn append_jsonl_creates_and_appends_lines() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock moved backwards")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("fr_conformance_log_contract_live_{unique}"));
    let output = root.join("suite/core_errors.jsonl");
    let events = golden_live_log_events(LIVE_LOG_GOLDEN_FIXTURES[0]).expect("live golden events");

    append_structured_log_jsonl(&output, &[events[0].clone()]).expect("append first");
    append_structured_log_jsonl(&output, &[events[1].clone()]).expect("append second");

    let raw = fs::read_to_string(&output).expect("read output");
    let lines = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "expected two jsonl records");

    let first: StructuredLogEvent = serde_json::from_str(lines[0]).expect("parse first");
    let second: StructuredLogEvent = serde_json::from_str(lines[1]).expect("parse second");
    assert!(first.test_or_scenario_id.ends_with("::pass"));
    assert_eq!(first.outcome, LogOutcome::Pass);
    assert!(second.test_or_scenario_id.ends_with("::fail"));
    assert_eq!(second.outcome, LogOutcome::Fail);

    let _ = fs::remove_dir_all(PathBuf::from(&root));
}
