#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use fr_conformance::{
    CaseOutcome, HarnessConfig, run_fixture, run_protocol_fixture, run_replay_fixture,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    manifest: PathBuf,
    output_root: PathBuf,
    run_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AdversarialCorpusManifest {
    schema_version: String,
    corpus_id: String,
    default_seed: u64,
    suites: Vec<AdversarialSuite>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AdversarialSuite {
    suite_id: String,
    mode: SuiteMode,
    fixture: String,
    risk_focus: String,
    owner_bead: String,
    strict_replay_cmd: String,
    hardened_replay_cmd: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SuiteMode {
    Command,
    Protocol,
    Replay,
}

impl SuiteMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Protocol => "protocol",
            Self::Replay => "replay",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SuiteTriage {
    suite_id: String,
    mode: String,
    fixture: String,
    total: usize,
    passed: usize,
    failed_count: usize,
    status: String,
    execution_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RoutedFailure {
    suite_id: String,
    fixture: String,
    mode: String,
    case_name: String,
    classification: String,
    route_bead: String,
    detail: String,
    strict_replay_cmd: String,
    hardened_replay_cmd: String,
}

#[derive(Debug, Clone, Serialize)]
struct TriageReport {
    schema_version: String,
    manifest_schema_version: String,
    corpus_id: String,
    run_id: String,
    generated_at_unix_s: u64,
    suite_count: usize,
    routed_failure_count: usize,
    suite_execution_error_count: usize,
    suites: Vec<SuiteTriage>,
    routed_failures: Vec<RoutedFailure>,
    counts_by_classification: BTreeMap<String, usize>,
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
    let cli = parse_args(env::args().skip(1).collect())?;
    let manifest = load_manifest(&cli.manifest)?;

    let run_root = cli.output_root.join(&cli.run_id);
    let live_log_root = run_root.join("live_logs");
    fs::create_dir_all(&live_log_root).map_err(|err| {
        format!(
            "failed to create triage output directory {}: {err}",
            live_log_root.display()
        )
    })?;

    let mut cfg = HarnessConfig::default_paths();
    cfg.live_log_root = Some(live_log_root.clone());

    let mut suite_reports = Vec::new();
    let mut routed_failures = Vec::new();

    for suite in &manifest.suites {
        let run_result = match suite.mode {
            SuiteMode::Command => run_fixture(&cfg, &suite.fixture),
            SuiteMode::Protocol => run_protocol_fixture(&cfg, &suite.fixture),
            SuiteMode::Replay => run_replay_fixture(&cfg, &suite.fixture),
        };

        match run_result {
            Ok(report) => {
                let failed_count = report.failed.len();
                for case in &report.failed {
                    routed_failures.push(classify_case_failure(suite, case));
                }
                suite_reports.push(SuiteTriage {
                    suite_id: suite.suite_id.clone(),
                    mode: suite.mode.as_str().to_string(),
                    fixture: suite.fixture.clone(),
                    total: report.total,
                    passed: report.passed,
                    failed_count,
                    status: if failed_count == 0 {
                        "passed".to_string()
                    } else {
                        "failed".to_string()
                    },
                    execution_error: None,
                });
            }
            Err(err) => {
                routed_failures.push(RoutedFailure {
                    suite_id: suite.suite_id.clone(),
                    fixture: suite.fixture.clone(),
                    mode: suite.mode.as_str().to_string(),
                    case_name: "__suite__".to_string(),
                    classification: "suite_execution_error".to_string(),
                    route_bead: "bd-2wb.10".to_string(),
                    detail: err.clone(),
                    strict_replay_cmd: suite.strict_replay_cmd.clone(),
                    hardened_replay_cmd: suite.hardened_replay_cmd.clone(),
                });
                suite_reports.push(SuiteTriage {
                    suite_id: suite.suite_id.clone(),
                    mode: suite.mode.as_str().to_string(),
                    fixture: suite.fixture.clone(),
                    total: 0,
                    passed: 0,
                    failed_count: 0,
                    status: "execution_error".to_string(),
                    execution_error: Some(err),
                });
            }
        }
    }

    let mut counts_by_classification: BTreeMap<String, usize> = BTreeMap::new();
    for failure in &routed_failures {
        *counts_by_classification
            .entry(failure.classification.clone())
            .or_insert(0) += 1;
    }

    let suite_execution_error_count = routed_failures
        .iter()
        .filter(|failure| failure.classification == "suite_execution_error")
        .count();
    let report = TriageReport {
        schema_version: "adversarial_triage/v1".to_string(),
        manifest_schema_version: manifest.schema_version.clone(),
        corpus_id: manifest.corpus_id.clone(),
        run_id: cli.run_id.clone(),
        generated_at_unix_s: now_unix_secs(),
        suite_count: suite_reports.len(),
        routed_failure_count: routed_failures.len(),
        suite_execution_error_count,
        suites: suite_reports,
        routed_failures: routed_failures.clone(),
        counts_by_classification,
    };

    let report_path = run_root.join("triage_report.json");
    let routes_path = run_root.join("triage_routes.tsv");
    let readme_path = run_root.join("README.md");
    let env_path = run_root.join("env.json");
    let manifest_copy_path = run_root.join("manifest.json");
    let repro_lock_path = run_root.join("repro.lock");

    write_json(&report_path, &report)?;
    write_routes_tsv(&routes_path, &routed_failures)?;
    write_env_json(&env_path, &manifest, &cfg, &cli)?;
    copy_manifest(&cli.manifest, &manifest_copy_path)?;
    write_repro_lock(&repro_lock_path, &cli, &report)?;
    write_readme(&readme_path, &cli, &report, &routes_path, &report_path)?;

    println!("triage_run_id: {}", cli.run_id);
    println!("triage_output_root: {}", run_root.display());
    println!("triage_report: {}", report_path.display());
    println!("triage_routes: {}", routes_path.display());

    if report.routed_failure_count > 0 {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

fn default_manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/adversarial_corpus_v1.json")
}

fn default_run_id() -> String {
    format!("triage-{}", now_unix_secs())
}

fn parse_args(raw_args: Vec<String>) -> Result<CliArgs, String> {
    let mut args = raw_args;
    let mut manifest = default_manifest_path();
    let mut output_root = PathBuf::from("artifacts/adversarial_triage");
    let mut run_id = env::var("FR_ADV_RUN_ID").unwrap_or_else(|_| default_run_id());

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--manifest" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing path after --manifest"));
                }
                manifest = PathBuf::from(args[idx + 1].clone());
                args.drain(idx..=idx + 1);
                continue;
            }
            "--output-root" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing path after --output-root"));
                }
                output_root = PathBuf::from(args[idx + 1].clone());
                args.drain(idx..=idx + 1);
                continue;
            }
            "--run-id" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing value after --run-id"));
                }
                run_id = args[idx + 1].clone();
                args.drain(idx..=idx + 1);
                continue;
            }
            "-h" | "--help" => return Err(usage("help requested")),
            _ => idx += 1,
        }
    }

    if !args.is_empty() {
        return Err(usage("unexpected positional arguments"));
    }

    Ok(CliArgs {
        manifest,
        output_root,
        run_id,
    })
}

fn usage(reason: &str) -> String {
    format!(
        "{reason}\nusage: cargo run -p fr-conformance --bin adversarial_triage -- [--manifest <path>] [--output-root <dir>] [--run-id <id>]"
    )
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn load_manifest(path: &Path) -> Result<AdversarialCorpusManifest, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read manifest {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid manifest JSON {}: {err}", path.display()))
}

fn classify_case_failure(suite: &AdversarialSuite, case: &CaseOutcome) -> RoutedFailure {
    let detail = case.detail.clone().unwrap_or_default();
    let haystack = format!(
        "{} {:?} {:?}",
        detail.to_ascii_lowercase(),
        case.expected,
        case.actual
    )
    .to_ascii_lowercase();
    let (classification, route_bead) = classify_text(&haystack);
    RoutedFailure {
        suite_id: suite.suite_id.clone(),
        fixture: suite.fixture.clone(),
        mode: suite.mode.as_str().to_string(),
        case_name: case.name.clone(),
        classification: classification.to_string(),
        route_bead: route_bead.to_string(),
        detail,
        strict_replay_cmd: suite.strict_replay_cmd.clone(),
        hardened_replay_cmd: suite.hardened_replay_cmd.clone(),
    }
}

fn classify_text(haystack: &str) -> (&'static str, &'static str) {
    if haystack.contains("protocol_parse_failure")
        || haystack.contains("protocol error")
        || haystack.contains("unsupported resp3")
    {
        return ("parser_abuse", "bd-2wb.13.6");
    }
    if haystack.contains("eventloop.")
        || haystack.contains("ae_barrier_violation")
        || haystack.contains("blocked_mode")
    {
        return ("eventloop_contract", "bd-2wb.12.6");
    }
    if haystack.contains("compat_array_len_exceeded")
        || haystack.contains("compat_bulk_len_exceeded")
    {
        return ("admission_gate", "bd-2wb.12.6");
    }
    if haystack.contains("noauth") || haystack.contains("wrongpass") || haystack.contains("auth.") {
        return ("auth_policy", "bd-2wb.15.6");
    }
    if haystack.contains("tlscfg.") || haystack.contains("tls") {
        return ("tls_policy", "bd-2wb.20.6");
    }
    if haystack.contains("repl.") || haystack.contains("psync") || haystack.contains("handshake") {
        return ("replication_state", "bd-2wb.17.6");
    }
    if haystack.contains("aof") || haystack.contains("replay") {
        return ("replay_ordering", "bd-2wb.16.6");
    }
    if haystack.contains("wrong number of arguments")
        || haystack.contains("syntax error")
        || haystack.contains("unknown command")
    {
        return ("dispatch_validation", "bd-2wb.14.6");
    }
    ("unknown_regression", "bd-2wb.10")
}

fn write_json(path: &Path, report: &TriageReport) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = serde_json::to_string_pretty(report)
        .map_err(|err| format!("failed to encode triage report json: {err}"))?;
    fs::write(path, payload).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_routes_tsv(path: &Path, routes: &[RoutedFailure]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let mut out = String::from(
        "suite_id\tfixture\tmode\tcase_name\tclassification\troute_bead\tstrict_replay_cmd\thardened_replay_cmd\tdetail\n",
    );
    for route in routes {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            sanitize_tsv(&route.suite_id),
            sanitize_tsv(&route.fixture),
            sanitize_tsv(&route.mode),
            sanitize_tsv(&route.case_name),
            sanitize_tsv(&route.classification),
            sanitize_tsv(&route.route_bead),
            sanitize_tsv(&route.strict_replay_cmd),
            sanitize_tsv(&route.hardened_replay_cmd),
            sanitize_tsv(&route.detail),
        ));
    }
    fs::write(path, out).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn sanitize_tsv(value: &str) -> String {
    value.replace(['\t', '\n'], " ")
}

#[derive(Debug, Serialize)]
struct EnvReport {
    schema_version: &'static str,
    run_id: String,
    manifest_path: String,
    fixture_root: String,
    strict_mode: bool,
    live_log_root: String,
}

fn write_env_json(
    path: &Path,
    manifest: &AdversarialCorpusManifest,
    config: &HarnessConfig,
    cli: &CliArgs,
) -> Result<(), String> {
    let live_log_root = config
        .live_log_root
        .as_ref()
        .map_or(String::new(), |path| path.display().to_string());
    let env_report = EnvReport {
        schema_version: "adversarial_triage_env/v1",
        run_id: cli.run_id.clone(),
        manifest_path: cli.manifest.display().to_string(),
        fixture_root: config.fixture_root.display().to_string(),
        strict_mode: config.strict_mode,
        live_log_root,
    };
    let payload = serde_json::to_string_pretty(&env_report)
        .map_err(|err| format!("failed to encode env json: {err}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }
    fs::write(path, payload).map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    let _ = manifest;
    Ok(())
}

fn copy_manifest(source: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = fs::read_to_string(source)
        .map_err(|err| format!("failed to read manifest {}: {err}", source.display()))?;
    fs::write(target, payload)
        .map_err(|err| format!("failed to write manifest copy {}: {err}", target.display()))
}

fn write_repro_lock(path: &Path, cli: &CliArgs, report: &TriageReport) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = format!(
        "schema_version=adversarial_triage_repro/v1\nrun_id={}\nmanifest={}\noutput_root={}\nrouted_failure_count={}\n",
        cli.run_id,
        cli.manifest.display(),
        cli.output_root.display(),
        report.routed_failure_count
    );
    fs::write(path, payload).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn write_readme(
    path: &Path,
    cli: &CliArgs,
    report: &TriageReport,
    routes_path: &Path,
    report_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory {}: {err}",
                parent.display()
            )
        })?;
    }

    let body = format!(
        "# Adversarial Triage Bundle\n\n- run_id: `{}`\n- manifest: `{}`\n- suites: `{}`\n- routed_failures: `{}`\n- suite_execution_errors: `{}`\n\n## Artifacts\n\n- `triage_report.json`: machine-readable suite and failure classification report\n- `triage_routes.tsv`: route table (`classification -> blocker bead`)\n- `env.json`: execution environment summary\n- `manifest.json`: copied adversarial corpus manifest\n- `repro.lock`: deterministic replay metadata\n- `live_logs/`: structured log output generated by fixture runs\n\n## Re-run\n\n```bash\ncargo run -p fr-conformance --bin adversarial_triage -- --manifest {} --output-root {} --run-id {}\n```\n\nPaths:\n- report: `{}`\n- routes: `{}`\n",
        cli.run_id,
        cli.manifest.display(),
        report.suite_count,
        report.routed_failure_count,
        report.suite_execution_error_count,
        cli.manifest.display(),
        cli.output_root.display(),
        cli.run_id,
        report_path.display(),
        routes_path.display()
    );
    fs::write(path, body).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;

    use super::{CaseOutcome, classify_text, parse_args};

    #[test]
    fn parse_args_supports_overrides() {
        let parsed = parse_args(vec![
            "--manifest".to_string(),
            "fixtures/custom.json".to_string(),
            "--output-root".to_string(),
            "artifacts/custom".to_string(),
            "--run-id".to_string(),
            "run-123".to_string(),
        ])
        .expect("arguments parse");

        assert_eq!(
            parsed.manifest.to_string_lossy(),
            "fixtures/custom.json".to_string()
        );
        assert_eq!(
            parsed.output_root.to_string_lossy(),
            "artifacts/custom".to_string()
        );
        assert_eq!(parsed.run_id, "run-123".to_string());
    }

    #[test]
    fn classify_text_parser_abuse() {
        let (classification, route) = classify_text("protocol_parse_failure: malformed request");
        assert_eq!(classification, "parser_abuse");
        assert_eq!(route, "bd-2wb.13.6");
    }

    #[test]
    fn classify_text_replication_state() {
        let (classification, route) = classify_text("repl.psync_replid_or_offset_reject_mismatch");
        assert_eq!(classification, "replication_state");
        assert_eq!(route, "bd-2wb.17.6");
    }

    #[test]
    fn classify_text_dispatch_validation() {
        let (classification, route) = classify_text("ERR wrong number of arguments");
        assert_eq!(classification, "dispatch_validation");
        assert_eq!(route, "bd-2wb.14.6");
    }

    #[test]
    fn classify_text_eventloop_contract() {
        let (classification, route) = classify_text("eventloop.write.pending_reply_lost");
        assert_eq!(classification, "eventloop_contract");
        assert_eq!(route, "bd-2wb.12.6");
    }

    #[test]
    fn case_outcome_debug_compatibility() {
        let outcome = CaseOutcome {
            name: "sample".to_string(),
            passed: false,
            expected: RespFrame::SimpleString("OK".to_string()),
            actual: RespFrame::Error("ERR syntax error".to_string()),
            detail: Some("detail".to_string()),
            reason_code: Some("dispatch.wrong_arity".to_string()),
            replay_cmd: Some("cargo test -- sample".to_string()),
            artifact_refs: vec!["artifact.log".to_string()],
        };
        assert!(!outcome.passed);
    }
}
