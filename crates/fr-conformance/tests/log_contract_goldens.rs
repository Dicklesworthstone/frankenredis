use std::fs;
use std::path::PathBuf;

use fr_conformance::log_contract::{PACKET_FAMILIES, StructuredLogEvent, VerificationPath};

#[test]
fn golden_logs_exist_and_validate() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let log_root = repo_root.join("crates/fr-conformance/fixtures/log_contract_v1");

    for packet_id in PACKET_FAMILIES {
        let path = log_root.join(format!("{packet_id}.golden.jsonl"));
        assert!(
            path.exists(),
            "missing golden file for {packet_id}: {}",
            path.display()
        );

        let raw = fs::read_to_string(&path).expect("read golden log");
        let lines = raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            2,
            "expected two lines (unit + e2e) in {}",
            path.display()
        );

        let mut has_unit = false;
        let mut has_e2e = false;
        for line in lines {
            let event: StructuredLogEvent = serde_json::from_str(line).expect("parse line");
            event.validate().expect("event validates");
            if event.verification_path == VerificationPath::Unit {
                has_unit = true;
            }
            if event.verification_path == VerificationPath::E2e {
                has_e2e = true;
            }
        }

        assert!(has_unit, "expected a unit-path event in {}", path.display());
        assert!(has_e2e, "expected an e2e-path event in {}", path.display());
    }
}
