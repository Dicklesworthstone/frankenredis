#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone)]
struct CliArgs {
    summary_path: PathBuf,
}

#[derive(Debug, Clone)]
struct GateConfig {
    result_path: PathBuf,
    coverage_floor: f64,
    flake_ceiling: usize,
    hard_fail_ceiling: usize,
    case_failure_ceiling: usize,
    packet_coverage_floors: BTreeMap<String, f64>,
    quarantine_path: PathBuf,
}

#[derive(Debug, Clone)]
struct BudgetEvaluation {
    pass_rate: f64,
    failed_suites: usize,
    total_case_failures: usize,
    flake_suspects: Vec<String>,
    hard_fail_suites: Vec<String>,
    packet_thresholds: Vec<PacketThreshold>,
    primary_reason_codes: Vec<Value>,
    run_id: Value,
    readme_path: Value,
    replay_script: Value,
    quarantine_candidates: Vec<String>,
    violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PacketThreshold {
    packet_id: String,
    pass_rate: f64,
    floor: f64,
}

#[derive(Debug, Serialize)]
struct GateResult {
    schema_version: String,
    bead_id: String,
    status: String,
    summary_path: String,
    thresholds: Thresholds,
    metrics: Metrics,
    violations: Vec<String>,
    remediation: Remediation,
}

#[derive(Debug, Serialize)]
struct Thresholds {
    coverage_floor: f64,
    packet_coverage_floors: BTreeMap<String, f64>,
    flake_ceiling: usize,
    hard_fail_ceiling: usize,
    case_failure_ceiling: usize,
}

#[derive(Debug, Serialize)]
struct Metrics {
    run_id: Value,
    total_suites: usize,
    passed_suites: usize,
    failed_suites: usize,
    pass_rate: f64,
    total_case_failures: usize,
    flake_suspect_suites: Vec<String>,
    hard_fail_suites: Vec<String>,
    packet_family_thresholds: Vec<PacketThreshold>,
    primary_reason_codes: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct Remediation {
    readme_path: Value,
    replay_script: Value,
    quarantine_candidates_path: Option<String>,
    quarantine_candidates: Vec<String>,
    next_steps: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err((message, code)) => {
            if !message.is_empty() {
                eprintln!("{message}");
            }
            ExitCode::from(code)
        }
    }
}

fn run() -> Result<ExitCode, (String, u8)> {
    let cli = match parse_args(env::args().skip(1).collect())? {
        Some(args) => args,
        None => {
            println!("{}", usage());
            return Ok(ExitCode::SUCCESS);
        }
    };

    if !cli.summary_path.is_file() {
        return Err((
            format!("missing summary file: {}", cli.summary_path.display()),
            2,
        ));
    }

    let cfg = load_config(&cli.summary_path).map_err(|err| (err, 2))?;
    let summary = load_summary(&cli.summary_path).map_err(|err| (err, 2))?;
    let evaluation = evaluate_budget(&summary, &cfg);

    if !evaluation.quarantine_candidates.is_empty() {
        write_quarantine_candidates(&cfg.quarantine_path, &evaluation.quarantine_candidates)
            .map_err(|err| {
                (
                    format!(
                        "failed to write quarantine candidates {}: {err}",
                        cfg.quarantine_path.display()
                    ),
                    2,
                )
            })?;
    }

    let status = if evaluation.violations.is_empty() {
        "pass".to_string()
    } else {
        "fail".to_string()
    };

    let result = GateResult {
        schema_version: "live_oracle_budget_gate/v1".to_string(),
        bead_id: "bd-2wb.23".to_string(),
        status: status.clone(),
        summary_path: cli.summary_path.display().to_string(),
        thresholds: Thresholds {
            coverage_floor: cfg.coverage_floor,
            packet_coverage_floors: cfg.packet_coverage_floors.clone(),
            flake_ceiling: cfg.flake_ceiling,
            hard_fail_ceiling: cfg.hard_fail_ceiling,
            case_failure_ceiling: cfg.case_failure_ceiling,
        },
        metrics: Metrics {
            run_id: evaluation.run_id.clone(),
            total_suites: usize_from_json(summary.get("total_suites")),
            passed_suites: usize_from_json(summary.get("passed_suites")),
            failed_suites: evaluation.failed_suites,
            pass_rate: round4(evaluation.pass_rate),
            total_case_failures: evaluation.total_case_failures,
            flake_suspect_suites: evaluation.flake_suspects.clone(),
            hard_fail_suites: evaluation.hard_fail_suites.clone(),
            packet_family_thresholds: evaluation.packet_thresholds.clone(),
            primary_reason_codes: evaluation.primary_reason_codes.clone(),
        },
        violations: evaluation.violations.clone(),
        remediation: Remediation {
            readme_path: evaluation.readme_path.clone(),
            replay_script: evaluation.replay_script.clone(),
            quarantine_candidates_path: if evaluation.quarantine_candidates.is_empty() {
                None
            } else {
                Some(cfg.quarantine_path.display().to_string())
            },
            quarantine_candidates: evaluation.quarantine_candidates.clone(),
            next_steps: vec![
                "Inspect suite_status.tsv and suite report JSON files under run_root.".to_string(),
                "Run replay_failed.sh to reproduce failing suites deterministically.".to_string(),
                "Use per-suite reason_code_counts to route ownership and open/advance packet beads."
                    .to_string(),
            ],
        },
    };

    if let Some(parent) = cfg.result_path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            (
                format!(
                    "failed to create result directory {}: {err}",
                    parent.display()
                ),
                2,
            )
        })?;
    }
    let payload = serde_json::to_string_pretty(&result).map_err(|err| {
        (
            format!(
                "failed to serialize budget result {}: {err}",
                cfg.result_path.display()
            ),
            2,
        )
    })?;
    fs::write(&cfg.result_path, format!("{payload}\n")).map_err(|err| {
        (
            format!(
                "failed to write budget result {}: {err}",
                cfg.result_path.display()
            ),
            2,
        )
    })?;

    println!("budget_result: {}", cfg.result_path.display());
    println!("status: {status}");
    println!(
        "metrics: pass_rate={:.4}, failed_suites={}, flake_suspects={}, hard_fails={}, total_case_failures={}",
        evaluation.pass_rate,
        evaluation.failed_suites,
        evaluation.flake_suspects.len(),
        evaluation.hard_fail_suites.len(),
        evaluation.total_case_failures
    );
    if !evaluation.quarantine_candidates.is_empty() {
        println!("quarantine_candidates: {}", cfg.quarantine_path.display());
    }
    if !evaluation.violations.is_empty() {
        println!("violations:");
        for violation in &evaluation.violations {
            println!("- {violation}");
        }
        println!("remediation:");
        println!("- readme: {}", value_display(&evaluation.readme_path));
        println!("- replay: {}", value_display(&evaluation.replay_script));
        return Ok(ExitCode::from(1));
    }

    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<Option<CliArgs>, (String, u8)> {
    if raw_args.len() == 1 && matches!(raw_args[0].as_str(), "-h" | "--help") {
        return Ok(None);
    }
    if raw_args.len() != 1 {
        return Err((usage(), 2));
    }
    Ok(Some(CliArgs {
        summary_path: PathBuf::from(&raw_args[0]),
    }))
}

fn usage() -> String {
    "Usage:\n  cargo run -p fr-conformance --bin live_oracle_budget_gate -- <coverage_summary.json>\n\nDescription:\n  Enforces bd-2wb.23 reliability budgets against the machine-readable\n  coverage summary emitted by scripts/run_live_oracle_diff.sh.\n\nBudget knobs (env):\n  FR_COVERAGE_FLOOR        Minimum suite pass_rate ratio (default: 0.95)\n  FR_PACKET_COVERAGE_FLOORS_JSON\n                           Optional JSON map for per-packet floors\n                           (example: {\"FR-P2C-002\":0.99,\"FR-P2C-003\":0.97})\n  FR_FLAKE_CEILING         Max allowed flake_suspect_suites count (default: 0)\n  FR_HARD_FAIL_CEILING     Max allowed hard_fail_suites count (default: 0)\n  FR_CASE_FAILURE_CEILING  Max allowed total_case_failures (default: 0)\n  FR_BUDGET_RESULT_PATH    Optional output JSON path for gate result\n  FR_QUARANTINE_PATH       Optional path for flake quarantine candidate list"
        .to_string()
}

fn load_config(summary_path: &Path) -> Result<GateConfig, String> {
    let summary_dir = summary_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);

    let result_path = env::var("FR_BUDGET_RESULT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| summary_dir.join("reliability_budget_result.json"));
    let quarantine_path = env::var("FR_QUARANTINE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| summary_dir.join("flake_quarantine_candidates.txt"));
    let coverage_floor = parse_f64_env("FR_COVERAGE_FLOOR", 0.95)?;
    let flake_ceiling = parse_usize_env("FR_FLAKE_CEILING", 0)?;
    let hard_fail_ceiling = parse_usize_env("FR_HARD_FAIL_CEILING", 0)?;
    let case_failure_ceiling = parse_usize_env("FR_CASE_FAILURE_CEILING", 0)?;
    let packet_floors_raw =
        env::var("FR_PACKET_COVERAGE_FLOORS_JSON").unwrap_or_else(|_| "{}".to_string());
    let packet_coverage_floors = parse_packet_coverage_floors(&packet_floors_raw)?;

    Ok(GateConfig {
        result_path,
        coverage_floor,
        flake_ceiling,
        hard_fail_ceiling,
        case_failure_ceiling,
        packet_coverage_floors,
        quarantine_path,
    })
}

fn load_summary(path: &Path) -> Result<Value, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read summary {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse summary {}: {err}", path.display()))
}

fn parse_f64_env(name: &str, default: f64) -> Result<f64, String> {
    match env::var(name) {
        Ok(raw) => raw
            .parse::<f64>()
            .map_err(|err| format!("invalid {name}: {err}")),
        Err(_) => Ok(default),
    }
}

fn parse_usize_env(name: &str, default: usize) -> Result<usize, String> {
    match env::var(name) {
        Ok(raw) => raw
            .parse::<usize>()
            .map_err(|err| format!("invalid {name}: {err}")),
        Err(_) => Ok(default),
    }
}

fn parse_packet_coverage_floors(raw: &str) -> Result<BTreeMap<String, f64>, String> {
    let decoded: Value = serde_json::from_str(raw)
        .map_err(|err| format!("invalid FR_PACKET_COVERAGE_FLOORS_JSON: {err}"))?;
    let map = decoded
        .as_object()
        .ok_or_else(|| "FR_PACKET_COVERAGE_FLOORS_JSON must decode to a JSON object".to_string())?;

    let mut floors = BTreeMap::new();
    for (key, value) in map {
        let floor = value
            .as_f64()
            .ok_or_else(|| format!("invalid packet coverage floor for {key}: expected number"))?;
        floors.insert(key.clone(), floor);
    }
    Ok(floors)
}

fn evaluate_budget(summary: &Value, cfg: &GateConfig) -> BudgetEvaluation {
    let pass_rate = f64_from_json(summary.get("pass_rate"));
    let failed_suites = usize_from_json(summary.get("failed_suites"));
    let total_case_failures = usize_from_json(summary.get("total_case_failures"));
    let flake_suspects = string_vec(summary.get("flake_suspect_suites"));
    let hard_fail_suites = string_vec(summary.get("hard_fail_suites"));

    let mut violations = Vec::new();
    if pass_rate + 1e-9 < cfg.coverage_floor {
        violations.push(format!(
            "coverage floor violated: pass_rate={pass_rate:.4} < floor={:.4}",
            cfg.coverage_floor
        ));
    }
    if flake_suspects.len() > cfg.flake_ceiling {
        violations.push(format!(
            "flake ceiling violated: flake_suspect_suites={} > ceiling={}",
            flake_suspects.len(),
            cfg.flake_ceiling
        ));
    }
    if hard_fail_suites.len() > cfg.hard_fail_ceiling {
        violations.push(format!(
            "hard-fail ceiling violated: hard_fail_suites={} > ceiling={}",
            hard_fail_suites.len(),
            cfg.hard_fail_ceiling
        ));
    }
    if total_case_failures > cfg.case_failure_ceiling {
        violations.push(format!(
            "case-failure ceiling violated: total_case_failures={} > ceiling={}",
            total_case_failures, cfg.case_failure_ceiling
        ));
    }

    let mut packet_thresholds = Vec::new();
    if let Some(items) = summary
        .get("packet_family_pass_rates")
        .and_then(Value::as_array)
    {
        for packet in items {
            let packet_id = packet
                .get("packet_id")
                .map(value_display)
                .unwrap_or_else(|| "None".to_string());
            let packet_pass_rate = f64_from_json(packet.get("pass_rate"));
            let packet_floor = cfg
                .packet_coverage_floors
                .get(&packet_id)
                .copied()
                .unwrap_or(cfg.coverage_floor);

            packet_thresholds.push(PacketThreshold {
                packet_id: packet_id.clone(),
                pass_rate: round4(packet_pass_rate),
                floor: round4(packet_floor),
            });
            if packet_pass_rate + 1e-9 < packet_floor {
                violations.push(format!(
                    "packet coverage floor violated: packet={packet_id} pass_rate={packet_pass_rate:.4} < floor={packet_floor:.4}"
                ));
            }
        }
    }

    let mut quarantine_set = BTreeSet::new();
    for suite in &flake_suspects {
        quarantine_set.insert(suite.clone());
    }
    let quarantine_candidates = quarantine_set.into_iter().collect();

    BudgetEvaluation {
        pass_rate,
        failed_suites,
        total_case_failures,
        flake_suspects,
        hard_fail_suites,
        packet_thresholds,
        primary_reason_codes: value_array(summary.get("primary_reason_codes")),
        run_id: summary.get("run_id").cloned().unwrap_or(Value::Null),
        readme_path: summary.get("readme_path").cloned().unwrap_or(Value::Null),
        replay_script: summary.get("replay_script").cloned().unwrap_or(Value::Null),
        quarantine_candidates,
        violations,
    }
}

fn value_display(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn string_vec(value: Option<&Value>) -> Vec<String> {
    match value.and_then(Value::as_array) {
        Some(items) => items.iter().map(value_display).collect(),
        None => Vec::new(),
    }
}

fn value_array(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn usize_from_json(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

fn f64_from_json(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn write_quarantine_candidates(path: &Path, suites: &[String]) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut payload = suites.join("\n");
    payload.push('\n');
    fs::write(path, payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> GateConfig {
        GateConfig {
            result_path: PathBuf::from("out.json"),
            coverage_floor: 0.95,
            flake_ceiling: 0,
            hard_fail_ceiling: 0,
            case_failure_ceiling: 0,
            packet_coverage_floors: BTreeMap::new(),
            quarantine_path: PathBuf::from("quarantine.txt"),
        }
    }

    #[test]
    fn evaluate_budget_passes_when_thresholds_hold() {
        let summary = json!({
            "pass_rate": 1.0,
            "failed_suites": 0,
            "total_case_failures": 0,
            "flake_suspect_suites": [],
            "hard_fail_suites": [],
            "packet_family_pass_rates": [
                { "packet_id": "FR-P2C-001", "pass_rate": 1.0 }
            ]
        });

        let evaluation = evaluate_budget(&summary, &test_config());
        assert!(evaluation.violations.is_empty());
        assert!(evaluation.quarantine_candidates.is_empty());
    }

    #[test]
    fn evaluate_budget_detects_packet_floor_violation() {
        let mut cfg = test_config();
        cfg.packet_coverage_floors
            .insert("FR-P2C-002".to_string(), 0.99);
        let summary = json!({
            "pass_rate": 0.99,
            "failed_suites": 1,
            "total_case_failures": 2,
            "flake_suspect_suites": [],
            "hard_fail_suites": [],
            "packet_family_pass_rates": [
                { "packet_id": "FR-P2C-002", "pass_rate": 0.90 }
            ]
        });

        let evaluation = evaluate_budget(&summary, &cfg);
        assert!(
            evaluation
                .violations
                .iter()
                .any(|line| line.contains("packet coverage floor violated"))
        );
    }

    #[test]
    fn evaluate_budget_deduplicates_quarantine_candidates() {
        let summary = json!({
            "pass_rate": 1.0,
            "failed_suites": 0,
            "total_case_failures": 0,
            "flake_suspect_suites": ["suite_b", "suite_a", "suite_b"],
            "hard_fail_suites": [],
            "packet_family_pass_rates": []
        });

        let evaluation = evaluate_budget(&summary, &test_config());
        assert_eq!(
            evaluation.quarantine_candidates,
            vec!["suite_a".to_string(), "suite_b".to_string()]
        );
    }

    #[test]
    fn parse_packet_coverage_floors_requires_object() {
        let err = parse_packet_coverage_floors("[]").expect_err("must reject non-object");
        assert_eq!(
            err,
            "FR_PACKET_COVERAGE_FLOORS_JSON must decode to a JSON object"
        );
    }
}
