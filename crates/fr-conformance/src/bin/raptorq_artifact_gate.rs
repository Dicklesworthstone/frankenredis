#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use fr_fec::{
    CODEC_ID_RAPTORQ_1_8_1, ENVELOPE_SCHEMA_VERSION, ScrubOutcome, decode_artifact,
    encode_artifact, envelope_to_json, scrub, serialize_symbol_stream, verify_sidecar_gate,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct CliArgs {
    output_root: PathBuf,
    run_id: String,
    include_phase2c: bool,
    simulate_corruption: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DecodeProofEntry {
    proof_id: String,
    status: String,
    reason_code: String,
    generated_ts: String,
    recovered_artifact_sha256: String,
    source_hash: String,
    recovered_blocks: u32,
    codec_id: String,
    fec_proof_hash: String,
    fec_reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct DecodeProofFile {
    schema_version: String,
    artifact_id: String,
    source_rel_path: String,
    source_hash: String,
    envelope_path: String,
    symbols_path: String,
    decode_proofs: Vec<DecodeProofEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ReportEntry {
    artifact_id: String,
    source_rel_path: String,
    source_hash: String,
    sidecar_path: String,
    symbols_path: String,
    decode_proof_path: String,
    validation: String,
    reason_code: String,
    replay_cmd: String,
    corruption_check: String,
}

#[derive(Debug, Clone, Serialize)]
struct GateReport {
    schema_version: String,
    run_id: String,
    generated_ts: String,
    simulated_corruption: bool,
    artifact_count: usize,
    corruption_checks_passed: usize,
    artifacts: Vec<ReportEntry>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let repo_root = repo_root();
    let cli = match parse_args(env::args().skip(1).collect(), &repo_root)? {
        Some(cli) => cli,
        None => {
            println!("{}", usage());
            return Ok(ExitCode::SUCCESS);
        }
    };
    let artifact_targets = collect_artifact_targets(&repo_root, cli.include_phase2c)?;
    if artifact_targets.is_empty() {
        return Err("no durability artifact targets discovered".to_string());
    }

    // Gate D requirement: verify release-grade state artifact sidecar exists and is scrub-clean
    let state_artifact =
        repo_root.join("crates/fr-persist/tests/golden/stream_type21_vendored_redis_724.dump");
    if state_artifact.is_file() {
        verify_sidecar_gate(&state_artifact, epoch_ms_now() as u64).map_err(|err| {
            format!(
                "release state artifact sidecar gate failed for {}: {err}",
                state_artifact.display()
            )
        })?;
    }

    let run_dir = cli.output_root.join(&cli.run_id);
    let sidecar_dir = run_dir.join("sidecars");
    let corruption_dir = run_dir.join("corruption");
    fs::create_dir_all(&sidecar_dir)
        .map_err(|err| format!("failed to create {}: {err}", sidecar_dir.display()))?;
    fs::create_dir_all(&corruption_dir)
        .map_err(|err| format!("failed to create {}: {err}", corruption_dir.display()))?;

    let report_ndjson_path = run_dir.join("artifacts.ndjson");
    let mut report_ndjson = BufWriter::new(
        File::create(&report_ndjson_path)
            .map_err(|err| format!("failed to create {}: {err}", report_ndjson_path.display()))?,
    );

    let mut report_entries = Vec::new();
    for rel_path in artifact_targets {
        let source_path = repo_root.join(&rel_path);
        if !source_path.is_file() {
            return Err(format!("missing durability artifact target: {rel_path}"));
        }

        let source_bytes = fs::read(&source_path)
            .map_err(|err| format!("failed to read {}: {err}", source_path.display()))?;
        let source_hash = sha256_hex(&source_path)?;
        let expected_prefixed_hash = format!("sha256:{source_hash}");
        let artifact_id = artifact_id(&rel_path);
        // Emit canonical fr-fec sidecars (.envelope.json + .symbols) so gate
        // outputs are directly scrubbable and decodable by fr-fec without a
        // parallel lossy schema.
        let sidecar_path = sidecar_dir.join(format!("{artifact_id}.envelope.json"));
        let symbols_path = sidecar_dir.join(format!("{artifact_id}.symbols"));
        let decode_path = sidecar_dir.join(format!("{artifact_id}.decode_proof.json"));
        let generated_ts = utc_timestamp_iso();
        let scrub_ms = epoch_ms_now();

        // Encode artifact using real RFC 6330 systematic RaptorQ FEC
        let mut encoded = encode_artifact(
            &artifact_id,
            "durability_evidence_bundle",
            &source_bytes,
            4,
            256,
            scrub_ms as u64,
        )
        .map_err(|err| format!("failed to encode {rel_path} with RaptorQ: {err}"))?;
        if encoded.envelope.schema_version != ENVELOPE_SCHEMA_VERSION {
            return Err(format!(
                "unexpected fr-fec envelope schema {} for {rel_path}",
                encoded.envelope.schema_version
            ));
        }

        // Always execute a real, non-vacuous scrub before emitting any proof.
        match scrub(&encoded.envelope, &encoded.symbols, scrub_ms as u64) {
            ScrubOutcome::Clean => {}
            other => {
                return Err(format!(
                    "intact RaptorQ scrub failed for {rel_path}: {other:?}"
                ));
            }
        }

        let mut corruption_check = "skipped".to_string();
        let mut decode_proof_entries = Vec::new();

        if cli.simulate_corruption {
            let corrupt_path = corruption_dir.join(format!("{artifact_id}.corrupt"));
            fs::copy(&source_path, &corrupt_path).map_err(|err| {
                format!(
                    "failed to copy {} -> {}: {err}",
                    source_path.display(),
                    corrupt_path.display()
                )
            })?;
            let mut handle = OpenOptions::new()
                .append(true)
                .open(&corrupt_path)
                .map_err(|err| {
                    format!(
                        "failed to open {} for append: {err}",
                        corrupt_path.display()
                    )
                })?;
            handle
                .write_all(b"\nRAPTORQ_CORRUPTION_SENTINEL\n")
                .map_err(|err| format!("failed to append corruption sentinel: {err}"))?;
            let corrupt_hash = sha256_hex(&corrupt_path)?;
            if corrupt_hash == source_hash {
                return Err(format!(
                    "corruption simulation did not change digest for {rel_path}"
                ));
            }

            // Real G6 RaptorQ forced-recovery drill:
            // Lose the first source symbol and recover from surviving symbols.
            let mut surviving = encoded.symbols.clone();
            if surviving.len() > 1 {
                surviving.remove(0);
            }
            let (recovered_bytes, real_proof) = decode_artifact(
                &encoded.envelope,
                &surviving,
                "simulated-corruption forced-recovery drill",
                scrub_ms as u64,
            )
            .map_err(|err| format!("forced recovery failed for {rel_path}: {err}"))?;

            if recovered_bytes != source_bytes {
                return Err(format!("recovered payload mismatch for {rel_path}"));
            }
            if real_proof.proof_hash != expected_prefixed_hash {
                return Err(format!(
                    "decode proof hash mismatch for {rel_path}: {} != {expected_prefixed_hash}",
                    real_proof.proof_hash
                ));
            }

            encoded.envelope.decode_proofs.push(real_proof.clone());
            decode_proof_entries.push(DecodeProofEntry {
                proof_id: format!("{artifact_id}-proof-001"),
                status: "verified".to_string(),
                reason_code: "raptorq.decode_verified".to_string(),
                generated_ts: generated_ts.clone(),
                recovered_artifact_sha256: source_hash.clone(),
                source_hash: source_hash.clone(),
                recovered_blocks: real_proof.recovered_blocks,
                codec_id: encoded
                    .envelope
                    .codec_id
                    .clone()
                    .unwrap_or_else(|| CODEC_ID_RAPTORQ_1_8_1.to_string()),
                fec_proof_hash: real_proof.proof_hash,
                fec_reason: real_proof.reason,
            });

            corruption_check = "detected".to_string();
        } else {
            // Non-vacuous contract: even with --no-corruption, execute a real
            // decode_artifact round-trip and verify recovered bytes + proof hash.
            let (recovered_bytes, real_proof) = decode_artifact(
                &encoded.envelope,
                &encoded.symbols,
                "non-vacuous intact decode verification",
                scrub_ms as u64,
            )
            .map_err(|err| format!("intact decode verification failed for {rel_path}: {err}"))?;
            if recovered_bytes != source_bytes {
                return Err(format!("intact decode payload mismatch for {rel_path}"));
            }
            if real_proof.proof_hash != expected_prefixed_hash {
                return Err(format!(
                    "intact decode proof hash mismatch for {rel_path}: {} != {expected_prefixed_hash}",
                    real_proof.proof_hash
                ));
            }
            encoded.envelope.decode_proofs.push(real_proof.clone());
            decode_proof_entries.push(DecodeProofEntry {
                proof_id: format!("{artifact_id}-proof-001"),
                status: "verified".to_string(),
                reason_code: "raptorq.intact_decode_verified".to_string(),
                generated_ts: generated_ts.clone(),
                recovered_artifact_sha256: source_hash.clone(),
                source_hash: source_hash.clone(),
                recovered_blocks: real_proof.recovered_blocks,
                codec_id: encoded
                    .envelope
                    .codec_id
                    .clone()
                    .unwrap_or_else(|| CODEC_ID_RAPTORQ_1_8_1.to_string()),
                fec_proof_hash: real_proof.proof_hash,
                fec_reason: real_proof.reason,
            });
        }

        // Persist canonical fr-fec envelope + symbols stream.
        fs::write(
            &sidecar_path,
            format!("{}\n", envelope_to_json(&encoded.envelope)),
        )
        .map_err(|err| format!("failed to write {}: {err}", sidecar_path.display()))?;
        fs::write(&symbols_path, serialize_symbol_stream(&encoded.symbols))
            .map_err(|err| format!("failed to write {}: {err}", symbols_path.display()))?;

        // Read-back verification of persisted sidecar bytes through fr-fec.
        let reloaded_env = fr_fec::envelope_from_json(
            &fs::read_to_string(&sidecar_path)
                .map_err(|err| format!("failed to re-read {}: {err}", sidecar_path.display()))?,
        )
        .map_err(|err| format!("persisted envelope parse failed for {rel_path}: {err}"))?;
        let reloaded_syms = fr_fec::deserialize_symbol_stream(
            &fs::read(&symbols_path)
                .map_err(|err| format!("failed to re-read {}: {err}", symbols_path.display()))?,
        )
        .map_err(|err| format!("persisted symbol stream parse failed for {rel_path}: {err}"))?;
        if !matches!(
            scrub(&reloaded_env, &reloaded_syms, scrub_ms as u64),
            ScrubOutcome::Clean
        ) {
            return Err(format!("persisted sidecar failed scrub for {rel_path}"));
        }

        let decode = DecodeProofFile {
            schema_version: "fr_raptorq_decode_proof_v1".to_string(),
            artifact_id: artifact_id.clone(),
            source_rel_path: rel_path.clone(),
            source_hash: source_hash.clone(),
            envelope_path: path_to_string(&sidecar_path),
            symbols_path: path_to_string(&symbols_path),
            decode_proofs: decode_proof_entries,
        };
        write_json(&decode_path, &decode)?;

        if reloaded_env.source_hash != expected_prefixed_hash {
            return Err(format!("sidecar hash mismatch for {rel_path}"));
        }
        if decode.source_hash != source_hash {
            return Err(format!("decode-proof source hash mismatch for {rel_path}"));
        }
        if decode
            .decode_proofs
            .first()
            .map(|proof| proof.status.as_str())
            != Some("verified")
        {
            return Err(format!(
                "decode-proof status is not verified for {rel_path}"
            ));
        }

        let entry = ReportEntry {
            artifact_id: artifact_id.clone(),
            source_rel_path: rel_path.clone(),
            source_hash: source_hash.clone(),
            sidecar_path: path_to_string(&sidecar_path),
            symbols_path: path_to_string(&symbols_path),
            decode_proof_path: path_to_string(&decode_path),
            validation: "pass".to_string(),
            reason_code: decode.decode_proofs[0].reason_code.clone(),
            replay_cmd: format!(
                "./scripts/run_raptorq_artifact_gate.sh --output-root {} --run-id {}",
                path_to_string(&cli.output_root),
                cli.run_id
            ),
            corruption_check,
        };

        serde_json::to_writer(&mut report_ndjson, &entry).map_err(|err| {
            format!(
                "failed to write ndjson record {}: {err}",
                report_ndjson_path.display()
            )
        })?;
        report_ndjson
            .write_all(b"\n")
            .map_err(|err| format!("failed to write newline to ndjson: {err}"))?;
        report_entries.push(entry);
    }
    report_ndjson
        .flush()
        .map_err(|err| format!("failed to flush ndjson report: {err}"))?;

    let report_json_path = run_dir.join("report.json");
    let corruption_checks_passed = report_entries
        .iter()
        .filter(|entry| entry.corruption_check == "detected")
        .count();
    let gate_report = GateReport {
        schema_version: "fr_raptorq_artifact_gate_report/v1".to_string(),
        run_id: cli.run_id.clone(),
        generated_ts: utc_timestamp_iso(),
        simulated_corruption: cli.simulate_corruption,
        artifact_count: report_entries.len(),
        corruption_checks_passed,
        artifacts: report_entries,
    };
    write_json(&report_json_path, &gate_report)?;

    println!("raptorq artifact gate completed");
    println!("run_dir: {}", run_dir.display());
    println!("report: {}", report_json_path.display());

    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>, repo_root: &Path) -> Result<Option<CliArgs>, String> {
    let mut output_root = repo_root.join("artifacts/durability/raptorq_runs");
    let mut run_id = format!("local-{}", compact_utc_timestamp());
    let mut include_phase2c = true;
    let mut simulate_corruption = true;

    let mut idx = 0;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "--output-root" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing path after --output-root".to_string())?;
                output_root = PathBuf::from(value);
                idx += 2;
            }
            "--run-id" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --run-id".to_string())?;
                run_id = value.clone();
                idx += 2;
            }
            "--no-phase2c" => {
                include_phase2c = false;
                idx += 1;
            }
            "--no-corruption" => {
                simulate_corruption = false;
                idx += 1;
            }
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("Unknown argument: {other}\n{}", usage())),
        }
    }

    Ok(Some(CliArgs {
        output_root,
        run_id,
        include_phase2c,
        simulate_corruption,
    }))
}

fn usage() -> String {
    "Usage: raptorq_artifact_gate [options]\n\nGenerate and validate deterministic RaptorQ sidecar/decode-proof artifacts for\ndurability-critical evidence files, with optional corruption simulation.\n\nOptions:\n  --output-root <path>   Output root for run artifacts (default: artifacts/durability/raptorq_runs)\n  --run-id <id>          Run identifier (default: local-<utc-timestamp>)\n  --no-phase2c           Skip auto-discovery of artifacts/phase2c evidence bundles\n  --no-corruption        Skip corruption simulation checks\n  -h, --help             Show this help"
        .to_string()
}

fn repo_root() -> PathBuf {
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    candidate.canonicalize().unwrap_or(candidate)
}

fn collect_artifact_targets(
    repo_root: &Path,
    include_phase2c: bool,
) -> Result<Vec<String>, String> {
    let seed_targets = [
        "baselines/round1_conformance_baseline.json",
        "baselines/round2_protocol_negative_baseline.json",
        "crates/fr-conformance/fixtures/core_strings.json",
        "golden_outputs/core_strings.json",
        "crates/fr-persist/tests/golden/stream_type21_vendored_redis_724.dump",
    ];
    let mut seen = BTreeSet::new();

    for rel in seed_targets {
        if repo_root.join(rel).is_file() {
            seen.insert(rel.to_string());
        }
    }

    // Dynamically include all benchmark baselines (*.json, excluding sidecars)
    let baselines_dir = repo_root.join("baselines");
    if baselines_dir.is_dir() {
        let entries = fs::read_dir(&baselines_dir)
            .map_err(|err| format!("failed to read {}: {err}", baselines_dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("failed to read entry in baselines: {err}"))?;
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".json")
                && !name.ends_with(".envelope.json")
            {
                seen.insert(format!("baselines/{name}"));
            }
        }
    }

    // Dynamically include all conformance fixture suites (*.json, excluding sidecars)
    let fixtures_dir = repo_root.join("crates/fr-conformance/fixtures");
    if fixtures_dir.is_dir() {
        let entries = fs::read_dir(&fixtures_dir)
            .map_err(|err| format!("failed to read {}: {err}", fixtures_dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("failed to read entry in fixtures: {err}"))?;
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".json")
                && !name.ends_with(".envelope.json")
            {
                seen.insert(format!("crates/fr-conformance/fixtures/{name}"));
            }
        }
    }

    if include_phase2c {
        let phase2c_root = repo_root.join("artifacts/phase2c");
        if phase2c_root.is_dir() {
            let packets = fs::read_dir(&phase2c_root)
                .map_err(|err| format!("failed to read {}: {err}", phase2c_root.display()))?;
            for packet in packets {
                let packet = packet.map_err(|err| {
                    format!(
                        "failed to read packet entry in {}: {err}",
                        phase2c_root.display()
                    )
                })?;
                if !packet.path().is_dir() {
                    continue;
                }
                let files = fs::read_dir(packet.path()).map_err(|err| {
                    format!(
                        "failed to read phase2c packet dir {}: {err}",
                        packet.path().display()
                    )
                })?;
                for file in files {
                    let file = file.map_err(|err| {
                        format!(
                            "failed to read phase2c file entry in {}: {err}",
                            packet.path().display()
                        )
                    })?;
                    let file_path = file.path();
                    if !file_path.is_file() {
                        continue;
                    }
                    let Some(name) = file_path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !is_phase2c_target(name) {
                        continue;
                    }
                    let rel = file_path
                        .strip_prefix(repo_root)
                        .map_err(|err| format!("failed to strip repo prefix: {err}"))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    seen.insert(rel);
                }
            }
        }
    }

    Ok(seen.into_iter().collect())
}

fn is_phase2c_target(name: &str) -> bool {
    matches!(
        name,
        "baseline_profile.json"
            | "post_profile.json"
            | "lever_selection.md"
            | "isomorphism_report.md"
            | "env.json"
            | "manifest.json"
            | "repro.lock"
            | "LEGAL.md"
    )
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let bytes = file
            .read(&mut buf)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        if bytes == 0 {
            break;
        }
        hasher.update(&buf[..bytes]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.as_slice() {
        use std::fmt::Write;
        write!(&mut out, "{byte:02x}").expect("write to String is infallible");
    }
    Ok(out)
}

fn artifact_id(rel_path: &str) -> String {
    rel_path.replace(['/', '.'], "__")
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let payload = serde_json::to_string_pretty(value)
        .map_err(|err| format!("failed to serialize json: {err}"))?;
    fs::write(path, format!("{payload}\n"))
        .map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn compact_utc_timestamp() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    format!(
        "{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    )
}

fn utc_timestamp_iso() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    "1970-01-01T00:00:00Z".to_string()
}

fn epoch_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_id_is_compatible_with_script_transform() {
        assert_eq!(
            artifact_id("baselines/round1_conformance_baseline.json"),
            "baselines__round1_conformance_baseline__json"
        );
    }

    #[test]
    fn phase2c_target_filter_matches_contract() {
        assert!(is_phase2c_target("baseline_profile.json"));
        assert!(is_phase2c_target("LEGAL.md"));
        assert!(!is_phase2c_target("notes.txt"));
    }

    #[test]
    fn collect_artifact_targets_includes_all_baselines_and_fixtures() {
        let repo = repo_root();
        let targets = collect_artifact_targets(&repo, false).expect("collect targets");

        let baseline_targets: Vec<_> = targets
            .iter()
            .filter(|t| t.starts_with("baselines/"))
            .collect();
        assert!(
            baseline_targets.len() >= 13,
            "expected at least 13 baselines, found {}: {:?}",
            baseline_targets.len(),
            baseline_targets
        );

        let fixture_targets: Vec<_> = targets
            .iter()
            .filter(|t| t.starts_with("crates/fr-conformance/fixtures/"))
            .collect();
        assert!(
            fixture_targets.len() >= 51,
            "expected at least 51 fixtures, found {}: {:?}",
            fixture_targets.len(),
            fixture_targets
        );

        assert!(targets.contains(
            &"crates/fr-persist/tests/golden/stream_type21_vendored_redis_724.dump".to_string()
        ));
        assert!(targets.contains(&"crates/fr-conformance/fixtures/core_strings.json".to_string()));
        assert!(
            targets
                .contains(&"crates/fr-conformance/fixtures/adversarial_corpus_v1.json".to_string())
        );

        for target in &targets {
            assert!(
                !target.ends_with(".envelope.json"),
                "found envelope target: {target}"
            );
            assert!(
                !target.ends_with(".symbols"),
                "found symbols target: {target}"
            );
            assert!(
                repo.join(target).is_file(),
                "target does not exist on disk: {target}"
            );
        }
    }

    #[test]
    fn encode_and_corruption_recovery_on_target() {
        let repo = repo_root();
        let target = repo.join("baselines/round1_conformance_baseline.json");
        let source_bytes = fs::read(&target).expect("read baseline");
        let encoded = encode_artifact(
            "test_baseline",
            "durability_evidence_bundle",
            &source_bytes,
            4,
            256,
            1_788_700_000_000,
        )
        .expect("encode artifact");

        assert_eq!(encoded.envelope.raptorq.repair_symbols, 4);

        let mut surviving = encoded.symbols.clone();
        surviving.remove(0);
        let (recovered, proof) = decode_artifact(
            &encoded.envelope,
            &surviving,
            "test recovery",
            1_788_700_000_100,
        )
        .expect("decode artifact");

        assert_eq!(recovered, source_bytes);
        assert_eq!(proof.recovered_blocks, encoded.envelope.raptorq.k);
    }

    #[test]
    fn canonical_gate_sidecar_roundtrips_through_fr_fec_scrub_and_decode() {
        let repo = repo_root();
        let target = repo.join("baselines/round1_conformance_baseline.json");
        let source_bytes = fs::read(&target).expect("read baseline");
        let encoded = encode_artifact(
            "gate_canonical_test",
            "durability_evidence_bundle",
            &source_bytes,
            4,
            256,
            1_788_700_000_000,
        )
        .expect("encode");
        let env_json = envelope_to_json(&encoded.envelope);
        let sym_bytes = serialize_symbol_stream(&encoded.symbols);

        let parsed_env = fr_fec::envelope_from_json(&env_json).expect("parse canonical envelope");
        let parsed_syms =
            fr_fec::deserialize_symbol_stream(&sym_bytes).expect("parse canonical symbols");
        assert_eq!(parsed_env.schema_version, ENVELOPE_SCHEMA_VERSION);
        assert_eq!(parsed_env.codec_id.as_deref(), Some(CODEC_ID_RAPTORQ_1_8_1));
        assert!(matches!(
            scrub(&parsed_env, &parsed_syms, 1_788_700_000_100),
            ScrubOutcome::Clean
        ));
        let (recovered, proof) = decode_artifact(
            &parsed_env,
            &parsed_syms,
            "non-vacuous intact decode verification",
            1_788_700_000_200,
        )
        .expect("decode");
        assert_eq!(recovered, source_bytes);
        assert_eq!(proof.proof_hash, parsed_env.source_hash);
    }
}
