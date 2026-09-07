//! Tests for fr-fec sidecar filesystem operations, symbol streams, and gate verification.

use fr_fec::{
    ScrubOutcome, deserialize_symbol_stream, read_sidecar, scrub_sidecar, serialize_symbol_stream,
    verify_sidecar_gate, write_sidecar,
};
use std::fs;
use std::path::PathBuf;

const NOW: u64 = 1_788_700_000_000;

fn setup_temp_artifact(name: &str, content: &[u8]) -> PathBuf {
    let tmp_dir = std::env::temp_dir().join(format!("fr_fec_test_{}", std::process::id()));
    let _ = fs::create_dir_all(&tmp_dir);
    let file_path = tmp_dir.join(name);
    fs::write(&file_path, content).expect("write test artifact");
    file_path
}

#[test]
fn sidecar_write_read_scrub_roundtrip() {
    let content = b"{\"benchmark\": \"set\", \"ops_sec\": 123456.78, \"p99_us\": 42.0}\n";
    let artifact = setup_temp_artifact("baseline_test.json", content);

    // 1. Write sidecar
    let (env, env_path, sym_path) =
        write_sidecar(&artifact, "benchmark", 4, 128, NOW).expect("write sidecar");
    assert!(env_path.exists());
    assert!(sym_path.exists());
    assert_eq!(env.raptorq.repair_symbols, 4);

    // 2. Read sidecar
    let (read_env, symbols) = read_sidecar(&artifact).expect("read sidecar");
    assert_eq!(read_env.source_hash, env.source_hash);
    assert_eq!(symbols.len(), (read_env.raptorq.k + 4) as usize);

    // 3. Scrub intact sidecar
    let outcome = scrub_sidecar(&artifact, NOW + 1_000).expect("scrub intact");
    assert!(matches!(outcome, ScrubOutcome::Clean));

    // 4. Verify gate passes
    verify_sidecar_gate(&artifact, NOW + 2_000).expect("gate check passes");

    // 5. Corrupt one symbol in the symbols file
    let mut sym_bytes = fs::read(&sym_path).expect("read symbols");
    // Flip bytes in the middle of a symbol payload
    let mid = sym_bytes.len() / 2;
    sym_bytes[mid] ^= 0xFF;
    fs::write(&sym_path, &sym_bytes).expect("write corrupted symbols");

    // 6. Scrub should detect corruption and recover
    let outcome2 = scrub_sidecar(&artifact, NOW + 3_000).expect("scrub after damage");
    assert!(matches!(outcome2, ScrubOutcome::Recovered { .. }));
    if let ScrubOutcome::Recovered { source, proof } = outcome2 {
        assert_eq!(source, content);
        assert!(proof.reason.contains("hash-damaged") || proof.reason.contains("missing"));
    }

    // 7. Envelope on disk should now record the proof and "recovered" status
    let (recheck_env, _) = read_sidecar(&artifact).expect("read after recovery");
    assert_eq!(recheck_env.scrub.status, "recovered");
    assert_eq!(recheck_env.decode_proofs.len(), 1);

    // 8. Gate still passes because it recovered
    verify_sidecar_gate(&artifact, NOW + 4_000).expect("gate passes after recovery");
}

#[test]
fn symbol_stream_serialization_roundtrip() {
    let content = b"Some test data for symbol stream serialization testing";
    let artifact = setup_temp_artifact("stream_test.bin", content);

    let (_env, _, _) = write_sidecar(&artifact, "test", 4, 32, NOW).expect("write sidecar");
    let (_, symbols) = read_sidecar(&artifact).expect("read sidecar");

    let stream = serialize_symbol_stream(&symbols);
    let deserialized = deserialize_symbol_stream(&stream).expect("deserialize stream");

    assert_eq!(symbols.len(), deserialized.len());
    for (orig, deser) in symbols.iter().zip(deserialized.iter()) {
        assert_eq!(orig.serialize(), deser.serialize());
    }
}

#[test]
fn reproducibility_ledgers_raptorq_sidecars_scrub_clean() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ledgers = [
        "artifacts/evidence/LEDGER_CONTRACT.md",
        "artifacts/optimization/campaign-20260725-cc/ledger_resurrection.json",
        "artifacts/optimization/cod-pass49-borrowed-set-v2-20260606T2353Z/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/cod-perf-20260604-8yfmt-zero-copy-argv/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/cod-perf-20260605-ds9o7/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/frankenredis-9mh3o/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/frankenredis-incr-floor/20260724T0100Z/PENDING_LEDGER_ENTRY.md",
        "artifacts/optimization/frankenredis-ptqye/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/frankenredis-vlis9/20260722T1810Z/PENDING_LEDGER_ENTRIES.md",
        "artifacts/optimization/icywolf-perf-20260603-pass23-lazy-entry-digest/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/icywolf-perf-20260603-pass24-write-interest/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/icywolf-perf-20260603-pass25-rss-sampling/ISOMORPHISM_PROOF.md",
        "artifacts/optimization/ISOMORPHISM_PROOF_ROUND1.md",
        "artifacts/optimization/ISOMORPHISM_PROOF_ROUND2.md",
        "artifacts/optimization/phase2c-gate/decision_ledger_sample.txt",
        "artifacts/optimization/throughput-gap/ISOMORPHISM_PROOF_LAZY_DIGEST.md",
    ];

    for rel in ledgers {
        let path = repo_root.join(rel);
        assert!(path.exists(), "artifact missing: {}", path.display());
        let gate_res = verify_sidecar_gate(&path, NOW);
        assert!(gate_res.is_ok(), "gate check for {}", path.display());
    }
}

#[test]
fn release_grade_state_artifacts_raptorq_sidecars_scrub_clean() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let state_artifacts = [
        "crates/fr-persist/tests/golden/stream_type21_vendored_redis_724.dump",
        "artifacts/optimization/codex-fv18u-rdb-capacity-20260605T1620Z/baseline-golden.rdb",
        "artifacts/optimization/codex-fv18u-rdb-capacity-20260605T1620Z/candidate-golden.rdb",
        "artifacts/optimization/codex-pass15-rdb-inline-writer-20260605T1645Z/baseline-golden.rdb",
        "artifacts/optimization/codex-pass15-rdb-inline-writer-20260605T1645Z/candidate-golden.rdb",
        "artifacts/optimization/codex-pass15-rdb-inline-writer-20260605T1645Z/candidate2-golden.rdb",
        "artifacts/optimization/codex-pass17-rdb-short-record-20260605T2050Z/baseline-golden.rdb",
        "artifacts/optimization/codex-pass17-rdb-short-record-20260605T2050Z/candidate-golden.rdb",
        "artifacts/optimization/codex-pass18-rdb-sorted-input-20260605T2100Z/baseline-sorted-golden.rdb",
        "artifacts/optimization/codex-pass18-rdb-sorted-input-20260605T2100Z/baseline-unsorted-multidb-golden.rdb",
        "artifacts/optimization/codex-pass18-rdb-sorted-input-20260605T2100Z/candidate-sorted-golden.rdb",
        "artifacts/optimization/codex-pass18-rdb-sorted-input-20260605T2100Z/candidate-unsorted-multidb-golden.rdb",
        "artifacts/optimization/coralox-47bzu/pass204/baseline-golden-restored.dump",
        "artifacts/optimization/coralox-47bzu/pass204/baseline-golden-source.dump",
        "artifacts/optimization/coralox-47bzu/pass204/candidate-golden-restored.dump",
        "artifacts/optimization/coralox-47bzu/pass204/candidate-golden-source.dump",
        "artifacts/optimization/coralox-qbs5q/pass202/baseline-golden-restored.dump",
        "artifacts/optimization/coralox-qbs5q/pass202/baseline-golden-source.dump",
        "artifacts/optimization/coralox-qbs5q/pass202/candidate-golden-restored.dump",
        "artifacts/optimization/coralox-qbs5q/pass202/candidate-golden-source.dump",
        "artifacts/optimization/icywolf-perf-20260605-pass49-rdb-streaming-crc/baseline-golden.rdb",
        "artifacts/optimization/icywolf-perf-20260605-pass49-rdb-streaming-crc/candidate-golden.rdb",
    ];

    for rel in state_artifacts {
        let path = repo_root.join(rel);
        assert!(path.exists(), "artifact missing: {}", path.display());
        let gate_res = verify_sidecar_gate(&path, NOW);
        assert!(gate_res.is_ok(), "gate check for {}", path.display());
    }
}
