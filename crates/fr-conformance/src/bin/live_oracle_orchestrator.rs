#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    host: String,
    port: u16,
    output_root: PathBuf,
    run_id: String,
    runner: String,
    run_seed: u64,
    matrix: String,
    suite_manifest: PathBuf,
    suites: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct SuiteSpec {
    name: String,
    mode: String,
    fixture: String,
    scenario_class: String,
    profiles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct SuiteMatrixManifest {
    schema_version: String,
    suites: Vec<SuiteSpec>,
}

#[derive(Debug, Clone, Copy)]
struct SummaryCommandInputs<'a> {
    status_tsv: &'a Path,
    run_root: &'a Path,
    readme_path: &'a Path,
    replay_script: &'a Path,
    replay_all_script: &'a Path,
    coverage_summary: &'a Path,
    failure_envelope: &'a Path,
    run_fingerprint: &'a str,
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
    let cli = match parse_args(env::args().skip(1).collect())? {
        Some(cli) => cli,
        None => {
            println!("{}", usage());
            return Ok(ExitCode::SUCCESS);
        }
    };
    let run_fingerprint = compute_run_fingerprint(&cli);
    let manifest = load_suite_manifest(&cli.suite_manifest)?;
    let selected_suites = select_suites(&manifest, &cli.matrix, &cli.suites)?;

    let run_root = cli.output_root.join(&cli.run_id);
    let suites_root = run_root.join("suites");
    let live_log_root = run_root.join("live_logs");
    let trace_log = run_root.join("command_trace.log");
    let status_tsv = run_root.join("suite_status.tsv");
    let replay_script = run_root.join("replay_failed.sh");
    let replay_all_script = run_root.join("replay_all.sh");
    let readme_path = run_root.join("README.md");
    let coverage_summary = run_root.join("coverage_summary.json");
    let failure_envelope = run_root.join("failure_envelope.json");

    fs::create_dir_all(&suites_root)
        .map_err(|err| format!("failed to create {}: {err}", suites_root.display()))?;
    fs::create_dir_all(&live_log_root)
        .map_err(|err| format!("failed to create {}: {err}", live_log_root.display()))?;
    fs::write(&trace_log, "")
        .map_err(|err| format!("failed to initialize {}: {err}", trace_log.display()))?;
    fs::write(
        &status_tsv,
        "suite\tmode\tfixture\tscenario_class\texit_code\treport_json\tstdout_log\n",
    )
    .map_err(|err| format!("failed to initialize {}: {err}", status_tsv.display()))?;

    initialize_replay_script(&replay_script)?;
    initialize_replay_script(&replay_all_script)?;

    println!("Verifying live Redis endpoint {}:{}", cli.host, cli.port);
    verify_redis_endpoint(&cli.host, cli.port)?;
    println!(
        "Using live-oracle matrix '{}' from {} ({} suites)",
        cli.matrix,
        cli.suite_manifest.display(),
        selected_suites.len()
    );

    let mut failed_count = 0usize;
    let mut total_count = 0usize;
    for suite in &selected_suites {
        total_count += 1;

        let suite_dir = suites_root.join(&suite.name);
        let suite_log = suite_dir.join("stdout.log");
        let suite_report = suite_dir.join("report.json");
        fs::create_dir_all(&suite_dir)
            .map_err(|err| format!("failed to create {}: {err}", suite_dir.display()))?;

        let suite_cmd = suite_command_tokens(&cli, &live_log_root, &suite_report, suite);
        append_trace(
            &trace_log,
            suite,
            &cli.runner,
            cli.run_seed,
            &run_fingerprint,
            &suite_cmd,
        )?;

        println!("running {} ({} {})", suite.name, suite.mode, suite.fixture);
        let exit_code = run_command_to_log(&suite_cmd, &suite_log)?;

        append_status_row(&status_tsv, suite, exit_code, &suite_report, &suite_log)?;
        append_replay_command(
            &replay_all_script,
            &format!("# {} ({})", suite.name, suite.scenario_class),
            &suite_cmd,
        )?;

        if exit_code != 0 {
            failed_count += 1;
            append_replay_command(&replay_script, &format!("# {}", suite.name), &suite_cmd)?;
            println!("failed: {} (exit {})", suite.name, exit_code);
        } else {
            println!("passed: {}", suite.name);
        }
    }

    write_readme(
        &readme_path,
        &cli,
        &run_fingerprint,
        total_count,
        failed_count,
        &selected_suites,
    )?;

    let summary_cmd = summary_command_tokens(
        &cli,
        SummaryCommandInputs {
            status_tsv: &status_tsv,
            run_root: &run_root,
            readme_path: &readme_path,
            replay_script: &replay_script,
            replay_all_script: &replay_all_script,
            coverage_summary: &coverage_summary,
            failure_envelope: &failure_envelope,
            run_fingerprint: &run_fingerprint,
        },
    );
    run_command_inherit(&summary_cmd)?;

    println!("coverage_summary: {}", coverage_summary.display());
    if let Ok(contents) = fs::read_to_string(&coverage_summary) {
        println!("{contents}");
    }
    println!("failure_envelope: {}", failure_envelope.display());

    if failed_count > 0 {
        println!(
            "live oracle diffs failed ({failed_count}/{total_count}); bundle: {}",
            run_root.display()
        );
        return Ok(ExitCode::from(1));
    }

    println!(
        "live oracle diffs passed ({total_count}/{total_count}); bundle: {}",
        run_root.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<Option<CliArgs>, String> {
    let mut host = "127.0.0.1".to_string();
    let mut port = 6379_u16;
    let mut output_root = PathBuf::from(
        env::var("FR_E2E_OUTPUT_ROOT").unwrap_or_else(|_| "artifacts/e2e_orchestrator".to_string()),
    );
    let mut run_id = env::var("FR_E2E_RUN_ID").unwrap_or_else(|_| compact_utc_timestamp());
    let runner = env::var("FR_E2E_RUNNER").unwrap_or_else(|_| "local".to_string());
    let mut matrix = env::var("FR_E2E_MATRIX").unwrap_or_else(|_| "baseline".to_string());
    let mut suite_manifest = default_suite_manifest_path();
    let run_seed = env::var("FR_E2E_SEED")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|err| format!("invalid FR_E2E_SEED value {value}: {err}"))
        })
        .unwrap_or(Ok(424242_u64))?;
    let mut suites = Vec::new();

    let mut positional = Vec::new();
    let mut idx = 0usize;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "-h" | "--help" => return Ok(None),
            "--host" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --host".to_string())?;
                host = value.clone();
                idx += 2;
            }
            "--port" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --port".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|err| format!("invalid --port value {value}: {err}"))?;
                idx += 2;
            }
            "--output-root" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --output-root".to_string())?;
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
            "--matrix" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --matrix".to_string())?;
                matrix = value.clone();
                idx += 2;
            }
            "--suite-manifest" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --suite-manifest".to_string())?;
                suite_manifest = PathBuf::from(value);
                idx += 2;
            }
            "--suite" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --suite".to_string())?;
                suites.push(value.clone());
                idx += 2;
            }
            other => {
                positional.push(other.to_string());
                idx += 1;
            }
        }
    }

    if let Some(value) = positional.first() {
        host = value.clone();
    }
    if let Some(value) = positional.get(1) {
        port = value
            .parse::<u16>()
            .map_err(|err| format!("invalid positional port value {value}: {err}"))?;
    }

    Ok(Some(CliArgs {
        host,
        port,
        output_root,
        run_id,
        runner,
        run_seed,
        matrix,
        suite_manifest,
        suites,
    }))
}

fn usage() -> String {
    "Usage:\n  ./scripts/run_live_oracle_diff.sh [--host <host>] [--port <port>] [--output-root <dir>] [--run-id <id>] [--matrix <baseline|parity|all>] [--suite-manifest <path>] [--suite <name> ...]\n  ./scripts/run_live_oracle_diff.sh [host] [port]\n\nDescription:\n  Deterministic local/CI orchestrator for live Redis differential E2E suites.\n  It creates a self-contained failure bundle with per-suite logs, JSON reports,\n  replay commands, and command trace artifacts. The default matrix is 'baseline';\n  use '--matrix parity' for broader Redis-vs-FrankenRedis coverage."
        .to_string()
}

fn compute_run_fingerprint(cli: &CliArgs) -> String {
    let joined = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}",
        cli.run_id,
        cli.host,
        cli.port,
        cli.runner,
        cli.run_seed,
        cli.matrix,
        cli.suite_manifest.display(),
        cli.suites.join(",")
    );
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn default_suite_manifest_path() -> PathBuf {
    fixture_root().join("live_oracle_matrix.json")
}

fn load_suite_manifest(path: &Path) -> Result<SuiteMatrixManifest, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read suite manifest {}: {err}", path.display()))?;
    let manifest: SuiteMatrixManifest = serde_json::from_str(&raw)
        .map_err(|err| format!("invalid suite manifest JSON {}: {err}", path.display()))?;
    if manifest.schema_version != "live_oracle_matrix/v1" {
        return Err(format!(
            "unsupported suite manifest schema '{}' in {}",
            manifest.schema_version,
            path.display()
        ));
    }

    let mut seen_names = std::collections::BTreeSet::new();
    for suite in &manifest.suites {
        if suite.name.trim().is_empty() {
            return Err(format!(
                "suite manifest {} contains empty suite name",
                path.display()
            ));
        }
        if !seen_names.insert(suite.name.clone()) {
            return Err(format!(
                "suite manifest {} contains duplicate suite '{}'",
                path.display(),
                suite.name
            ));
        }
        if !matches!(suite.mode.as_str(), "command" | "protocol" | "multi_client") {
            return Err(format!(
                "suite '{}' uses unsupported mode '{}'",
                suite.name, suite.mode
            ));
        }
        if suite.fixture.trim().is_empty() {
            return Err(format!("suite '{}' is missing a fixture name", suite.name));
        }
        let fixture_path = fixture_root().join(&suite.fixture);
        if !fixture_path.exists() {
            return Err(format!(
                "suite '{}' references missing fixture {}",
                suite.name,
                fixture_path.display()
            ));
        }
        if suite.profiles.is_empty() {
            return Err(format!(
                "suite '{}' must belong to at least one profile",
                suite.name
            ));
        }
    }

    Ok(manifest)
}

fn select_suites(
    manifest: &SuiteMatrixManifest,
    matrix: &str,
    filters: &[String],
) -> Result<Vec<SuiteSpec>, String> {
    let filter_set: std::collections::BTreeSet<&str> = filters.iter().map(String::as_str).collect();
    for filter in &filter_set {
        if !manifest.suites.iter().any(|suite| suite.name == *filter) {
            return Err(format!("unknown suite filter '{}'", filter));
        }
    }

    let selected: Vec<SuiteSpec> = manifest
        .suites
        .iter()
        .filter(|suite| matrix == "all" || suite.profiles.iter().any(|profile| profile == matrix))
        .filter(|suite| filter_set.is_empty() || filter_set.contains(suite.name.as_str()))
        .cloned()
        .collect();

    if selected.is_empty() {
        return Err(format!(
            "suite selection is empty for matrix '{}' and filters {:?}",
            matrix, filters
        ));
    }

    Ok(selected)
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
    "19700101T000000Z".to_string()
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

fn initialize_replay_script(path: &Path) -> Result<(), String> {
    fs::write(path, "#!/usr/bin/env bash\nset -euo pipefail\n")
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    let metadata = fs::metadata(path)
        .map_err(|err| format!("failed to read metadata for {}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = metadata.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .map_err(|err| format!("failed to chmod {}: {err}", path.display()))?;
    }
    Ok(())
}

fn verify_redis_endpoint(host: &str, port: u16) -> Result<(), String> {
    let status = Command::new("redis-cli")
        .args(["-h", host, "-p", &port.to_string(), "ping"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("failed to execute redis-cli: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("redis-cli ping failed for {host}:{port}"))
    }
}

fn append_trace(
    trace_log: &Path,
    suite: &SuiteSpec,
    runner: &str,
    run_seed: u64,
    run_fingerprint: &str,
    cmd_tokens: &[String],
) -> Result<(), String> {
    let mut trace = OpenOptions::new()
        .create(true)
        .append(true)
        .open(trace_log)
        .map_err(|err| format!("failed to open {}: {err}", trace_log.display()))?;

    writeln!(trace, "[{}] suite={}", utc_timestamp_iso(), suite.name)
        .map_err(|err| format!("failed writing trace log: {err}"))?;
    writeln!(trace, "runner={runner}").map_err(|err| format!("failed writing trace log: {err}"))?;
    writeln!(trace, "scenario_class={}", suite.scenario_class)
        .map_err(|err| format!("failed writing trace log: {err}"))?;
    writeln!(trace, "run_seed={run_seed}")
        .map_err(|err| format!("failed writing trace log: {err}"))?;
    writeln!(trace, "run_fingerprint={run_fingerprint}")
        .map_err(|err| format!("failed writing trace log: {err}"))?;
    writeln!(trace, "cmd={}", format_shell_command(cmd_tokens))
        .map_err(|err| format!("failed writing trace log: {err}"))?;
    Ok(())
}

fn append_status_row(
    status_tsv: &Path,
    suite: &SuiteSpec,
    exit_code: i32,
    suite_report: &Path,
    suite_log: &Path,
) -> Result<(), String> {
    let mut status = OpenOptions::new()
        .create(true)
        .append(true)
        .open(status_tsv)
        .map_err(|err| format!("failed to open {}: {err}", status_tsv.display()))?;
    writeln!(
        status,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        suite.name,
        suite.mode,
        suite.fixture,
        suite.scenario_class,
        exit_code,
        path_to_string(suite_report),
        path_to_string(suite_log)
    )
    .map_err(|err| format!("failed to append status row: {err}"))
}

fn append_replay_command(path: &Path, heading: &str, cmd_tokens: &[String]) -> Result<(), String> {
    let mut script = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    writeln!(script, "\n{heading}")
        .map_err(|err| format!("failed writing replay script: {err}"))?;
    writeln!(script, "{}", format_shell_command(cmd_tokens))
        .map_err(|err| format!("failed writing replay script: {err}"))?;
    Ok(())
}

fn suite_command_tokens(
    cli: &CliArgs,
    live_log_root: &Path,
    suite_report: &Path,
    suite: &SuiteSpec,
) -> Vec<String> {
    let inner = vec![
        "env".to_string(),
        format!("FR_SEED={}", cli.run_seed),
        "cargo".to_string(),
        "run".to_string(),
        "-p".to_string(),
        "fr-conformance".to_string(),
        "--bin".to_string(),
        "live_oracle_diff".to_string(),
        "--".to_string(),
        "--log-root".to_string(),
        path_to_string(live_log_root),
        "--json-out".to_string(),
        path_to_string(suite_report),
        "--run-id".to_string(),
        cli.run_id.clone(),
        suite.mode.to_string(),
        suite.fixture.to_string(),
        cli.host.clone(),
        cli.port.to_string(),
    ];
    wrap_runner_tokens(&cli.runner, inner)
}

fn summary_command_tokens(cli: &CliArgs, inputs: SummaryCommandInputs<'_>) -> Vec<String> {
    let inner = vec![
        "cargo".to_string(),
        "run".to_string(),
        "-p".to_string(),
        "fr-conformance".to_string(),
        "--bin".to_string(),
        "live_oracle_bundle_summarizer".to_string(),
        "--".to_string(),
        "--status-tsv".to_string(),
        path_to_string(inputs.status_tsv),
        "--run-id".to_string(),
        cli.run_id.clone(),
        "--host".to_string(),
        cli.host.clone(),
        "--port".to_string(),
        cli.port.to_string(),
        "--runner".to_string(),
        cli.runner.clone(),
        "--run-root".to_string(),
        path_to_string(inputs.run_root),
        "--readme-path".to_string(),
        path_to_string(inputs.readme_path),
        "--replay-script".to_string(),
        path_to_string(inputs.replay_script),
        "--replay-all-script".to_string(),
        path_to_string(inputs.replay_all_script),
        "--coverage-summary-out".to_string(),
        path_to_string(inputs.coverage_summary),
        "--failure-envelope-out".to_string(),
        path_to_string(inputs.failure_envelope),
        "--run-seed".to_string(),
        cli.run_seed.to_string(),
        "--run-fingerprint".to_string(),
        inputs.run_fingerprint.to_string(),
    ];
    wrap_runner_tokens(&cli.runner, inner)
}

fn wrap_runner_tokens(runner: &str, inner: Vec<String>) -> Vec<String> {
    if runner == "rch" {
        let mut wrapped = vec![
            path_to_string(&rch_binary()),
            "exec".to_string(),
            "--".to_string(),
        ];
        wrapped.extend(inner);
        wrapped
    } else {
        inner
    }
}

fn run_command_to_log(cmd_tokens: &[String], suite_log: &Path) -> Result<i32, String> {
    let (program, args) = cmd_tokens
        .split_first()
        .ok_or_else(|| "cannot execute empty command".to_string())?;
    let log_file = File::create(suite_log)
        .map_err(|err| format!("failed to create {}: {err}", suite_log.display()))?;
    let err_file = log_file
        .try_clone()
        .map_err(|err| format!("failed to clone log handle {}: {err}", suite_log.display()))?;

    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(err_file))
        .status()
        .map_err(|err| {
            format!(
                "failed to execute suite command '{}': {err}",
                format_shell_command(cmd_tokens)
            )
        })?;
    Ok(status.code().unwrap_or(1))
}

fn run_command_inherit(cmd_tokens: &[String]) -> Result<(), String> {
    let (program, args) = cmd_tokens
        .split_first()
        .ok_or_else(|| "cannot execute empty command".to_string())?;
    let status = Command::new(program).args(args).status().map_err(|err| {
        format!(
            "failed to execute command '{}': {err}",
            format_shell_command(cmd_tokens)
        )
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "command failed (exit {}): {}",
            status.code().unwrap_or(1),
            format_shell_command(cmd_tokens)
        ))
    }
}

fn write_readme(
    readme_path: &Path,
    cli: &CliArgs,
    run_fingerprint: &str,
    total_count: usize,
    failed_count: usize,
    suites: &[SuiteSpec],
) -> Result<(), String> {
    let scenario_matrix = suites
        .iter()
        .map(|suite| format!("- `{}` ({})", suite.name, suite.scenario_class))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "# Live Oracle Diff Bundle\n\n- run_id: `{}`\n- host: `{}`\n- port: `{}`\n- runner: `{}`\n- run_seed: `{}`\n- run_fingerprint: `{}`\n- suite_matrix: `{}`\n- suite_manifest: `{}`\n- total_suites: `{}`\n- failed_suites: `{}`\n\n## Artifact Layout\n\n- `suite_status.tsv`: machine-readable suite execution status.\n- `command_trace.log`: exact command trace with timestamps.\n- `live_logs/`: structured JSONL logs emitted by harness (`live_log_root`).\n- `suites/<suite>/stdout.log`: captured command output.\n- `suites/<suite>/report.json`: machine-readable diff report from `live_oracle_diff --json-out`.\n- `coverage_summary.json`: aggregated pass-rate and reason-code budget input.\n- `failure_envelope.json`: per-failure envelope with replay pointers + deterministic artifact index.\n- `replay_all.sh`: deterministic replay commands for the full suite matrix.\n- `replay_failed.sh`: deterministic replay commands for failed suites.\n\n## Scenario Matrix\n\n{}\n\n## Re-run\n\n```bash\nFR_E2E_SEED={} ./scripts/run_live_oracle_diff.sh --host {} --port {} --run-id {} --matrix {}\n```\n",
        cli.run_id,
        cli.host,
        cli.port,
        cli.runner,
        cli.run_seed,
        run_fingerprint,
        cli.matrix,
        cli.suite_manifest.display(),
        total_count,
        failed_count,
        scenario_matrix,
        cli.run_seed,
        cli.host,
        cli.port,
        cli.run_id,
        cli.matrix
    );
    fs::write(readme_path, body)
        .map_err(|err| format!("failed to write {}: {err}", readme_path.display()))
}

fn rch_binary() -> PathBuf {
    if let Some(home) = env::var_os("HOME") {
        let candidate = PathBuf::from(home).join(".local/bin/rch");
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from("rch")
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn format_shell_command(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| shell_escape(token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(token: &str) -> String {
    if token.is_empty() {
        return "''".to_string();
    }
    if token
        .bytes()
        .all(|ch| matches!(ch, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':' | b'=' | b'+'))
    {
        return token.to_string();
    }
    let escaped = token.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::{
        CliArgs, compute_run_fingerprint, default_suite_manifest_path, load_suite_manifest,
        parse_args, select_suites, shell_escape,
    };

    #[test]
    fn parse_args_supports_flags_and_positionals() {
        let parsed = parse_args(vec![
            "--host".to_string(),
            "10.0.0.5".to_string(),
            "--port".to_string(),
            "6380".to_string(),
            "--output-root".to_string(),
            "artifacts/custom".to_string(),
            "--run-id".to_string(),
            "run-123".to_string(),
        ])
        .expect("arguments parse")
        .expect("help not requested");

        assert_eq!(parsed.host, "10.0.0.5");
        assert_eq!(parsed.port, 6380);
        assert_eq!(parsed.output_root.to_string_lossy(), "artifacts/custom");
        assert_eq!(parsed.run_id, "run-123");
        assert_eq!(parsed.matrix, "baseline");
        assert_eq!(parsed.suites, Vec::<String>::new());
        assert!(
            parsed
                .suite_manifest
                .to_string_lossy()
                .ends_with("crates/fr-conformance/fixtures/live_oracle_matrix.json")
        );
    }

    #[test]
    fn parse_args_positional_override_matches_script_contract() {
        let parsed = parse_args(vec!["host-from-pos".to_string(), "6389".to_string()])
            .expect("arguments parse")
            .expect("help not requested");
        assert_eq!(parsed.host, "host-from-pos");
        assert_eq!(parsed.port, 6389);
    }

    #[test]
    fn parse_args_supports_matrix_manifest_and_suite_filters() {
        let parsed = parse_args(vec![
            "--matrix".to_string(),
            "parity".to_string(),
            "--suite-manifest".to_string(),
            "fixtures/custom.json".to_string(),
            "--suite".to_string(),
            "core_hash".to_string(),
            "--suite".to_string(),
            "core_set".to_string(),
        ])
        .expect("arguments parse")
        .expect("help not requested");

        assert_eq!(parsed.matrix, "parity");
        assert_eq!(
            parsed.suite_manifest,
            std::path::PathBuf::from("fixtures/custom.json")
        );
        assert_eq!(
            parsed.suites,
            vec!["core_hash".to_string(), "core_set".to_string()]
        );
    }

    #[test]
    fn shell_escape_quotes_spaces_and_single_quotes() {
        assert_eq!(shell_escape("simple-token"), "simple-token");
        assert_eq!(shell_escape("needs space"), "'needs space'");
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn run_fingerprint_is_stable() {
        let cli = CliArgs {
            host: "127.0.0.1".to_string(),
            port: 6379,
            output_root: "artifacts/e2e_orchestrator".into(),
            run_id: "run-abc".to_string(),
            runner: "local".to_string(),
            run_seed: 424242,
            matrix: "baseline".to_string(),
            suite_manifest: default_suite_manifest_path(),
            suites: Vec::new(),
        };
        assert_eq!(
            compute_run_fingerprint(&cli),
            "09cbc41f84fc318f41bfa9562d041ff36953689225d1afbb5dc6fd1433364ad4".to_string()
        );
    }

    #[test]
    fn baseline_matrix_matches_legacy_suite_order() {
        let manifest = load_suite_manifest(&default_suite_manifest_path()).expect("load manifest");
        let suites = select_suites(&manifest, "baseline", &[]).expect("baseline matrix");
        let names = suites
            .iter()
            .map(|suite| suite.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "core_strings",
                "fr_p2c_001_eventloop_journey",
                "fr_p2c_003_dispatch_journey",
                "core_errors",
                "fr_p2c_002_protocol_negative",
            ]
        );
    }

    #[test]
    fn parity_matrix_is_a_strict_superset_of_baseline() {
        let manifest = load_suite_manifest(&default_suite_manifest_path()).expect("load manifest");
        let baseline = select_suites(&manifest, "baseline", &[]).expect("baseline matrix");
        let parity = select_suites(&manifest, "parity", &[]).expect("parity matrix");
        assert!(parity.len() > baseline.len());
        for suite in &baseline {
            assert!(
                parity.iter().any(|candidate| candidate.name == suite.name),
                "parity matrix must include baseline suite {}",
                suite.name
            );
        }
        assert!(parity.iter().any(|suite| suite.name == "core_hash"));
        assert!(parity.iter().any(|suite| suite.name == "core_scan"));
        assert!(
            parity
                .iter()
                .any(|suite| suite.name == "core_module_sentinel")
        );
        assert!(
            parity
                .iter()
                .any(|suite| suite.name == "fr_p2c_008_expire_evict_journey")
        );
    }
}
