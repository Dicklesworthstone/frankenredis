use std::fs;
use std::path::PathBuf;

use fr_conformance::log_contract::{
    PACKET_FAMILIES, StructuredLogEvent, VerificationPath, golden_packet_logs,
};

fn render_jsonl(events: &[StructuredLogEvent]) -> String {
    let mut rendered = String::new();
    for event in events {
        rendered.push_str(&event.to_json_line().expect("serialize golden"));
        rendered.push('\n');
    }
    rendered
}

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

        let expected = render_jsonl(&golden_packet_logs(packet_id).expect("golden packet logs"));
        let raw = fs::read_to_string(&path).expect("read golden log");
        assert_eq!(
            raw,
            expected,
            "checked-in packet golden drifted for {}",
            path.display()
        );
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

fn verify_committed_sidecar_and_gate_in_tempdir(
    artifact_path: &std::path::Path,
    temp_dir: &std::path::Path,
    now_unix_ms: u64,
) {
    let (envelope, symbols) = fr_fec::read_sidecar(artifact_path).unwrap_or_else(|err| {
        panic!(
            "read committed sidecar for {}: {err}",
            artifact_path.display()
        )
    });
    let outcome = fr_fec::scrub(&envelope, &symbols, now_unix_ms);
    assert!(
        matches!(outcome, fr_fec::ScrubOutcome::Clean),
        "committed sidecar scrub must be Clean for {}: {outcome:?}",
        artifact_path.display()
    );

    let file_name = artifact_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("artifact file name");
    let temp_artifact = temp_dir.join(file_name);
    let (src_env, src_sym) = fr_fec::sidecar_paths(artifact_path);
    let (dst_env, dst_sym) = fr_fec::sidecar_paths(&temp_artifact);
    fs::copy(artifact_path, &temp_artifact).expect("copy artifact to temp_dir");
    fs::copy(&src_env, &dst_env).expect("copy envelope to temp_dir");
    fs::copy(&src_sym, &dst_sym).expect("copy symbols to temp_dir");

    let gate_res = fr_fec::verify_sidecar_gate(&temp_artifact, now_unix_ms);
    assert!(
        gate_res.is_ok(),
        "gate check for {}: {:?}",
        artifact_path.display(),
        gate_res.err()
    );
}

#[test]
fn conformance_fixtures_and_goldens_raptorq_sidecars_scrub_clean() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixtures_root = repo_root.join("crates/fr-conformance/fixtures");
    let log_root = fixtures_root.join("log_contract_v1");
    let temp_dir = std::env::temp_dir().join(format!(
        "fr_conformance_sidecar_gate_{}",
        std::process::id()
    ));
    fs::create_dir_all(&temp_dir).expect("create temp dir for sidecar gate");

    for packet_id in PACKET_FAMILIES {
        let golden_path = log_root.join(format!("{packet_id}.golden.jsonl"));
        assert!(golden_path.exists());
        verify_committed_sidecar_and_gate_in_tempdir(&golden_path, &temp_dir, 1_788_700_000_000);
    }

    let entries = fs::read_dir(&fixtures_root).expect("read fixtures dir");
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let path = entry.path();
        if path.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".json") && !name.ends_with(".envelope.json") {
                verify_committed_sidecar_and_gate_in_tempdir(&path, &temp_dir, 1_788_700_000_000);
            }
        }
    }
}
