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
    assert_eq!(recheck_env.scrub.last_ok_unix_ms, NOW + 3_000);
    assert_eq!(
        fs::read(&artifact).expect("read unchanged primary"),
        content
    );

    // 8. Gate still passes because it recovered
    verify_sidecar_gate(&artifact, NOW + 4_000).expect("gate passes after recovery");
}

#[test]
fn scrub_rejects_primary_tampering_without_overwriting_evidence() {
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
    fs::write(&corrupt_path, b"retained forensic evidence").expect("write existing backup");
    let before = sidecar_bytes(&artifact);

    let outcome = scrub_sidecar(&artifact, NOW + 5_000).expect("scrub tampered primary");
    match outcome {
        ScrubOutcome::Failed { reason } => {
            assert!(reason.contains("refusing to replace"), "{reason}");
        }
        other => panic!("expected Failed outcome, got {other:?}"),
    }

    assert_eq!(fs::read(&artifact).expect("read primary"), tampered);
    assert_eq!(
        fs::read(&corrupt_path).expect("read original backup"),
        b"retained forensic evidence"
    );
    assert_eq!(sidecar_bytes(&artifact), before);
    let (recheck_env, _) = read_sidecar(&artifact).expect("read unchanged envelope");
    assert_eq!(recheck_env.source_hash, env.source_hash);
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
            assert_eq!(
                sidecar_bytes(&artifact),
                before,
                "verification wrote evidence"
            );
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
            let outcome = scrub_sidecar(&artifact, NOW + 1_000).expect("scrub stale sidecar");
            assert!(
                matches!(outcome, ScrubOutcome::Failed { .. }),
                "stale sidecar unexpectedly accepted: {outcome:?}"
            );
            assert_eq!(
                fs::read(&artifact).expect("read newer primary"),
                new_content
            );
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
            let error = scrub_sidecar(&artifact, NOW + 1_000).unwrap_err();
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
    assert!(matches!(
        scrub_sidecar(&artifact, NOW + 1_000).expect("scrub undecodable sidecar"),
        ScrubOutcome::Failed { .. }
    ));

    assert_eq!(fs::read(&artifact).expect("read primary"), content);
    assert_eq!(sidecar_bytes(&artifact), before);
}

#[test]
fn recovery_exports_verified_candidates_without_changing_any_original() {
    let old = b"older generation retained by the sidecar";
    let current = b"newer committed primary";
    for legacy in [false, true] {
        for damaged in [false, true] {
            for missing_primary in [false, true] {
                let name = format!("export_{legacy}_{damaged}_{missing_primary}.bin");
                let artifact = setup_temp_artifact(&name, old);
                let (mut envelope, envelope_path, symbols_path) =
                    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
                if legacy {
                    envelope.source_hash.clear();
                    fs::write(&envelope_path, fr_fec::envelope_to_json(&envelope))
                        .expect("write legacy envelope");
                }
                if damaged {
                    let (_, mut symbols) = read_sidecar(&artifact).expect("read symbols");
                    symbols.remove(0);
                    fs::write(&symbols_path, serialize_symbol_stream(&symbols))
                        .expect("retain repair symbols");
                }
                fs::write(&artifact, current).expect("commit newer primary");
                let retained = artifact.with_extension("retained");
                if missing_primary {
                    fs::rename(&artifact, &retained).expect("retain primary elsewhere");
                }
                let backup = fr_fec::corrupt_backup_path(&artifact);
                fs::write(&backup, b"earlier forensic evidence").expect("write backup");
                let before = sidecar_bytes(&artifact);
                let output = artifact.with_extension("recovery");

                let proof = fr_fec::recover_sidecar_to_directory(&artifact, &output, NOW + 1_000)
                    .expect("export recovery candidate");

                let exported = fs::read(output.join("artifact")).expect("read exported artifact");
                assert_eq!(exported, old);
                assert_eq!(proof.proof_hash, fr_fec::sha256_hex(old));
                let receipt_bytes = fs::read(output.join("proof.json")).expect("read proof");
                let receipt: serde_json::Value =
                    serde_json::from_slice(&receipt_bytes).expect("parse complete proof");
                assert_eq!(receipt["artifact_bytes"], exported.len());
                assert_eq!(receipt["artifact_hash"], fr_fec::sha256_hex(&exported));
                assert_eq!(receipt["decode_proof"]["proof_hash"], proof.proof_hash);
                assert_eq!(receipt["envelope_source_hash"], envelope.source_hash);
                assert_eq!(receipt["primary_generation_verified"], false);
                assert_eq!(sidecar_bytes(&artifact), before);
                assert_eq!(
                    fs::read(&backup).expect("read retained backup"),
                    b"earlier forensic evidence"
                );
                if missing_primary {
                    assert!(!artifact.exists());
                    assert_eq!(fs::read(&retained).expect("read retained primary"), current);
                } else {
                    assert_eq!(fs::read(&artifact).expect("read primary"), current);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let permissions = fs::metadata(&output).expect("stat bundle").permissions();
                    assert_eq!(
                        permissions.mode() & 0o077,
                        0,
                        "recovered state must not be exposed to other users"
                    );
                }
            }
        }
    }
}

#[test]
fn invalid_recovery_creates_no_bundle_and_preserves_evidence() {
    for wrong_hash in [false, true] {
        let artifact = setup_temp_artifact(
            &format!("export_invalid_{wrong_hash}.bin"),
            b"intact primary",
        );
        let (mut envelope, envelope_path, symbols_path) =
            write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
        if wrong_hash {
            envelope.source_hash = fr_fec::sha256_hex(b"unrelated source");
            fs::write(&envelope_path, fr_fec::envelope_to_json(&envelope))
                .expect("replace claimed source hash");
        } else {
            fs::write(&symbols_path, serialize_symbol_stream(&[])).expect("remove all symbols");
        }
        let before = sidecar_bytes(&artifact);
        let output = artifact.with_extension("recovery");

        assert!(fr_fec::recover_sidecar_to_directory(&artifact, &output, NOW).is_err());

        assert!(!output.exists());
        assert_eq!(
            fs::read(&artifact).expect("read primary"),
            b"intact primary"
        );
        assert_eq!(sidecar_bytes(&artifact), before);
    }
}

#[test]
fn recovery_refuses_existing_files_directories_and_in_place_destinations() {
    let artifact = setup_temp_artifact("export_existing.bin", b"original artifact");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    let file_output = artifact.with_extension("existing-file");
    fs::write(&file_output, b"existing output").expect("write existing output");
    let directory_output = artifact.with_extension("existing-directory");
    fs::create_dir(&directory_output).expect("create existing directory");
    fs::write(directory_output.join("artifact"), b"previous export")
        .expect("write previous export");
    let (envelope_path, symbols_path) = fr_fec::sidecar_paths(&artifact);
    let before = sidecar_bytes(&artifact);

    for output in [
        &file_output,
        &directory_output,
        &artifact,
        &envelope_path,
        &symbols_path,
    ] {
        assert!(
            fr_fec::recover_sidecar_to_directory(&artifact, output, NOW).is_err(),
            "must refuse {}",
            output.display()
        );
    }

    assert_eq!(
        fs::read(&file_output).expect("read output"),
        b"existing output"
    );
    assert_eq!(
        fs::read(directory_output.join("artifact")).expect("read previous export"),
        b"previous export"
    );
    assert_eq!(
        fs::read(&artifact).expect("read primary"),
        b"original artifact"
    );
    assert_eq!(sidecar_bytes(&artifact), before);
}

#[test]
fn recovery_rejects_normalized_alias_of_a_missing_primary() {
    let artifact = setup_temp_artifact("export_missing_alias.bin", b"recoverable source");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    let retained = artifact.with_extension("retained");
    fs::rename(&artifact, &retained).expect("retain primary");
    let directory = artifact.with_extension("alias-parent");
    fs::create_dir(&directory).expect("create path for parent traversal");
    let alias = directory
        .join("..")
        .join(artifact.file_name().expect("artifact name"));
    let before = sidecar_bytes(&artifact);

    let error = fr_fec::recover_sidecar_to_directory(&artifact, &alias, NOW).unwrap_err();

    assert!(error.contains("aliases"), "{error}");
    assert!(
        !artifact.exists(),
        "must not create a directory at the missing primary"
    );
    assert_eq!(
        fs::read(&retained).expect("read retained primary"),
        b"recoverable source"
    );
    assert_eq!(sidecar_bytes(&artifact), before);
}

#[cfg(unix)]
#[test]
fn recovery_rejects_symlinked_parent_aliases_and_existing_symlink_outputs() {
    use std::os::unix::fs::symlink;

    let artifact = setup_temp_artifact("export_symlink_alias.bin", b"recoverable source");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    let retained = artifact.with_extension("retained");
    fs::rename(&artifact, &retained).expect("retain primary");
    let directory_alias = artifact.with_extension("parent-link");
    symlink(
        artifact.parent().expect("artifact parent"),
        &directory_alias,
    )
    .expect("create parent symlink");
    let alias = directory_alias.join(artifact.file_name().expect("artifact name"));
    let error = fr_fec::recover_sidecar_to_directory(&artifact, alias, NOW).unwrap_err();
    assert!(error.contains("aliases"), "{error}");
    assert!(!artifact.exists());

    let existing_alias = artifact.with_extension("output-link");
    symlink(&retained, &existing_alias).expect("create output symlink");
    assert!(fr_fec::recover_sidecar_to_directory(&artifact, &existing_alias, NOW).is_err());
    assert!(
        fs::symlink_metadata(&existing_alias)
            .expect("stat symlink")
            .is_symlink()
    );
    assert_eq!(
        fs::read(&retained).expect("read retained primary"),
        b"recoverable source"
    );
}

#[test]
fn concurrent_exports_cannot_replace_an_existing_recovery_bundle() {
    let artifact = setup_temp_artifact("export_concurrent.bin", b"concurrent recovery");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    let output = artifact.with_extension("recovery");
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| fr_fec::recover_sidecar_to_directory(&artifact, &output, NOW));
        let second = scope.spawn(|| fr_fec::recover_sidecar_to_directory(&artifact, &output, NOW));
        [
            first.join().expect("first recovery"),
            second.join().expect("second recovery"),
        ]
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        fs::read(output.join("artifact")).expect("read export"),
        b"concurrent recovery"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("proof.json")).expect("read proof"))
            .expect("complete proof");
    assert_eq!(
        receipt["artifact_hash"],
        fr_fec::sha256_hex(b"concurrent recovery")
    );
}

#[test]
fn cli_recovery_requires_a_new_bundle_and_scrub_reports_mismatches() {
    let artifact = setup_temp_artifact("export_cli.bin", b"sidecar generation");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    fs::write(&artifact, b"newer primary").expect("commit newer primary");
    let before = sidecar_bytes(&artifact);

    let unsafe_form = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .arg("recover")
        .arg(&artifact)
        .output()
        .expect("run legacy recovery syntax");
    assert!(!unsafe_form.status.success());
    assert!(String::from_utf8_lossy(&unsafe_form.stderr).contains("requires --output"));

    let scrub = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .arg("scrub")
        .arg(&artifact)
        .output()
        .expect("run CLI scrub");
    assert!(!scrub.status.success());
    assert_eq!(sidecar_bytes(&artifact), before);

    let output = artifact.with_extension("recovery");
    let export = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .args(["recover", "--output"])
        .arg(&output)
        .arg(&artifact)
        .output()
        .expect("run recovery export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert_eq!(
        fs::read(output.join("artifact")).expect("read export"),
        b"sidecar generation"
    );
    assert_eq!(fs::read(&artifact).expect("read primary"), b"newer primary");
    assert_eq!(sidecar_bytes(&artifact), before);
}

#[cfg(unix)]
#[test]
fn recovery_does_not_activate_a_dangling_primary_symlink() {
    use std::os::unix::fs::symlink;

    let artifact = setup_temp_artifact("export_dangling_source.bin", b"historical generation");
    write_sidecar(&artifact, "state", 4, 128, NOW).expect("write sidecar");
    let retained = artifact.with_extension("retained");
    fs::rename(&artifact, &retained).expect("retain original source");
    let output = artifact.with_extension("recovery");
    symlink(output.join("artifact"), &artifact).expect("create dangling primary link");
    let before = sidecar_bytes(&artifact);

    let error = fr_fec::recover_sidecar_to_directory(&artifact, &output, NOW).unwrap_err();

    assert!(error.contains("is a symlink"), "{error}");
    assert!(!output.exists());
    assert!(!artifact.exists(), "must not activate the dangling primary");
    assert_eq!(
        fs::read(&retained).expect("read retained source"),
        b"historical generation"
    );
    assert_eq!(sidecar_bytes(&artifact), before);
}

fn unique_gen_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("fr_fec_gen_{}_{}_{label}", std::process::id(), id));
    fs::create_dir_all(&dir).expect("create gen test dir");
    dir
}

#[test]
fn hmac_sha256_matches_rfc4231_test_vectors() {
    // RFC 4231 Test Case 1: key = 0x0b * 20, data = "Hi There"
    let mac1 = fr_fec::hmac_sha256_hex(&[0x0b; 20], b"Hi There");
    assert_eq!(
        mac1,
        "hmac-sha256:b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
    let mac2 = fr_fec::hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(
        mac2,
        "hmac-sha256:5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn generation_bound_sidecar_roundtrip_export_and_in_place_recovery() {
    let dir = unique_gen_dir("in-place");
    let primary = dir.join("dump.rdb");
    let original = b"REDIS0011\xfe\x00authentic committed generation payload for recovery!";
    fs::write(&primary, original).expect("write primary");

    let key = fr_fec::load_or_create_generation_auth_key(&dir).expect("provision key");
    let (record, env, gen_path, env_path, sym_path) =
        fr_fec::write_generation_bound_sidecar(&primary, "state", 4, 64, None, &key, NOW)
            .expect("write generation-bound sidecar");
    assert!(gen_path.is_file() && env_path.is_file() && sym_path.is_file());
    assert_eq!(record.generation_seq, 1);
    assert!(record.fec_enabled);
    assert_eq!(
        record.sidecar_generation_id.as_deref(),
        Some(record.generation_id.as_str())
    );
    assert_eq!(
        env.codec_id.as_deref(),
        Some(fr_fec::CODEC_ID_RAPTORQ_1_8_1)
    );

    // Read-only generation-bound gate succeeds on intact primary.
    let verified = fr_fec::verify_generation_bound_sidecar_gate(&primary, Some(&key), NOW + 10)
        .expect("verified generation gate");
    assert!(verified.symbols_file_intact);

    // Export recovery reports primary_generation_verified: true.
    let export_dir = dir.join("export-verified");
    fr_fec::recover_sidecar_to_directory(&primary, &export_dir, NOW + 20)
        .expect("export verified generation");
    let export_receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(export_dir.join("proof.json")).expect("read proof"))
            .expect("parse proof");
    assert_eq!(export_receipt["primary_generation_verified"], true);
    assert_eq!(export_receipt["generation_id"], record.generation_id);
    assert_eq!(export_receipt["generation_seq"], 1);

    // Seed an existing legacy .corrupt backup to prove in-place recovery never clobbers it.
    let legacy_backup = fr_fec::corrupt_backup_path(&primary);
    fs::write(&legacy_backup, b"existing forensic backup").expect("seed legacy backup");

    // Corrupt BOTH the primary file AND one source symbol in .symbols simultaneously!
    let corrupt_primary = b"REDIS0099\xffCORRUPTED_PRIMARY_BYTES";
    fs::write(&primary, corrupt_primary).expect("corrupt primary");
    let (_, mut packets) = read_sidecar(&primary).expect("read packets");
    packets.remove(0);
    fs::write(&sym_path, serialize_symbol_stream(&packets)).expect("drop one source symbol");

    // Read-only gate now detects primary damage without modifying anything.
    assert!(fr_fec::verify_generation_bound_sidecar_gate(&primary, Some(&key), NOW + 30).is_err());

    // Atomic in-place recovery preserves the corrupt primary, repairs atomically, and emits proof.
    let proof = fr_fec::recover_sidecar_in_place(&primary, Some(&key), NOW + 40)
        .expect("atomic in-place recovery");
    assert!(proof.primary_generation_verified);
    assert_eq!(proof.generation_id, record.generation_id);
    assert_eq!(proof.generation_seq, 1);
    assert_eq!(proof.recovered_primary_sha256, fr_fec::sha256_hex(original));
    assert_eq!(
        proof.preserved_corrupt_primary_sha256,
        fr_fec::sha256_hex(corrupt_primary)
    );
    assert_eq!(fs::read(&primary).expect("read repaired primary"), original);
    assert_eq!(
        fs::read(&proof.preserved_corrupt_primary_path).expect("read preserved corrupt primary"),
        corrupt_primary
    );
    assert_eq!(
        fs::read(&legacy_backup).expect("legacy backup untouched"),
        b"existing forensic backup"
    );
    let written_proof: fr_fec::InPlaceRecoveryProof =
        serde_json::from_slice(&fs::read(&proof.proof_path).expect("read written proof"))
            .expect("parse written proof");
    assert_eq!(written_proof, proof);

    // Post-repair verification gate passes again.
    fr_fec::verify_generation_bound_sidecar_gate(&primary, Some(&key), NOW + 50)
        .expect("gate passes after verified in-place recovery");
}

#[test]
fn generation_bound_recovery_refuses_stale_disabled_tampered_interrupted_missing_and_bad_codec() {
    // 1. Unversioned sidecar (no .generation.json) is refused for in-place recovery.
    let dir_unv = unique_gen_dir("unversioned");
    let p_unv = dir_unv.join("dump.rdb");
    fs::write(&p_unv, b"unversioned primary").expect("write");
    write_sidecar(&p_unv, "state", 4, 64, NOW).expect("unversioned sidecar");
    let key_unv = fr_fec::load_or_create_generation_auth_key(&dir_unv).expect("key");
    let err = fr_fec::recover_sidecar_in_place(&p_unv, Some(&key_unv), NOW + 1).unwrap_err();
    assert!(err.contains("read generation record"), "{err}");

    // 2. Disabled-FEC transition invalidates older sidecars and allows safe retirement.
    let dir_dis = unique_gen_dir("disabled-fec");
    let p_dis = dir_dis.join("dump.rdb");
    fs::write(&p_dis, b"gen1 bytes").expect("write gen1");
    let key_dis = fr_fec::load_or_create_generation_auth_key(&dir_dis).expect("key");
    fr_fec::write_generation_bound_sidecar(&p_dis, "state", 4, 64, None, &key_dis, NOW)
        .expect("write gen1 sidecar");
    // Cannot retire active current-generation sidecar.
    assert!(fr_fec::retire_superseded_sidecar(&p_dis, &key_dis).is_err());

    let gen2_bytes = b"gen2 committed with FEC disabled";
    fs::write(&p_dis, gen2_bytes).expect("write gen2");
    fr_fec::commit_primary_generation(
        &p_dis,
        &fr_fec::sha256_hex(gen2_bytes),
        gen2_bytes.len() as u64,
        false,
        &key_dis,
        NOW + 10,
    )
    .expect("commit disabled-FEC gen2");
    let err = fr_fec::recover_sidecar_in_place(&p_dis, Some(&key_dis), NOW + 20).unwrap_err();
    assert!(err.contains("FEC disabled"), "{err}");
    assert_eq!(fs::read(&p_dis).expect("gen2 untouched"), gen2_bytes);
    let retired = fr_fec::retire_superseded_sidecar(&p_dis, &key_dis).expect("retire superseded");
    assert!(
        retired
            .retired_envelope
            .as_ref()
            .is_some_and(|p| p.is_file())
    );
    assert!(
        retired
            .retired_symbols
            .as_ref()
            .is_some_and(|p| p.is_file())
    );
    let (active_env, active_sym) = fr_fec::sidecar_paths(&p_dis);
    assert!(!active_env.exists() && !active_sym.exists());

    // 3. Interrupted sidecar publication (Phase 1 committed, Phase 2 never finished) refuses recovery.
    let dir_int = unique_gen_dir("interrupted");
    let p_int = dir_int.join("dump.rdb");
    fs::write(&p_int, b"gen1 bytes").expect("write gen1");
    let key_int = fr_fec::load_or_create_generation_auth_key(&dir_int).expect("key");
    fr_fec::write_generation_bound_sidecar(&p_int, "state", 4, 64, None, &key_int, NOW)
        .expect("write gen1");
    let gen2_int = b"gen2 crashed before sidecar phase 2";
    fs::write(&p_int, gen2_int).expect("write gen2");
    fr_fec::commit_primary_generation(
        &p_int,
        &fr_fec::sha256_hex(gen2_int),
        gen2_int.len() as u64,
        true,
        &key_int,
        NOW + 10,
    )
    .expect("phase 1 commit");
    let err = fr_fec::recover_sidecar_in_place(&p_int, Some(&key_int), NOW + 20).unwrap_err();
    assert!(err.contains("no committed sidecar"), "{err}");

    // 4. Tampered generation record MAC is rejected.
    let dir_tamper = unique_gen_dir("tamper-mac");
    let p_tamper = dir_tamper.join("dump.rdb");
    fs::write(&p_tamper, b"authentic").expect("write");
    let key_tamper = fr_fec::load_or_create_generation_auth_key(&dir_tamper).expect("key");
    let (mut rec, _, gen_path, _, _) =
        fr_fec::write_generation_bound_sidecar(&p_tamper, "state", 4, 64, None, &key_tamper, NOW)
            .expect("write");
    rec.generation_seq = 999;
    fs::write(&gen_path, serde_json::to_vec_pretty(&rec).expect("json")).expect("tamper rec");
    let err = fr_fec::recover_sidecar_in_place(&p_tamper, Some(&key_tamper), NOW + 5).unwrap_err();
    assert!(err.contains("authentication failed"), "{err}");

    // 5. Missing primary is never resurrected in place.
    let dir_miss = unique_gen_dir("missing-primary");
    let p_miss = dir_miss.join("dump.rdb");
    fs::write(&p_miss, b"do not resurrect").expect("write");
    let key_miss = fr_fec::load_or_create_generation_auth_key(&dir_miss).expect("key");
    fr_fec::write_generation_bound_sidecar(&p_miss, "state", 4, 64, None, &key_miss, NOW)
        .expect("write");
    fs::rename(&p_miss, dir_miss.join("moved-aside.rdb")).expect("move primary");
    let err = fr_fec::recover_sidecar_in_place(&p_miss, Some(&key_miss), NOW + 5).unwrap_err();
    assert!(err.contains("missing primary artifact"), "{err}");
    assert!(!p_miss.exists());

    // 6. Unknown/future codec_id (e.g. raptorq-2.x-rfc6330) fails closed.
    let dir_codec = unique_gen_dir("bad-codec");
    let p_codec = dir_codec.join("dump.rdb");
    fs::write(&p_codec, b"codec discrimination").expect("write");
    let (mut env, env_path, _) = write_sidecar(&p_codec, "state", 4, 64, NOW).expect("sidecar");
    env.codec_id = Some("raptorq-2.x-rfc6330".to_string());
    fs::write(&env_path, fr_fec::envelope_to_json(&env)).expect("write future codec");
    let err = verify_sidecar_gate(&p_codec, NOW + 5).unwrap_err();
    assert!(err.contains("unsupported FEC codec_id"), "{err}");
}

#[test]
fn chunked_v2_streaming_encode_scrub_export_and_in_place_recovery() {
    // Validate chunked preflight on both sides of the 255-source-block limit
    // without allocating multi-GiB memory.
    #[cfg(target_pointer_width = "64")]
    {
        let max_single_chunk = 14_382_765_u64 * 512;
        fr_fec::validate_chunked_options(fr_fec::ChunkedEncodeOptions {
            chunk_size: max_single_chunk,
            repair_symbols: 4,
            symbol_size: 512,
        })
        .expect("largest single chunk at 255-block ceiling");
        assert!(
            fr_fec::validate_chunked_options(fr_fec::ChunkedEncodeOptions {
                chunk_size: max_single_chunk + 1,
                repair_symbols: 4,
                symbol_size: 512,
            })
            .is_err()
        );
    }
    assert!(
        fr_fec::validate_chunked_options(fr_fec::ChunkedEncodeOptions {
            chunk_size: 0,
            repair_symbols: 4,
            symbol_size: 64,
        })
        .is_err()
    );

    // Multi-chunk streaming roundtrip (5 chunks of 300 bytes + tail = 1,337 bytes)
    let dir = unique_gen_dir("chunked-v2");
    let primary = dir.join("large-sim.rdb");
    let original: Vec<u8> = (0..1_337_usize)
        .map(|i| ((i * 31 + 7) & 0xff) as u8)
        .collect();
    fs::write(&primary, &original).expect("write multi-chunk artifact");
    let key = fr_fec::load_or_create_generation_auth_key(&dir).expect("key");

    let (record, env, _, _, sym_path) =
        fr_fec::write_generation_bound_sidecar(&primary, "state", 4, 64, Some(300), &key, NOW)
            .expect("write chunked v2 generation-bound sidecar");
    assert_eq!(env.schema_version, fr_fec::CHUNKED_ENVELOPE_SCHEMA_VERSION);
    let layout = env.chunked.as_ref().expect("chunked layout");
    assert_eq!(layout.chunks.len(), 5);
    assert_eq!(layout.total_source_bytes, 1_337);

    // Clean gate + clean scrub pass.
    fr_fec::verify_generation_bound_sidecar_gate(&primary, Some(&key), NOW + 10)
        .expect("chunked gate passes");
    assert!(matches!(
        scrub_sidecar(&primary, NOW + 15).expect("chunked scrub"),
        ScrubOutcome::Clean
    ));

    // Flip 1 payload byte inside chunk 0's first symbol AND corrupt the primary file!
    let mut sym_bytes = fs::read(&sym_path).expect("read v2 symbols");
    let c0_payload_offset = layout.chunks[0].symbol_stream_offset as usize + 16;
    sym_bytes[c0_payload_offset] ^= 0xFF;
    fs::write(&sym_path, &sym_bytes).expect("damage chunk 0 symbol");
    let corrupt_primary = vec![0xDE_u8; 77];
    fs::write(&primary, &corrupt_primary).expect("corrupt primary");

    // Export recovery works for v2 and reports primary_generation_verified = true.
    let export_dir = dir.join("chunked-export");
    fr_fec::recover_sidecar_to_directory(&primary, &export_dir, NOW + 20)
        .expect("export chunked recovery");
    assert_eq!(
        fs::read(export_dir.join("artifact")).expect("exported bytes"),
        original
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(export_dir.join("proof.json")).expect("proof"))
            .expect("parse proof");
    assert_eq!(receipt["primary_generation_verified"], true);

    // Atomic in-place recovery repairs from v2 chunked symbols despite damaged chunk-0 symbol.
    let proof = fr_fec::recover_sidecar_in_place(&primary, Some(&key), NOW + 30)
        .expect("chunked in-place recovery");
    assert_eq!(proof.generation_id, record.generation_id);
    assert_eq!(fs::read(&primary).expect("repaired bytes"), original);
    assert_eq!(
        fs::read(&proof.preserved_corrupt_primary_path).expect("preserved corrupt"),
        corrupt_primary
    );
    // Scrub now sees the damaged symbol on repaired primary and recovers cleanly.
    assert!(matches!(
        scrub_sidecar(&primary, NOW + 40).expect("scrub chunked after damage"),
        ScrubOutcome::Recovered { .. }
    ));
}

#[test]
fn cli_generation_bound_in_place_recovery_and_retire_superseded() {
    let dir = unique_gen_dir("cli-gen");
    let primary = dir.join("snapshot.rdb");
    let original = b"cli generation-bound recovery test payload 0123456789";
    fs::write(&primary, original).expect("write primary");

    let gen_out = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .args([
            "sidecar",
            "--type",
            "state",
            "--repair-symbols",
            "4",
            "--symbol-size",
            "64",
            "--chunk-size",
            "32",
            "--generation-bound",
        ])
        .arg(&primary)
        .output()
        .expect("run sidecar --generation-bound");
    assert!(
        gen_out.status.success(),
        "{}",
        String::from_utf8_lossy(&gen_out.stderr)
    );

    let gate_out = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .args(["verify-gate", "--generation-bound"])
        .arg(&primary)
        .output()
        .expect("run verify-gate --generation-bound");
    assert!(
        gate_out.status.success(),
        "{}",
        String::from_utf8_lossy(&gate_out.stderr)
    );

    // Corrupt primary and repair via CLI --in-place.
    fs::write(&primary, b"corrupted").expect("corrupt");
    let rec_out = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .args(["recover", "--in-place"])
        .arg(&primary)
        .output()
        .expect("run recover --in-place");
    assert!(
        rec_out.status.success(),
        "{}",
        String::from_utf8_lossy(&rec_out.stderr)
    );
    assert_eq!(fs::read(&primary).expect("repaired"), original);

    // Commit FEC-disabled generation and retire superseded sidecars via CLI.
    let key = fr_fec::load_existing_generation_auth_key(&dir).expect("key");
    let newer = b"newer generation without FEC";
    fs::write(&primary, newer).expect("write newer");
    fr_fec::commit_primary_generation(
        &primary,
        &fr_fec::sha256_hex(newer),
        newer.len() as u64,
        false,
        &key,
        NOW + 100,
    )
    .expect("disable fec");
    let retire_out = std::process::Command::new(env!("CARGO_BIN_EXE_fr-fec"))
        .arg("retire-superseded")
        .arg(&primary)
        .output()
        .expect("run retire-superseded");
    assert!(
        retire_out.status.success(),
        "{}",
        String::from_utf8_lossy(&retire_out.stderr)
    );
    let (env_p, sym_p) = fr_fec::sidecar_paths(&primary);
    assert!(!env_p.exists() && !sym_p.exists());
}
