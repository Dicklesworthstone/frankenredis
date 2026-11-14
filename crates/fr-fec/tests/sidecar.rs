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
fn sidecar_detects_primary_tampering_and_preserves_corrupt_backup() {
    let content = b"{\"snapshot\": \"primary_v1\", \"checksum\": 999}\n";
    let artifact = setup_temp_artifact("primary_tamper_test.json", content);

    let (env, env_path, sym_path) =
        write_sidecar(&artifact, "snapshot", 4, 64, NOW).expect("write sidecar");
    assert!(env_path.exists());
    assert!(sym_path.exists());

    // Verify backward-compatible deserialization when source_hash is omitted from JSON.
    let legacy_json = serde_json::json!({
        "schema_version": "frankenredis_fec_envelope/v1",
        "artifact_id": "legacy.rdb",
        "artifact_type": "snapshot",
        "padding": 22,
        "symbol_size": 64,
        "oti": [0, 0, 0, 42, 0, 0, 0, 64, 1, 0, 1, 8],
        "raptorq": {
            "k": 1,
            "repair_symbols": 4,
            "overhead_ratio": 4.0,
            "symbol_hashes": []
        },
        "scrub": {
            "last_ok_unix_ms": NOW,
            "status": "ok"
        },
        "decode_proofs": []
    })
    .to_string();
    let legacy_env: fr_fec::FecSidecarEnvelope = fr_fec::envelope_from_json(&legacy_json)
        .expect("deserialize legacy envelope without source_hash");
    assert!(legacy_env.source_hash.is_empty());

    // Tamper with the primary file on disk while leaving .symbols completely intact.
    let tampered = b"{\"snapshot\": \"TAMPERED!\", \"checksum\": 000}\n";
    fs::write(&artifact, tampered).expect("write tampered primary");

    let corrupt_path = fr_fec::corrupt_backup_path(&artifact);
    let _ = fs::remove_file(&corrupt_path);

    let outcome = scrub_sidecar(&artifact, NOW + 5_000).expect("scrub tampered primary");
    match outcome {
        ScrubOutcome::Recovered { source, proof } => {
            assert_eq!(source, content);
            assert!(
                proof.reason.contains("primary artifact hash mismatch"),
                "unexpected proof reason: {}",
                proof.reason
            );
        }
        other => panic!("expected Recovered outcome, got {other:?}"),
    }

    // Primary file on disk must be restored to original content.
    assert_eq!(fs::read(&artifact).expect("read restored primary"), content);
    // Corrupt primary must be preserved at <path>.corrupt.
    assert!(corrupt_path.exists(), ".corrupt backup must be preserved");
    assert_eq!(
        fs::read(&corrupt_path).expect("read .corrupt backup"),
        tampered
    );
    // Envelope source_hash is still bound to original content.
    let (recheck_env, _) = read_sidecar(&artifact).expect("read after primary recovery");
    assert_eq!(recheck_env.source_hash, env.source_hash);

    let _ = fs::remove_file(&artifact);
    let _ = fs::remove_file(&corrupt_path);
    let _ = fs::remove_file(&env_path);
    let _ = fs::remove_file(&sym_path);
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

fn verify_tracked_artifact_sidecar_clean(path: &std::path::Path, rel: &str) {
    let (env_path, sym_path) = fr_fec::sidecar_paths(path);
    if rel.starts_with("artifacts/") && (!path.exists() || !env_path.exists() || !sym_path.exists())
    {
        // `.rchignore` excludes `artifacts/` when syncing to remote build workers.
        return;
    }
    assert!(path.exists(), "artifact missing: {}", path.display());
    let (envelope, symbols) =
        read_sidecar(path).unwrap_or_else(|err| panic!("read sidecar {}: {err}", path.display()));
    let outcome = fr_fec::scrub(&envelope, &symbols, NOW);
    assert!(
        matches!(outcome, ScrubOutcome::Clean),
        "expected Clean scrub for {}, got {outcome:?}",
        path.display()
    );
    let disk_bytes = fs::read(path).expect("read primary artifact");
    assert_eq!(
        fr_fec::sha256_hex(&disk_bytes),
        envelope.source_hash,
        "primary sha256 mismatch for {}",
        path.display()
    );
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
        verify_tracked_artifact_sidecar_clean(&path, rel);
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
        verify_tracked_artifact_sidecar_clean(&path, rel);
    }
}

#[test]
fn short_symbol_payload_ids_return_errors() {
    for len in 0..4 {
        assert!(
            fr_fec::deserialize_symbol(&[0_u8; 4][..len]).is_err(),
            "{len}-byte packet must not reach RaptorQ's payload ID parser"
        );
    }
}

#[test]
fn malformed_symbol_stream_counts_lengths_and_trailing_bytes_return_errors() {
    assert!(deserialize_symbol_stream(&u32::MAX.to_le_bytes()).is_err());
    assert!(
        deserialize_symbol_stream(&0_u32.to_le_bytes())
            .expect("empty stream")
            .is_empty()
    );

    let mut short_packet = 1_u32.to_le_bytes().to_vec();
    short_packet.extend_from_slice(&3_u32.to_le_bytes());
    short_packet.extend_from_slice(&[0; 3]);
    assert!(deserialize_symbol_stream(&short_packet).is_err());

    let mut invalid_length = 1_u32.to_le_bytes().to_vec();
    invalid_length.extend_from_slice(&u32::MAX.to_le_bytes());
    invalid_length.extend_from_slice(&[0; 4]);
    assert!(deserialize_symbol_stream(&invalid_length).is_err());

    let encoded = fr_fec::encode_artifact("stream", "fixture", b"stream validation", 4, 64, NOW)
        .expect("encode");
    let original = serialize_symbol_stream(&encoded.symbols);
    assert_eq!(
        deserialize_symbol_stream(&original).expect("valid stream"),
        encoded.symbols
    );
    let mut trailing = original.clone();
    trailing.push(0);
    assert!(deserialize_symbol_stream(&trailing).is_err());

    let mut missing_packet = original;
    let claimed_count = u32::try_from(encoded.symbols.len() + 1).expect("packet count");
    missing_packet[..4].copy_from_slice(&claimed_count.to_le_bytes());
    assert!(deserialize_symbol_stream(&missing_packet).is_err());
}

fn sidecar_bytes(artifact: &std::path::Path) -> (Vec<u8>, Vec<u8>) {
    let (envelope_path, symbols_path) = fr_fec::sidecar_paths(artifact);
    (
        fs::read(envelope_path).expect("read envelope bytes"),
        fs::read(symbols_path).expect("read symbol bytes"),
    )
}

#[test]
fn gate_verification_is_read_only_for_intact_and_recoverable_sidecars() {
    let content = b"gate verification must preserve the artifact and its evidence";
    for legacy in [false, true] {
        for missing_source in [false, true] {
            let name = format!("gate_read_only_{legacy}_{missing_source}.bin");
            let artifact = setup_temp_artifact(&name, content);
            let (mut envelope, envelope_path, symbols_path) =
                write_sidecar(&artifact, "test", 4, 128, NOW).expect("write sidecar");
            if legacy {
                envelope.source_hash.clear();
                fs::write(&envelope_path, fr_fec::envelope_to_json(&envelope))
                    .expect("write legacy envelope");
            }
            if missing_source {
                let (_, mut symbols) = read_sidecar(&artifact).expect("read symbols");
                symbols.remove(0);
                fs::write(&symbols_path, serialize_symbol_stream(&symbols))
                    .expect("write surviving repair symbols");
            }
            let before = sidecar_bytes(&artifact);

            verify_sidecar_gate(&artifact, NOW + 1_000).expect("verified source matches primary");

            assert_eq!(fs::read(&artifact).expect("read primary"), content);
            assert_eq!(sidecar_bytes(&artifact), before, "verification wrote evidence");
        }
    }
}

#[test]
fn gate_rejects_stale_sidecars_without_replacing_newer_primary_or_backup() {
    let old_content = b"original artifact generation";
    let new_content = b"newer committed artifact generation";
    for legacy in [false, true] {
        for missing_source in [false, true] {
            let name = format!("gate_stale_{legacy}_{missing_source}.bin");
            let artifact = setup_temp_artifact(&name, old_content);
            let (mut envelope, envelope_path, symbols_path) =
                write_sidecar(&artifact, "test", 4, 128, NOW).expect("write sidecar");
            if legacy {
                envelope.source_hash.clear();
                fs::write(&envelope_path, fr_fec::envelope_to_json(&envelope))
                    .expect("write legacy envelope");
            }
            if missing_source {
                let (_, mut symbols) = read_sidecar(&artifact).expect("read symbols");
                symbols.remove(0);
                fs::write(&symbols_path, serialize_symbol_stream(&symbols))
                    .expect("write surviving repair symbols");
            }
            fs::write(&artifact, new_content).expect("commit newer primary");
            let backup = fr_fec::corrupt_backup_path(&artifact);
            fs::write(&backup, b"previous forensic backup").expect("write existing backup");
            let before = sidecar_bytes(&artifact);

            let error = verify_sidecar_gate(&artifact, NOW + 1_000).unwrap_err();

            assert!(error.contains("primary artifact does not match"), "{error}");
            assert_eq!(fs::read(&artifact).expect("read newer primary"), new_content);
            assert_eq!(
                fs::read(&backup).expect("read original forensic backup"),
                b"previous forensic backup"
            );
            assert_eq!(sidecar_bytes(&artifact), before);
        }
    }
}

#[test]
fn gate_rejects_missing_and_unreadable_primaries_without_recreating_them() {
    for unreadable in [false, true] {
        for missing_source in [false, true] {
            let name = format!("gate_absent_{unreadable}_{missing_source}.bin");
            let artifact = setup_temp_artifact(&name, b"must not recreate this primary");
            let (_, _, symbols_path) =
                write_sidecar(&artifact, "test", 4, 128, NOW).expect("write sidecar");
            if missing_source {
                let (_, mut symbols) = read_sidecar(&artifact).expect("read symbols");
                symbols.remove(0);
                fs::write(&symbols_path, serialize_symbol_stream(&symbols))
                    .expect("write surviving repair symbols");
            }
            let saved_primary = artifact.with_extension("saved");
            fs::rename(&artifact, &saved_primary).expect("move primary out of the way");
            if unreadable {
                fs::create_dir(&artifact).expect("replace primary with a directory");
            }
            let before = sidecar_bytes(&artifact);

            let error = verify_sidecar_gate(&artifact, NOW + 1_000).unwrap_err();

            assert!(error.contains("read primary artifact"), "{error}");
            if unreadable {
                assert!(artifact.is_dir());
            } else {
                assert!(!artifact.exists());
            }
            assert_eq!(
                fs::read(&saved_primary).expect("read retained primary"),
                b"must not recreate this primary"
            );
            assert_eq!(sidecar_bytes(&artifact), before);
        }
    }
}

#[test]
fn gate_rejects_undecodable_sidecars_without_writing_files() {
    let content = b"a valid primary cannot compensate for an undecodable sidecar";
    let artifact = setup_temp_artifact("gate_undecodable.bin", content);
    let (_, _, symbols_path) =
        write_sidecar(&artifact, "test", 4, 128, NOW).expect("write sidecar");
    fs::write(&symbols_path, serialize_symbol_stream(&[])).expect("empty symbol stream");
    let before = sidecar_bytes(&artifact);

    assert!(verify_sidecar_gate(&artifact, NOW + 1_000).is_err());

    assert_eq!(fs::read(&artifact).expect("read primary"), content);
    assert_eq!(sidecar_bytes(&artifact), before);
}
