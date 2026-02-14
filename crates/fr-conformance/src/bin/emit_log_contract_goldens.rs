#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use fr_conformance::log_contract::{
    PACKET_FAMILIES, STRUCTURED_LOG_SCHEMA_VERSION, golden_packet_logs,
};
use serde_json::json;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output_dir = repo_root.join("crates/fr-conformance/fixtures/log_contract_v1");
    fs::create_dir_all(&output_dir)
        .map_err(|err| format!("failed to create {}: {err}", output_dir.display()))?;

    let mut generated_files = Vec::new();
    for packet_id in PACKET_FAMILIES {
        let events = golden_packet_logs(packet_id)?;
        let output_path = output_dir.join(format!("{packet_id}.golden.jsonl"));

        let mut payload = String::new();
        for event in events {
            payload.push_str(&event.to_json_line()?);
            payload.push('\n');
        }

        fs::write(&output_path, payload)
            .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;
        generated_files.push(output_path);
    }

    let manifest_path = output_dir.join("manifest.json");
    let manifest = json!({
        "schema_version": STRUCTURED_LOG_SCHEMA_VERSION,
        "generated_at_utc": "2026-02-14T00:00:00Z",
        "generator": "cargo run -p fr-conformance --bin emit_log_contract_goldens",
        "packets": PACKET_FAMILIES,
        "files": generated_files.iter().map(|path| path.file_name().and_then(|name| name.to_str()).unwrap_or("<invalid>")).collect::<Vec<_>>(),
    });
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)
            .map_err(|err| format!("failed to serialize manifest: {err}"))?,
    )
    .map_err(|err| format!("failed to write {}: {err}", manifest_path.display()))?;

    let env_path = output_dir.join("env.json");
    let env_payload = json!({
        "workspace": "frankenredis",
        "crate": "fr-conformance",
        "schema_version": STRUCTURED_LOG_SCHEMA_VERSION,
        "purpose": "golden structured-log artifacts for unit/property/e2e schema stability",
        "regenerate_cmd": "cargo run -p fr-conformance --bin emit_log_contract_goldens",
    });
    fs::write(
        &env_path,
        serde_json::to_string_pretty(&env_payload)
            .map_err(|err| format!("failed to serialize env payload: {err}"))?,
    )
    .map_err(|err| format!("failed to write {}: {err}", env_path.display()))?;

    let repro_lock_path = output_dir.join("repro.lock");
    let repro_lock = [
        "cargo run -p fr-conformance --bin emit_log_contract_goldens",
        "cargo test -p fr-conformance --test log_contract_goldens -- --nocapture",
        "cargo test --workspace",
    ]
    .join("\n");
    fs::write(&repro_lock_path, repro_lock)
        .map_err(|err| format!("failed to write {}: {err}", repro_lock_path.display()))?;

    let readme_path = output_dir.join("README.md");
    let readme = [
        "# Log Contract Goldens v1",
        "",
        "Generated with:",
        "- `cargo run -p fr-conformance --bin emit_log_contract_goldens`",
        "",
        "Contains one golden `unit` and one golden `e2e` structured log entry",
        "for each packet family `FR-P2C-001..009`.",
        "",
        "See `TEST_LOG_SCHEMA_V1.md` for schema and replay rules.",
    ]
    .join("\n");
    fs::write(&readme_path, readme)
        .map_err(|err| format!("failed to write {}: {err}", readme_path.display()))?;

    Ok(())
}
