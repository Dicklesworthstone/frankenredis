#![forbid(unsafe_code)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    status_tsv: String,
    run_id: String,
    host: String,
    port: u16,
    runner: String,
    run_root: String,
    readme_path: String,
    replay_script: String,
    replay_all_script: String,
    coverage_summary_out: String,
    failure_envelope_out: String,
    run_seed: u64,
    run_fingerprint: String,
}

#[derive(Debug, Clone)]
struct StatusRow {
    suite: String,
    mode: String,
    fixture: String,
    scenario_class: String,
    exit_code: i32,
    report_json: String,
    stdout_log: String,
}

#[derive(Debug, Clone)]
struct AggregationState {
    suite_results: Vec<SuiteResultRow>,
    reason_counts: HashMap<String, usize>,
    total_case_failures: usize,
    flake_suspects: Vec<String>,
    hard_fail_suites: Vec<String>,
    packet_totals: HashMap<String, PassTotals>,
    scenario_totals: HashMap<String, PassTotals>,
    scenario_classes_seen: HashSet<String>,
    failure_rows: Vec<FailureRow>,
    artifact_index: HashMap<String, Vec<ArtifactFailureRef>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PassTotals {
    total_suites: usize,
    passed_suites: usize,
}

#[derive(Debug, Clone, Serialize)]
struct PacketFamilyPassRate {
    packet_id: String,
    total_suites: usize,
    passed_suites: usize,
    failed_suites: usize,
    pass_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ScenarioClassPassRate {
    scenario_class: String,
    total_suites: usize,
    passed_suites: usize,
    failed_suites: usize,
    pass_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
struct PrimaryReasonCode {
    reason_code: String,
    count: usize,
}

#[derive(Debug, Clone, Serialize)]
struct SuiteResultRow {
    suite: String,
    mode: String,
    fixture: String,
    packet_id: String,
    scenario_class: String,
    exit_code: i32,
    report_json: String,
    stdout_log: String,
    report_status: String,
    failed_count: usize,
    pass_rate: f64,
    run_error: Value,
    reason_code_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
struct FailureRow {
    suite: String,
    fixture: String,
    packet_id: String,
    scenario_class: String,
    case_name: String,
    reason_code: Value,
    detail: Value,
    replay_cmd: Value,
    artifact_refs: Vec<String>,
    report_json: String,
    stdout_log: String,
    live_log_root: Value,
    run_seed: u64,
    run_fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
struct ArtifactFailureRef {
    suite: String,
    case_name: String,
    reason_code: Value,
    replay_cmd: Value,
}

#[derive(Debug, Clone, Serialize)]
struct ArtifactIndexEntry {
    artifact_ref: String,
    failure_count: usize,
    failures: Vec<ArtifactFailureRef>,
}

#[derive(Debug, Clone, Serialize)]
struct CoverageSummary {
    schema_version: String,
    run_id: String,
    host: String,
    port: u16,
    runner: String,
    run_seed: u64,
    run_fingerprint: String,
    run_root: String,
    status_tsv: String,
    readme_path: String,
    replay_script: String,
    replay_all_script: String,
    failure_envelope: String,
    total_suites: usize,
    passed_suites: usize,
    failed_suites: usize,
    pass_rate: f64,
    total_case_failures: usize,
    packet_family_pass_rates: Vec<PacketFamilyPassRate>,
    scenario_class_pass_rates: Vec<ScenarioClassPassRate>,
    required_scenario_classes: Vec<String>,
    missing_scenario_classes: Vec<String>,
    scenario_matrix_complete: bool,
    flake_suspect_suites: Vec<String>,
    hard_fail_suites: Vec<String>,
    primary_reason_codes: Vec<PrimaryReasonCode>,
    suite_results: Vec<SuiteResultRow>,
}

#[derive(Debug, Clone, Serialize)]
struct FailureEnvelope {
    schema_version: String,
    run_id: String,
    run_seed: u64,
    run_fingerprint: String,
    run_root: String,
    total_failures: usize,
    failures: Vec<FailureRow>,
    artifact_index: Vec<ArtifactIndexEntry>,
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
    let status_rows = load_status_rows(&cli.status_tsv)?;
    let mut state = AggregationState {
        suite_results: Vec::new(),
        reason_counts: HashMap::new(),
        total_case_failures: 0,
        flake_suspects: Vec::new(),
        hard_fail_suites: Vec::new(),
        packet_totals: HashMap::new(),
        scenario_totals: HashMap::new(),
        scenario_classes_seen: HashSet::new(),
        failure_rows: Vec::new(),
        artifact_index: HashMap::new(),
    };

    for row in &status_rows {
        aggregate_row(&cli, row, &mut state);
    }

    let total_suites = state.suite_results.len();
    let passed_suites = state
        .suite_results
        .iter()
        .filter(|row| row.exit_code == 0)
        .count();
    let failed_suites = total_suites.saturating_sub(passed_suites);

    let packet_family_pass_rates = build_packet_rates(&state.packet_totals);
    let scenario_class_pass_rates = build_scenario_rates(&state.scenario_totals);
    let required_scenario_classes = vec![
        "failure_injection".to_string(),
        "golden".to_string(),
        "regression".to_string(),
    ];
    let missing_scenario_classes = required_scenario_classes
        .iter()
        .filter(|required| !state.scenario_classes_seen.contains(*required))
        .cloned()
        .collect::<Vec<_>>();
    let scenario_matrix_complete = missing_scenario_classes.is_empty();

    let mut flake_suspect_suites = dedup_sorted(&state.flake_suspects);
    let mut hard_fail_suites = dedup_sorted(&state.hard_fail_suites);
    let primary_reason_codes = build_primary_reason_codes(&state.reason_counts);
    let pass_rate = if total_suites == 0 {
        0.0
    } else {
        round4(passed_suites as f64 / total_suites as f64)
    };

    let mut suite_results = state.suite_results;
    suite_results.sort_by(|left, right| left.suite.cmp(&right.suite));

    let coverage_summary = CoverageSummary {
        schema_version: "live_oracle_coverage_summary/v1".to_string(),
        run_id: cli.run_id.clone(),
        host: cli.host.clone(),
        port: cli.port,
        runner: cli.runner.clone(),
        run_seed: cli.run_seed,
        run_fingerprint: cli.run_fingerprint.clone(),
        run_root: cli.run_root.clone(),
        status_tsv: cli.status_tsv.clone(),
        readme_path: cli.readme_path.clone(),
        replay_script: cli.replay_script.clone(),
        replay_all_script: cli.replay_all_script.clone(),
        failure_envelope: cli.failure_envelope_out.clone(),
        total_suites,
        passed_suites,
        failed_suites,
        pass_rate,
        total_case_failures: state.total_case_failures,
        packet_family_pass_rates,
        scenario_class_pass_rates,
        required_scenario_classes,
        missing_scenario_classes,
        scenario_matrix_complete,
        flake_suspect_suites: std::mem::take(&mut flake_suspect_suites),
        hard_fail_suites: std::mem::take(&mut hard_fail_suites),
        primary_reason_codes,
        suite_results,
    };

    let mut failures = state.failure_rows;
    failures.sort_by(|left, right| {
        left.suite
            .cmp(&right.suite)
            .then_with(|| left.case_name.cmp(&right.case_name))
    });
    let artifact_index = build_artifact_index(state.artifact_index);
    let failure_envelope = FailureEnvelope {
        schema_version: "live_oracle_failure_envelope/v1".to_string(),
        run_id: cli.run_id.clone(),
        run_seed: cli.run_seed,
        run_fingerprint: cli.run_fingerprint.clone(),
        run_root: cli.run_root.clone(),
        total_failures: failures.len(),
        failures,
        artifact_index,
    };

    write_json_file(&cli.coverage_summary_out, &coverage_summary)?;
    write_json_file(&cli.failure_envelope_out, &failure_envelope)?;

    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<CliArgs, String> {
    if raw_args.len() == 1 && matches!(raw_args[0].as_str(), "-h" | "--help") {
        return Err(usage(String::new()));
    }

    let mut status_tsv = None;
    let mut run_id = None;
    let mut host = None;
    let mut port = None;
    let mut runner = None;
    let mut run_root = None;
    let mut readme_path = None;
    let mut replay_script = None;
    let mut replay_all_script = None;
    let mut coverage_summary_out = None;
    let mut failure_envelope_out = None;
    let mut run_seed = None;
    let mut run_fingerprint = None;

    let mut idx = 0;
    while idx < raw_args.len() {
        let key = &raw_args[idx];
        let value = raw_args
            .get(idx + 1)
            .ok_or_else(|| usage(format!("missing value after {key}")))?;
        match key.as_str() {
            "--status-tsv" => status_tsv = Some(value.clone()),
            "--run-id" => run_id = Some(value.clone()),
            "--host" => host = Some(value.clone()),
            "--port" => {
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|err| format!("invalid --port value {value}: {err}"))?,
                )
            }
            "--runner" => runner = Some(value.clone()),
            "--run-root" => run_root = Some(value.clone()),
            "--readme-path" => readme_path = Some(value.clone()),
            "--replay-script" => replay_script = Some(value.clone()),
            "--replay-all-script" => replay_all_script = Some(value.clone()),
            "--coverage-summary-out" => coverage_summary_out = Some(value.clone()),
            "--failure-envelope-out" => failure_envelope_out = Some(value.clone()),
            "--run-seed" => {
                run_seed = Some(
                    value
                        .parse::<u64>()
                        .map_err(|err| format!("invalid --run-seed value {value}: {err}"))?,
                )
            }
            "--run-fingerprint" => run_fingerprint = Some(value.clone()),
            _ => return Err(usage(format!("unknown argument: {key}"))),
        }
        idx += 2;
    }

    Ok(CliArgs {
        status_tsv: required_arg("--status-tsv", status_tsv)?,
        run_id: required_arg("--run-id", run_id)?,
        host: required_arg("--host", host)?,
        port: required_arg("--port", port)?,
        runner: required_arg("--runner", runner)?,
        run_root: required_arg("--run-root", run_root)?,
        readme_path: required_arg("--readme-path", readme_path)?,
        replay_script: required_arg("--replay-script", replay_script)?,
        replay_all_script: required_arg("--replay-all-script", replay_all_script)?,
        coverage_summary_out: required_arg("--coverage-summary-out", coverage_summary_out)?,
        failure_envelope_out: required_arg("--failure-envelope-out", failure_envelope_out)?,
        run_seed: required_arg("--run-seed", run_seed)?,
        run_fingerprint: required_arg("--run-fingerprint", run_fingerprint)?,
    })
}

fn required_arg<T>(flag: &str, value: Option<T>) -> Result<T, String> {
    value.ok_or_else(|| usage(format!("missing required argument: {flag}")))
}

fn usage(reason: String) -> String {
    let prefix = if reason.is_empty() {
        String::new()
    } else {
        format!("{reason}\n")
    };
    format!(
        "{prefix}usage: cargo run -p fr-conformance --bin live_oracle_bundle_summarizer -- \
--status-tsv <path> --run-id <id> --host <host> --port <port> --runner <runner> --run-root <path> \
--readme-path <path> --replay-script <path> --replay-all-script <path> \
--coverage-summary-out <path> --failure-envelope-out <path> \
--run-seed <u64> --run-fingerprint <sha256>"
    )
}

fn load_status_rows(path: &str) -> Result<Vec<StatusRow>, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read status tsv {path}: {err}"))?;
    let mut lines = raw.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("status tsv is empty: {path}"))?;
    let header_fields = header
        .split('\t')
        .enumerate()
        .map(|(idx, col)| (col.to_string(), idx))
        .collect::<HashMap<_, _>>();

    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let cols = line.split('\t').collect::<Vec<_>>();
        let suite = get_col(&header_fields, "suite", &cols);
        if suite.is_empty() {
            continue;
        }
        rows.push(StatusRow {
            suite,
            mode: get_col(&header_fields, "mode", &cols),
            fixture: get_col(&header_fields, "fixture", &cols),
            scenario_class: get_col(&header_fields, "scenario_class", &cols),
            exit_code: get_col(&header_fields, "exit_code", &cols)
                .parse::<i32>()
                .unwrap_or(1),
            report_json: get_col(&header_fields, "report_json", &cols),
            stdout_log: get_col(&header_fields, "stdout_log", &cols),
        });
    }
    Ok(rows)
}

fn get_col(header: &HashMap<String, usize>, name: &str, cols: &[&str]) -> String {
    header
        .get(name)
        .and_then(|idx| cols.get(*idx))
        .map_or_else(String::new, |raw| (*raw).to_string())
}

fn aggregate_row(cli: &CliArgs, row: &StatusRow, state: &mut AggregationState) {
    let report = load_report_json(&row.report_json);
    let report_status = value_to_string(report.get("status"), "missing_report");
    let failed_count = value_to_usize(report.get("failed_count"));
    let report_pass_rate = round4(value_to_f64(report.get("pass_rate")));
    let run_error = report.get("run_error").cloned().unwrap_or(Value::Null);
    let reason_code_counts = parse_reason_counts(report.get("reason_code_counts"));

    for (reason_code, count) in &reason_code_counts {
        *state.reason_counts.entry(reason_code.clone()).or_insert(0) += *count;
    }

    let packet_id = packet_id_for_fixture(&row.fixture).to_string();
    let scenario_class = if row.scenario_class.is_empty() {
        "unspecified".to_string()
    } else {
        row.scenario_class.clone()
    };
    state.scenario_classes_seen.insert(scenario_class.clone());

    for failure in value_to_array(report.get("failures")) {
        let case_name = value_to_string(failure.get("case_name"), "");
        let reason_code = failure.get("reason_code").cloned().unwrap_or(Value::Null);
        let replay_cmd = failure.get("replay_cmd").cloned().unwrap_or(Value::Null);
        let artifact_refs = value_to_array(failure.get("artifact_refs"))
            .into_iter()
            .map(|value| value_to_string(Some(&value), ""))
            .filter(|artifact| !artifact.trim().is_empty())
            .collect::<Vec<_>>();

        let detail = failure.get("detail").cloned().unwrap_or(Value::Null);
        let failure_row = FailureRow {
            suite: row.suite.clone(),
            fixture: row.fixture.clone(),
            packet_id: packet_id.clone(),
            scenario_class: scenario_class.clone(),
            case_name: case_name.clone(),
            reason_code: reason_code.clone(),
            detail,
            replay_cmd: replay_cmd.clone(),
            artifact_refs: artifact_refs.clone(),
            report_json: row.report_json.clone(),
            stdout_log: row.stdout_log.clone(),
            live_log_root: report.get("live_log_root").cloned().unwrap_or(Value::Null),
            run_seed: cli.run_seed,
            run_fingerprint: cli.run_fingerprint.clone(),
        };
        state.failure_rows.push(failure_row);

        for artifact_ref in artifact_refs {
            state
                .artifact_index
                .entry(artifact_ref)
                .or_default()
                .push(ArtifactFailureRef {
                    suite: row.suite.clone(),
                    case_name: case_name.clone(),
                    reason_code: reason_code.clone(),
                    replay_cmd: replay_cmd.clone(),
                });
        }
    }

    state.total_case_failures += failed_count;
    let packet_totals = state.packet_totals.entry(packet_id.clone()).or_default();
    packet_totals.total_suites += 1;

    let scenario_totals = state
        .scenario_totals
        .entry(scenario_class.clone())
        .or_default();
    scenario_totals.total_suites += 1;

    if row.exit_code == 0 {
        packet_totals.passed_suites += 1;
        scenario_totals.passed_suites += 1;
    } else if report_status == "execution_error" {
        state.hard_fail_suites.push(row.suite.clone());
    } else {
        state.flake_suspects.push(row.suite.clone());
    }

    state.suite_results.push(SuiteResultRow {
        suite: row.suite.clone(),
        mode: row.mode.clone(),
        fixture: row.fixture.clone(),
        packet_id,
        scenario_class,
        exit_code: row.exit_code,
        report_json: row.report_json.clone(),
        stdout_log: row.stdout_log.clone(),
        report_status,
        failed_count,
        pass_rate: report_pass_rate,
        run_error,
        reason_code_counts,
    });
}

fn load_report_json(path: &str) -> Value {
    if path.is_empty() || !Path::new(path).is_file() {
        return json!({});
    }
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(report) => report,
            Err(err) => json!({
                "status": "execution_error",
                "run_error": format!("report_parse_error:{err}"),
            }),
        },
        Err(err) => json!({
            "status": "execution_error",
            "run_error": format!("report_parse_error:{err}"),
        }),
    }
}

fn packet_id_for_fixture(fixture: &str) -> &'static str {
    match fixture {
        "fr_p2c_001_eventloop_journey.json" => "FR-P2C-001",
        "protocol_negative.json" => "FR-P2C-002",
        "persist_replay.json" => "FR-P2C-005",
        _ => "FR-P2C-003",
    }
}

fn build_packet_rates(packet_totals: &HashMap<String, PassTotals>) -> Vec<PacketFamilyPassRate> {
    let mut keys = packet_totals.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys.into_iter()
        .map(|packet_id| {
            let totals = packet_totals.get(&packet_id).copied().unwrap_or_default();
            let failed = totals.total_suites.saturating_sub(totals.passed_suites);
            let pass_rate = if totals.total_suites == 0 {
                0.0
            } else {
                round4(totals.passed_suites as f64 / totals.total_suites as f64)
            };
            PacketFamilyPassRate {
                packet_id,
                total_suites: totals.total_suites,
                passed_suites: totals.passed_suites,
                failed_suites: failed,
                pass_rate,
            }
        })
        .collect()
}

fn build_scenario_rates(
    scenario_totals: &HashMap<String, PassTotals>,
) -> Vec<ScenarioClassPassRate> {
    let mut keys = scenario_totals.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys.into_iter()
        .map(|scenario_class| {
            let totals = scenario_totals
                .get(&scenario_class)
                .copied()
                .unwrap_or_default();
            let failed = totals.total_suites.saturating_sub(totals.passed_suites);
            let pass_rate = if totals.total_suites == 0 {
                0.0
            } else {
                round4(totals.passed_suites as f64 / totals.total_suites as f64)
            };
            ScenarioClassPassRate {
                scenario_class,
                total_suites: totals.total_suites,
                passed_suites: totals.passed_suites,
                failed_suites: failed,
                pass_rate,
            }
        })
        .collect()
}

fn build_primary_reason_codes(reason_counts: &HashMap<String, usize>) -> Vec<PrimaryReasonCode> {
    let mut rows = reason_counts
        .iter()
        .map(|(reason_code, count)| PrimaryReasonCode {
            reason_code: reason_code.clone(),
            count: *count,
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| (Reverse(row.count), row.reason_code.clone()));
    rows
}

fn dedup_sorted(items: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for item in items {
        set.insert(item.clone());
    }
    set.into_iter().collect()
}

fn build_artifact_index(
    mut artifact_index: HashMap<String, Vec<ArtifactFailureRef>>,
) -> Vec<ArtifactIndexEntry> {
    let mut keys = artifact_index.keys().cloned().collect::<Vec<_>>();
    keys.sort();

    let mut rows = Vec::new();
    for artifact_ref in keys {
        let mut failures = artifact_index.remove(&artifact_ref).unwrap_or_default();
        failures.sort_by(|left, right| {
            left.suite
                .cmp(&right.suite)
                .then_with(|| left.case_name.cmp(&right.case_name))
        });
        rows.push(ArtifactIndexEntry {
            artifact_ref,
            failure_count: failures.len(),
            failures,
        });
    }
    rows
}

fn parse_reason_counts(value: Option<&Value>) -> BTreeMap<String, usize> {
    let mut parsed = BTreeMap::new();
    if let Some(map) = value.and_then(Value::as_object) {
        for (reason_code, count_value) in map {
            parsed.insert(reason_code.clone(), value_to_usize(Some(count_value)));
        }
    }
    parsed
}

fn value_to_array(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn value_to_string(value: Option<&Value>, default: &str) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) => default.to_string(),
        Some(other) => other.to_string(),
        None => default.to_string(),
    }
}

fn value_to_usize(value: Option<&Value>) -> usize {
    value
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

fn value_to_f64(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn write_json_file<T: Serialize>(path: &str, payload: &T) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create directory {}: {err}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(payload)
        .map_err(|err| format!("failed to serialize json output {path}: {err}"))?;
    fs::write(path, format!("{raw}\n")).map_err(|err| format!("failed to write {path}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_id_mapping_matches_contract() {
        assert_eq!(
            packet_id_for_fixture("fr_p2c_001_eventloop_journey.json"),
            "FR-P2C-001"
        );
        assert_eq!(
            packet_id_for_fixture("protocol_negative.json"),
            "FR-P2C-002"
        );
        assert_eq!(packet_id_for_fixture("persist_replay.json"), "FR-P2C-005");
        assert_eq!(packet_id_for_fixture("core_errors.json"), "FR-P2C-003");
    }

    #[test]
    fn primary_reason_codes_sort_by_count_then_key() {
        let mut counts = HashMap::new();
        counts.insert("z".to_string(), 1);
        counts.insert("a".to_string(), 2);
        counts.insert("b".to_string(), 2);
        let rows = build_primary_reason_codes(&counts);
        let pairs = rows
            .into_iter()
            .map(|row| (row.reason_code, row.count))
            .collect::<Vec<_>>();
        assert_eq!(
            pairs,
            vec![
                ("a".to_string(), 2),
                ("b".to_string(), 2),
                ("z".to_string(), 1)
            ]
        );
    }

    #[test]
    fn dedup_sorted_is_stable() {
        let result = dedup_sorted(&[
            "suite_b".to_string(),
            "suite_a".to_string(),
            "suite_b".to_string(),
        ]);
        assert_eq!(result, vec!["suite_a".to_string(), "suite_b".to_string()]);
    }
}
