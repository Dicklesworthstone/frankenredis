#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fr_conformance::log_contract::{PACKET_FAMILIES, StructuredLogEvent};
use serde::{Deserialize, Serialize};

const USER_WORKFLOW_CORPUS_SCHEMA_VERSION: &str = "user_workflow_corpus/v1";
const USER_WORKFLOW_CORPUS_REPORT_SCHEMA_VERSION: &str = "user_workflow_corpus_report/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    manifest: PathBuf,
    json_out: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowCorpus {
    schema_version: String,
    corpus_id: String,
    generated_at_utc: String,
    log_manifest_path: String,
    journeys: Vec<WorkflowJourney>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowJourney {
    journey_id: String,
    packet_id: String,
    description: String,
    golden_log_path: String,
    unit_hook: WorkflowHook,
    differential_hook: DifferentialHook,
    e2e_hook: WorkflowHook,
    stable_reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowHook {
    suite_id: String,
    test_or_scenario_id: String,
    replay_cmd: String,
    owner_bead: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DifferentialStatus {
    Active,
    Planned,
}

#[derive(Debug, Clone, Deserialize)]
struct DifferentialHook {
    status: DifferentialStatus,
    hook_mode: String,
    fixtures: Vec<String>,
    command: String,
    owner_bead: String,
    notes: String,
}

#[derive(Debug, Clone, Serialize)]
struct PacketJourneyCoverage {
    packet_id: String,
    journey_id: String,
    differential_status: DifferentialStatus,
    differential_owner_bead: String,
    differential_fixtures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowCorpusReport {
    schema_version: String,
    corpus_id: String,
    manifest_path: String,
    journey_count: usize,
    active_differential_count: usize,
    planned_differential_count: usize,
    packet_coverage: Vec<PacketJourneyCoverage>,
    violations: Vec<String>,
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
    let corpus = load_corpus(&cli.manifest)?;
    let report = validate_corpus(&corpus, &cli.manifest);

    println!("corpus_id: {}", report.corpus_id);
    println!("journey_count: {}", report.journey_count);
    println!(
        "differential_hooks: active={} planned={}",
        report.active_differential_count, report.planned_differential_count
    );

    if let Some(path) = &cli.json_out {
        write_json_report(path, &report)?;
        println!("json_report: {}", path.display());
    }

    if !report.violations.is_empty() {
        println!("violations:");
        for violation in &report.violations {
            println!("- {violation}");
        }
        return Ok(ExitCode::from(1));
    }

    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<CliArgs, String> {
    let mut args = raw_args;
    let mut manifest = default_manifest_path();
    let mut json_out: Option<PathBuf> = None;

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
            "--json-out" => {
                if idx + 1 >= args.len() {
                    return Err(usage("missing path after --json-out"));
                }
                json_out = Some(PathBuf::from(args[idx + 1].clone()));
                args.drain(idx..=idx + 1);
                continue;
            }
            "-h" | "--help" => {
                return Err(usage("help requested"));
            }
            _ => {}
        }
        idx += 1;
    }

    Ok(CliArgs { manifest, json_out })
}

fn usage(reason: &str) -> String {
    format!(
        "{reason}\nusage: cargo run -p fr-conformance --bin user_journey_corpus_gate -- [--manifest <path>] [--json-out <path>]"
    )
}

fn default_manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/user_workflow_corpus_v1.json")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_repo_path(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo_root().join(path)
    }
}

fn load_corpus(path: &Path) -> Result<WorkflowCorpus, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read manifest {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid manifest JSON {}: {err}", path.display()))
}

fn write_json_report(path: &Path, report: &WorkflowCorpusReport) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create report directory {}: {err}",
                parent.display()
            )
        })?;
    }
    let payload = serde_json::to_string_pretty(report)
        .map_err(|err| format!("failed to encode report JSON: {err}"))?;
    fs::write(path, payload)
        .map_err(|err| format!("failed to write report {}: {err}", path.display()))
}

fn validate_corpus(corpus: &WorkflowCorpus, manifest_path: &Path) -> WorkflowCorpusReport {
    let mut violations = Vec::new();

    if corpus.schema_version != USER_WORKFLOW_CORPUS_SCHEMA_VERSION {
        violations.push(format!(
            "schema_version expected '{}', got '{}'",
            USER_WORKFLOW_CORPUS_SCHEMA_VERSION, corpus.schema_version
        ));
    }
    if corpus.generated_at_utc.trim().is_empty() {
        violations.push("generated_at_utc must not be empty".to_string());
    }
    let log_manifest = resolve_repo_path(&corpus.log_manifest_path);
    if !log_manifest.exists() {
        violations.push(format!(
            "log_manifest_path does not exist: {}",
            log_manifest.display()
        ));
    }

    let mut journey_ids = HashSet::new();
    let mut packet_ids_seen = BTreeSet::new();
    let mut active_differential_count = 0_usize;
    let mut planned_differential_count = 0_usize;
    let mut packet_coverage = Vec::new();

    for journey in &corpus.journeys {
        validate_non_empty("journey_id", &journey.journey_id, &mut violations);
        validate_non_empty("packet_id", &journey.packet_id, &mut violations);
        validate_non_empty("description", &journey.description, &mut violations);

        if !journey_ids.insert(journey.journey_id.clone()) {
            violations.push(format!("duplicate journey_id '{}'", journey.journey_id));
        }
        packet_ids_seen.insert(journey.packet_id.clone());

        if !PACKET_FAMILIES.contains(&journey.packet_id.as_str()) {
            violations.push(format!(
                "unknown packet_id '{}' in journey '{}'",
                journey.packet_id, journey.journey_id
            ));
        }

        validate_hook("unit_hook", &journey.unit_hook, &mut violations);
        validate_hook("e2e_hook", &journey.e2e_hook, &mut violations);
        validate_differential_hook(&journey.differential_hook, &mut violations);

        if !journey
            .stable_reason_codes
            .iter()
            .any(|code| code == "parity_ok")
        {
            violations.push(format!(
                "journey '{}' stable_reason_codes missing 'parity_ok'",
                journey.journey_id
            ));
        }
        if !journey
            .stable_reason_codes
            .iter()
            .any(|code| code == "journey_ok")
        {
            violations.push(format!(
                "journey '{}' stable_reason_codes missing 'journey_ok'",
                journey.journey_id
            ));
        }

        match journey.differential_hook.status {
            DifferentialStatus::Active => active_differential_count += 1,
            DifferentialStatus::Planned => planned_differential_count += 1,
        }

        let golden_log_path = resolve_repo_path(&journey.golden_log_path);
        match load_golden_log_events(&golden_log_path) {
            Ok(events) => validate_golden_event_refs(journey, &events, &mut violations),
            Err(err) => violations.push(err),
        }

        packet_coverage.push(PacketJourneyCoverage {
            packet_id: journey.packet_id.clone(),
            journey_id: journey.journey_id.clone(),
            differential_status: journey.differential_hook.status,
            differential_owner_bead: journey.differential_hook.owner_bead.clone(),
            differential_fixtures: journey.differential_hook.fixtures.clone(),
        });
    }

    let expected_packet_ids = PACKET_FAMILIES
        .iter()
        .map(|packet| (*packet).to_string())
        .collect::<BTreeSet<_>>();
    let missing_packets = expected_packet_ids
        .difference(&packet_ids_seen)
        .cloned()
        .collect::<Vec<_>>();
    let unexpected_packets = packet_ids_seen
        .difference(&expected_packet_ids)
        .cloned()
        .collect::<Vec<_>>();
    if !missing_packets.is_empty() {
        violations.push(format!(
            "missing packet journeys: {}",
            missing_packets.join(", ")
        ));
    }
    if !unexpected_packets.is_empty() {
        violations.push(format!(
            "unexpected packet journeys: {}",
            unexpected_packets.join(", ")
        ));
    }

    packet_coverage.sort_by(|left, right| left.packet_id.cmp(&right.packet_id));

    WorkflowCorpusReport {
        schema_version: USER_WORKFLOW_CORPUS_REPORT_SCHEMA_VERSION.to_string(),
        corpus_id: corpus.corpus_id.clone(),
        manifest_path: manifest_path.display().to_string(),
        journey_count: corpus.journeys.len(),
        active_differential_count,
        planned_differential_count,
        packet_coverage,
        violations,
    }
}

fn validate_non_empty(field_name: &str, value: &str, violations: &mut Vec<String>) {
    if value.trim().is_empty() {
        violations.push(format!("{field_name} must not be empty"));
    }
}

fn validate_hook(prefix: &str, hook: &WorkflowHook, violations: &mut Vec<String>) {
    validate_non_empty(&format!("{prefix}.suite_id"), &hook.suite_id, violations);
    validate_non_empty(
        &format!("{prefix}.test_or_scenario_id"),
        &hook.test_or_scenario_id,
        violations,
    );
    validate_non_empty(
        &format!("{prefix}.replay_cmd"),
        &hook.replay_cmd,
        violations,
    );
    validate_non_empty(
        &format!("{prefix}.owner_bead"),
        &hook.owner_bead,
        violations,
    );
}

fn validate_differential_hook(hook: &DifferentialHook, violations: &mut Vec<String>) {
    validate_non_empty("differential_hook.hook_mode", &hook.hook_mode, violations);
    validate_non_empty("differential_hook.command", &hook.command, violations);
    validate_non_empty("differential_hook.owner_bead", &hook.owner_bead, violations);
    validate_non_empty("differential_hook.notes", &hook.notes, violations);
    if hook.fixtures.is_empty() {
        violations.push("differential_hook.fixtures must not be empty".to_string());
    }
}

fn load_golden_log_events(path: &Path) -> Result<Vec<StructuredLogEvent>, String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read golden log {}: {err}", path.display()))?;
    let mut events = Vec::new();
    for (line_idx, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: StructuredLogEvent = serde_json::from_str(line).map_err(|err| {
            format!(
                "failed to parse golden log line {} at {}: {err}",
                line_idx + 1,
                path.display()
            )
        })?;
        events.push(event);
    }
    if events.is_empty() {
        return Err(format!("golden log file has no events: {}", path.display()));
    }
    Ok(events)
}

fn validate_golden_event_refs(
    journey: &WorkflowJourney,
    events: &[StructuredLogEvent],
    violations: &mut Vec<String>,
) {
    let unit_event = events
        .iter()
        .find(|event| event.test_or_scenario_id == journey.unit_hook.test_or_scenario_id);
    match unit_event {
        Some(event) => {
            if event.packet_id != journey.packet_id {
                violations.push(format!(
                    "unit hook packet mismatch for journey '{}': expected '{}', got '{}'",
                    journey.journey_id, journey.packet_id, event.packet_id
                ));
            }
            if event.suite_id != journey.unit_hook.suite_id {
                violations.push(format!(
                    "unit hook suite mismatch for journey '{}': expected '{}', got '{}'",
                    journey.journey_id, journey.unit_hook.suite_id, event.suite_id
                ));
            }
        }
        None => violations.push(format!(
            "unit scenario '{}' missing from golden log for journey '{}'",
            journey.unit_hook.test_or_scenario_id, journey.journey_id
        )),
    }

    let e2e_event = events
        .iter()
        .find(|event| event.test_or_scenario_id == journey.e2e_hook.test_or_scenario_id);
    match e2e_event {
        Some(event) => {
            if event.packet_id != journey.packet_id {
                violations.push(format!(
                    "e2e hook packet mismatch for journey '{}': expected '{}', got '{}'",
                    journey.journey_id, journey.packet_id, event.packet_id
                ));
            }
            if event.suite_id != journey.e2e_hook.suite_id {
                violations.push(format!(
                    "e2e hook suite mismatch for journey '{}': expected '{}', got '{}'",
                    journey.journey_id, journey.e2e_hook.suite_id, event.suite_id
                ));
            }
        }
        None => violations.push(format!(
            "e2e scenario '{}' missing from golden log for journey '{}'",
            journey.e2e_hook.test_or_scenario_id, journey.journey_id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{default_manifest_path, parse_args, validate_corpus};
    use crate::load_corpus;

    #[test]
    fn parse_args_accepts_manifest_and_json_out() {
        let args = parse_args(vec![
            "--manifest".to_string(),
            "fixtures/custom.json".to_string(),
            "--json-out".to_string(),
            "artifacts/report.json".to_string(),
        ])
        .expect("arguments parse");
        assert_eq!(args.manifest.to_string_lossy(), "fixtures/custom.json");
        assert_eq!(
            args.json_out.expect("json out").to_string_lossy(),
            "artifacts/report.json"
        );
    }

    #[test]
    fn default_corpus_validates_without_violations() {
        let manifest = default_manifest_path();
        let corpus = load_corpus(&manifest).expect("load corpus");
        let report = validate_corpus(&corpus, &manifest);
        assert!(
            report.violations.is_empty(),
            "unexpected violations: {:?}",
            report.violations
        );
        assert_eq!(report.journey_count, 9);
        assert_eq!(report.active_differential_count, 6);
        assert_eq!(report.planned_differential_count, 3);
    }
}
