#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fr_conformance::{
    CaseOutcome, DIFFERENTIAL_REPORT_SCHEMA_VERSION, DifferentialReport, HarnessConfig,
    LiveOracleConfig, run_live_redis_diff, run_live_redis_protocol_diff,
};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    mode: String,
    fixture: String,
    host: String,
    port: u16,
    log_root: Option<PathBuf>,
    json_out: Option<PathBuf>,
    run_id: Option<String>,
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

    let mut cfg = HarnessConfig::default_paths();
    cfg.live_log_root = cli.log_root.clone();
    let oracle = LiveOracleConfig {
        host: cli.host.clone(),
        port: cli.port,
        ..LiveOracleConfig::default()
    };

    let report = match cli.mode.as_str() {
        "command" => match run_live_redis_diff(&cfg, &cli.fixture, &oracle) {
            Ok(report) => report,
            Err(err) => {
                if let Some(path) = &cli.json_out {
                    write_json_error_report(path, &cli, &err)?;
                    println!("json_report: {}", path.display());
                }
                return Err(err);
            }
        },
        "protocol" => match run_live_redis_protocol_diff(&cfg, &cli.fixture, &oracle) {
            Ok(report) => report,
            Err(err) => {
                if let Some(path) = &cli.json_out {
                    write_json_error_report(path, &cli, &err)?;
                    println!("json_report: {}", path.display());
                }
                return Err(err);
            }
        },
        _ => return Err(usage("mode must be 'command' or 'protocol'")),
    };

    println!("suite: {}", report.suite);
    println!("schema_version: {}", report.schema_version);
    println!("total: {}", report.total);
    println!("passed: {}", report.passed);
    println!("failed: {}", report.failed.len());
    println!(
        "pass_rate: {:.4}",
        compute_pass_rate(report.passed, report.total)
    );
    if !report.reason_code_counts.is_empty() {
        println!("reason_code_summary:");
        for (reason_code, count) in &report.reason_code_counts {
            println!("  {reason_code}: {count}");
        }
    }
    if report.failed_without_reason_code > 0 {
        println!(
            "failed_without_reason_code: {}",
            report.failed_without_reason_code
        );
    }
    if let Some(path) = &cli.log_root {
        println!("live_log_root: {}", path.display());
    }
    if let Some(path) = &cli.json_out {
        write_json_report(path, &cli, &report)?;
        println!("json_report: {}", path.display());
    }

    if !report.failed.is_empty() {
        for failure in &report.failed {
            println!("---");
            println!("case: {}", failure.name);
            println!("expected(redis): {:?}", failure.expected);
            println!("actual(runtime):  {:?}", failure.actual);
            if let Some(detail) = &failure.detail {
                println!("detail: {detail}");
            }
            if let Some(reason_code) = &failure.reason_code {
                println!("reason_code: {reason_code}");
            }
            if let Some(replay_cmd) = &failure.replay_cmd {
                println!("replay_cmd: {replay_cmd}");
            }
            if !failure.artifact_refs.is_empty() {
                println!("artifact_refs:");
                for artifact_ref in &failure.artifact_refs {
                    println!("  - {artifact_ref}");
                }
            }
        }
        return Ok(ExitCode::from(1));
    }

    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<CliArgs, String> {
    let mut args = raw_args;
    let mut log_root: Option<PathBuf> = None;
    let mut json_out: Option<PathBuf> = None;
    let mut run_id: Option<String> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--log-root" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing path after --log-root"));
                }
                log_root = Some(PathBuf::from(args[idx + 1].clone()));
                args.drain(idx..=idx + 1);
                continue;
            }
            "--json-out" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing path after --json-out"));
                }
                json_out = Some(PathBuf::from(args[idx + 1].clone()));
                args.drain(idx..=idx + 1);
                continue;
            }
            "--run-id" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing value after --run-id"));
                }
                run_id = Some(args[idx + 1].clone());
                args.drain(idx..=idx + 1);
                continue;
            }
            _ => {}
        }
        idx += 1;
    }

    let mut args = args.into_iter();
    let mode = args
        .next()
        .ok_or_else(|| usage("missing mode; expected 'command' or 'protocol'"))?;
    let fixture = args
        .next()
        .ok_or_else(|| usage("missing fixture file name (e.g. core_errors.json)"))?;
    let host = args.next().unwrap_or_else(|| "127.0.0.1".to_string());
    let port = match args.next() {
        Some(text) => text
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{text}'"))?,
        None => 6379,
    };

    Ok(CliArgs {
        mode,
        fixture,
        host,
        port,
        log_root,
        json_out,
        run_id,
    })
}

#[derive(Debug, Serialize)]
struct JsonFailure {
    case_name: String,
    expected: String,
    actual: String,
    detail: Option<String>,
    reason_code: Option<String>,
    replay_cmd: Option<String>,
    artifact_refs: Vec<String>,
}

#[derive(Debug, Serialize)]
struct JsonReport {
    schema_version: String,
    differential_schema_version: String,
    run_id: Option<String>,
    status: String,
    mode: String,
    fixture: String,
    host: String,
    port: u16,
    suite: String,
    total: usize,
    passed: usize,
    pass_rate: f64,
    failed_count: usize,
    total_failures: usize,
    live_log_root: Option<String>,
    reason_code_counts: BTreeMap<String, usize>,
    failed_without_reason_code: usize,
    failures: Vec<JsonFailure>,
    run_error: Option<String>,
}

fn write_json_report(
    path: &Path,
    cli: &CliArgs,
    report: &DifferentialReport,
) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create json report directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let json_report = JsonReport {
        schema_version: "live_oracle_diff_report/v3".to_string(),
        differential_schema_version: report.schema_version.to_string(),
        run_id: cli.run_id.clone(),
        status: "completed".to_string(),
        mode: cli.mode.clone(),
        fixture: cli.fixture.clone(),
        host: cli.host.clone(),
        port: cli.port,
        suite: report.suite.clone(),
        total: report.total,
        passed: report.passed,
        pass_rate: compute_pass_rate(report.passed, report.total),
        failed_count: report.failed.len(),
        total_failures: report.failed.len(),
        live_log_root: cli.log_root.as_ref().map(|root| root.display().to_string()),
        reason_code_counts: report.reason_code_counts.clone(),
        failed_without_reason_code: report.failed_without_reason_code,
        failures: report.failed.iter().map(json_failure).collect(),
        run_error: None,
    };

    let payload = serde_json::to_string_pretty(&json_report)
        .map_err(|err| format!("failed to encode json report: {err}"))?;
    fs::write(path, payload)
        .map_err(|err| format!("failed to write json report {}: {err}", path.display()))
}

fn write_json_error_report(path: &Path, cli: &CliArgs, run_error: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create json report directory {}: {err}",
                parent.display()
            )
        })?;
    }

    let json_report = JsonReport {
        schema_version: "live_oracle_diff_report/v3".to_string(),
        differential_schema_version: DIFFERENTIAL_REPORT_SCHEMA_VERSION.to_string(),
        run_id: cli.run_id.clone(),
        status: "execution_error".to_string(),
        mode: cli.mode.clone(),
        fixture: cli.fixture.clone(),
        host: cli.host.clone(),
        port: cli.port,
        suite: String::new(),
        total: 0,
        passed: 0,
        pass_rate: 0.0,
        failed_count: 0,
        total_failures: 0,
        live_log_root: cli.log_root.as_ref().map(|root| root.display().to_string()),
        reason_code_counts: BTreeMap::new(),
        failed_without_reason_code: 0,
        failures: Vec::new(),
        run_error: Some(run_error.to_string()),
    };
    let payload = serde_json::to_string_pretty(&json_report)
        .map_err(|err| format!("failed to encode json error report: {err}"))?;
    fs::write(path, payload)
        .map_err(|err| format!("failed to write json report {}: {err}", path.display()))
}

fn json_failure(case: &CaseOutcome) -> JsonFailure {
    JsonFailure {
        case_name: case.name.clone(),
        expected: format!("{:?}", case.expected),
        actual: format!("{:?}", case.actual),
        detail: case.detail.clone(),
        reason_code: case.reason_code.clone(),
        replay_cmd: case.replay_cmd.clone(),
        artifact_refs: case.artifact_refs.clone(),
    }
}

fn compute_pass_rate(passed: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        passed as f64 / total as f64
    }
}

fn usage(reason: &str) -> String {
    format!(
        "{reason}\nusage: cargo run -p fr-conformance --bin live_oracle_diff -- [--log-root <path>] [--json-out <path>] [--run-id <id>] <command|protocol> <fixture.json> [host] [port]"
    )
}

#[cfg(test)]
mod tests {
    use fr_protocol::RespFrame;

    use super::{json_failure, parse_args};
    use fr_conformance::CaseOutcome;

    #[test]
    fn parse_args_supports_optional_flags() {
        let parsed = parse_args(vec![
            "--log-root".to_string(),
            "artifacts/logs".to_string(),
            "--json-out".to_string(),
            "artifacts/report.json".to_string(),
            "--run-id".to_string(),
            "run-123".to_string(),
            "protocol".to_string(),
            "protocol_negative.json".to_string(),
            "10.0.0.5".to_string(),
            "6380".to_string(),
        ])
        .expect("arguments parse");

        assert_eq!(parsed.mode, "protocol");
        assert_eq!(parsed.fixture, "protocol_negative.json");
        assert_eq!(parsed.host, "10.0.0.5");
        assert_eq!(parsed.port, 6380);
        assert_eq!(
            parsed.log_root.expect("log root present").to_string_lossy(),
            "artifacts/logs"
        );
        assert_eq!(
            parsed
                .json_out
                .expect("json path present")
                .to_string_lossy(),
            "artifacts/report.json"
        );
        assert_eq!(parsed.run_id.as_deref(), Some("run-123"));
    }

    #[test]
    fn parse_args_defaults_host_and_port() {
        let parsed = parse_args(vec!["command".to_string(), "core_strings.json".to_string()])
            .expect("arguments parse");

        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 6379);
        assert_eq!(parsed.run_id, None);
    }

    #[test]
    fn json_failure_carries_replay_and_artifact_metadata() {
        let failure = CaseOutcome {
            name: "case-1".to_string(),
            passed: false,
            expected: RespFrame::SimpleString("OK".to_string()),
            actual: RespFrame::Error("ERR".to_string()),
            detail: Some("detail".to_string()),
            reason_code: Some("eventloop.accept.maxclients_reached".to_string()),
            replay_cmd: Some("cargo test -p fr-runtime case-1 -- --nocapture".to_string()),
            artifact_refs: vec!["artifacts/logs/case-1.jsonl".to_string()],
        };
        let encoded = json_failure(&failure);
        assert_eq!(
            encoded.replay_cmd.as_deref(),
            Some("cargo test -p fr-runtime case-1 -- --nocapture")
        );
        assert_eq!(
            encoded.artifact_refs,
            vec!["artifacts/logs/case-1.jsonl".to_string()]
        );
    }
}
