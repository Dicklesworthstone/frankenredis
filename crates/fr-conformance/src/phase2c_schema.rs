use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use serde_json::Value;

pub const PHASE2C_SCHEMA_VERSION: &str = "fr_phase2c_packet_v1";
pub const READY_FOR_IMPL: &str = "READY_FOR_IMPL";
pub const NOT_READY: &str = "NOT READY";
pub const READY_FOR_OPTIMIZATION: &str = "READY_FOR_OPTIMIZATION";

pub const REQUIRED_PACKET_FILES: [&str; 8] = [
    "legacy_anchor_map.md",
    "contract_table.md",
    "fixture_manifest.json",
    "parity_gate.yaml",
    "risk_note.md",
    "parity_report.json",
    "parity_report.raptorq.json",
    "parity_report.decode_proof.json",
];

pub const REQUIRED_MANIFEST_FIELDS: [&str; 15] = [
    "packet_id",
    "legacy_paths",
    "legacy_symbols",
    "state_machine_contract",
    "protocol_contract",
    "command_acl_contract",
    "persistence_replication_contract",
    "error_contract",
    "strict_mode_policy",
    "hardened_mode_policy",
    "excluded_scope",
    "oracle_tests",
    "performance_sentinels",
    "compatibility_risks",
    "raptorq_artifacts",
];

pub const REQUIRED_OPTIMIZATION_ROOT_FILES: [&str; 4] = [
    "run_gate_bench.sh",
    "bench_packets",
    "baseline_hyperfine_multi.json",
    "after_hyperfine_multi.json",
];

pub const REQUIRED_OPTIMIZATION_ROUND_FILES: [&str; 14] = [
    "manifest.json",
    "env.json",
    "repro.lock",
    "optimization_report.md",
    "alien_recommendation_card.md",
    "isomorphism_check.txt",
    "baseline_hyperfine.json",
    "after_hyperfine.json",
    "baseline_output.txt",
    "after_output.txt",
    "baseline_output.sha256",
    "after_output.sha256",
    "baseline_strace.txt",
    "after_strace.txt",
];

const FILE_BIT_LEGACY_ANCHOR_MAP: u16 = 1 << 0;
const FILE_BIT_CONTRACT_TABLE: u16 = 1 << 1;
const FILE_BIT_FIXTURE_MANIFEST: u16 = 1 << 2;
const FILE_BIT_PARITY_GATE: u16 = 1 << 3;
const FILE_BIT_RISK_NOTE: u16 = 1 << 4;
const FILE_BIT_PARITY_REPORT: u16 = 1 << 5;
const FILE_BIT_RAPTORQ_SIDECAR: u16 = 1 << 6;
const FILE_BIT_DECODE_PROOF: u16 = 1 << 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketReadiness {
    ReadyForImpl,
    NotReady,
}

impl PacketReadiness {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadyForImpl => READY_FOR_IMPL,
            Self::NotReady => NOT_READY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    ProceedImpl,
    BlockImpl,
}

impl GateAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProceedImpl => "PROCEED_IMPL",
            Self::BlockImpl => "BLOCK_IMPL",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceTerm {
    pub signal: &'static str,
    pub count: usize,
    pub log_odds_shift: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateDecisionLedger {
    pub posterior_contract_violation: f64,
    pub expected_loss_proceed: f64,
    pub expected_loss_block: f64,
    pub recommended_action: GateAction,
    pub evidence_terms: Vec<EvidenceTerm>,
}

impl GateDecisionLedger {
    #[must_use]
    pub fn default_prior() -> Self {
        let posterior_contract_violation = 0.01_f64;
        let expected_loss_proceed = posterior_contract_violation * 100.0_f64;
        let expected_loss_block = (posterior_contract_violation * 1.0_f64)
            + ((1.0_f64 - posterior_contract_violation) * 8.0_f64);
        Self {
            posterior_contract_violation,
            expected_loss_proceed,
            expected_loss_block,
            recommended_action: GateAction::ProceedImpl,
            evidence_terms: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PacketValidationReport {
    pub packet_id: String,
    pub schema_version: Option<String>,
    pub readiness: PacketReadiness,
    pub missing_files: Vec<String>,
    pub missing_fields: Vec<String>,
    pub errors: Vec<String>,
    pub decision_ledger: GateDecisionLedger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationGateStatus {
    Ready,
    NotReady,
}

impl OptimizationGateStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => READY_FOR_OPTIMIZATION,
            Self::NotReady => NOT_READY,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationRoundReport {
    pub round_id: String,
    pub claim_id: Option<String>,
    pub evidence_id: Option<String>,
    pub baseline_mean_seconds: Option<f64>,
    pub after_mean_seconds: Option<f64>,
    pub delta_percent: Option<f64>,
    pub missing_files: Vec<String>,
    pub errors: Vec<String>,
}

impl OptimizationRoundReport {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.missing_files.is_empty() && self.errors.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OptimizationGateReport {
    pub root: PathBuf,
    pub status: OptimizationGateStatus,
    pub baseline_mean_seconds: Option<f64>,
    pub after_mean_seconds: Option<f64>,
    pub missing_files: Vec<String>,
    pub errors: Vec<String>,
    pub rounds: Vec<OptimizationRoundReport>,
}

impl PacketValidationReport {
    #[must_use]
    pub fn is_ready_for_impl(&self) -> bool {
        self.readiness == PacketReadiness::ReadyForImpl
    }
}

pub fn discover_phase2c_packets(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !root.is_dir() {
        return Err(format!(
            "phase2c root is not a directory: {}",
            root.display()
        ));
    }

    let mut packets = fs::read_dir(root)
        .map_err(|err| format!("failed to read phase2c root {}: {err}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?;
            if name.starts_with("FR-P2C-") {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    packets.sort();
    Ok(packets)
}

pub fn validate_phase2c_tree(root: &Path) -> Result<Vec<PacketValidationReport>, String> {
    let packet_dirs = discover_phase2c_packets(root)?;
    validate_phase2c_packets(&packet_dirs)
}

pub fn validate_phase2c_packets(
    packet_dirs: &[PathBuf],
) -> Result<Vec<PacketValidationReport>, String> {
    if packet_dirs.is_empty() {
        return Ok(Vec::new());
    }

    let workers = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(packet_dirs.len());

    if workers <= 1 || packet_dirs.len() < 4 {
        let mut reports = validate_phase2c_packets_serial(packet_dirs)?;
        reports.sort_by(|a, b| a.packet_id.cmp(&b.packet_id));
        return Ok(reports);
    }

    let chunk_size = packet_dirs.len().div_ceil(workers);
    let handles = packet_dirs
        .chunks(chunk_size)
        .map(|chunk| {
            let chunk_dirs = chunk.to_vec();
            thread::spawn(move || validate_phase2c_packets_serial(&chunk_dirs))
        })
        .collect::<Vec<_>>();

    let mut reports = Vec::with_capacity(packet_dirs.len());
    for handle in handles {
        let chunk_reports = handle
            .join()
            .map_err(|_| "phase2c validation worker panicked".to_string())??;
        reports.extend(chunk_reports);
    }
    reports.sort_by(|a, b| a.packet_id.cmp(&b.packet_id));
    Ok(reports)
}

#[derive(Debug, Clone)]
struct HyperfineSummary {
    command: String,
    mean_seconds: f64,
    sample_count: usize,
}

pub fn validate_phase2c_optimization_gate(root: &Path) -> Result<OptimizationGateReport, String> {
    let mut report = OptimizationGateReport {
        root: root.to_path_buf(),
        status: OptimizationGateStatus::NotReady,
        baseline_mean_seconds: None,
        after_mean_seconds: None,
        missing_files: Vec::new(),
        errors: Vec::new(),
        rounds: Vec::new(),
    };

    if root.exists() && !root.is_dir() {
        return Err(format!(
            "optimization gate root is not a directory: {}",
            root.display()
        ));
    }

    for required in REQUIRED_OPTIMIZATION_ROOT_FILES {
        let path = root.join(required);
        if required == "bench_packets" {
            if !path.is_dir() {
                report.missing_files.push(required.to_string());
            }
        } else if !path.is_file() {
            report.missing_files.push(required.to_string());
        }
    }

    let run_gate_bench_path = root.join("run_gate_bench.sh");
    if run_gate_bench_path.is_file() {
        match fs::read_to_string(&run_gate_bench_path) {
            Ok(script) => {
                if !script.contains("phase2c_schema_gate") {
                    report.errors.push(format!(
                        "{} does not invoke phase2c_schema_gate",
                        run_gate_bench_path.display()
                    ));
                }
                if !script.contains("bench_packets") {
                    report.errors.push(format!(
                        "{} does not include bench_packets corpus",
                        run_gate_bench_path.display()
                    ));
                }
            }
            Err(err) => report.errors.push(format!(
                "failed to read {}: {err}",
                run_gate_bench_path.display()
            )),
        }
    }

    if let Some(summary) = parse_hyperfine_summary_if_present(
        &root.join("baseline_hyperfine_multi.json"),
        &mut report.errors,
    ) {
        if !summary.command.contains("run_gate_bench.sh") {
            report.errors.push(format!(
                "baseline_hyperfine_multi.json command should invoke run_gate_bench.sh, got '{}'",
                summary.command
            ));
        }
        if summary.sample_count < 10 {
            report.errors.push(format!(
                "baseline_hyperfine_multi.json expected >=10 samples, got {}",
                summary.sample_count
            ));
        }
        report.baseline_mean_seconds = Some(summary.mean_seconds);
    }

    if let Some(summary) = parse_hyperfine_summary_if_present(
        &root.join("after_hyperfine_multi.json"),
        &mut report.errors,
    ) {
        if !summary.command.contains("run_gate_bench.sh") {
            report.errors.push(format!(
                "after_hyperfine_multi.json command should invoke run_gate_bench.sh, got '{}'",
                summary.command
            ));
        }
        if summary.sample_count < 10 {
            report.errors.push(format!(
                "after_hyperfine_multi.json expected >=10 samples, got {}",
                summary.sample_count
            ));
        }
        report.after_mean_seconds = Some(summary.mean_seconds);
    }

    let bench_packets_root = root.join("bench_packets");
    if bench_packets_root.is_dir() {
        let mut valid_count = 0_usize;
        let mut invalid_count = 0_usize;
        let mut entries = fs::read_dir(&bench_packets_root).map_err(|err| {
            format!(
                "failed to read bench packet corpus {}: {err}",
                bench_packets_root.display()
            )
        })?;
        entries.try_for_each(|entry| -> Result<(), String> {
            let entry = entry.map_err(|err| {
                format!(
                    "failed to read bench packet entry {}: {err}",
                    bench_packets_root.display()
                )
            })?;
            let path = entry.path();
            if !path.is_dir() {
                return Ok(());
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains("VALID") {
                valid_count += 1;
            }
            if name.contains("INVALID") {
                invalid_count += 1;
            }
            Ok(())
        })?;
        if valid_count == 0 || invalid_count == 0 {
            report.errors.push(format!(
                "bench_packets corpus must include both VALID and INVALID suites (valid={valid_count}, invalid={invalid_count})"
            ));
        }
    }

    let round_dirs = discover_canonical_optimization_rounds(root)?;
    if round_dirs.is_empty() {
        report.errors.push(format!(
            "no canonical optimization rounds found under {} (expected round_*/manifest.json)",
            root.display()
        ));
    }

    for round_dir in round_dirs {
        let round_report = validate_optimization_round(&round_dir)?;
        if !round_report.is_ready() {
            report.errors.push(format!(
                "optimization round {} is incomplete",
                round_report.round_id
            ));
        }
        report.rounds.push(round_report);
    }

    if report.missing_files.is_empty() && report.errors.is_empty() {
        report.status = OptimizationGateStatus::Ready;
    }

    Ok(report)
}

fn discover_canonical_optimization_rounds(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut round_dirs = fs::read_dir(root)
        .map_err(|err| format!("failed to read optimization root {}: {err}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = path.file_name()?.to_str()?;
            if !name.starts_with("round_") {
                return None;
            }
            if path.join("manifest.json").is_file() {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    round_dirs.sort();
    Ok(round_dirs)
}

fn validate_optimization_round(round_dir: &Path) -> Result<OptimizationRoundReport, String> {
    if !round_dir.exists() {
        return Err(format!(
            "optimization round directory does not exist: {}",
            round_dir.display()
        ));
    }
    if !round_dir.is_dir() {
        return Err(format!(
            "optimization round path is not a directory: {}",
            round_dir.display()
        ));
    }

    let round_id = round_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid optimization round directory: {}",
                round_dir.display()
            )
        })?
        .to_string();

    let mut report = OptimizationRoundReport {
        round_id,
        claim_id: None,
        evidence_id: None,
        baseline_mean_seconds: None,
        after_mean_seconds: None,
        delta_percent: None,
        missing_files: Vec::new(),
        errors: Vec::new(),
    };

    for required in REQUIRED_OPTIMIZATION_ROUND_FILES {
        let path = round_dir.join(required);
        if !path.is_file() {
            report.missing_files.push(required.to_string());
        }
    }

    let manifest_path = round_dir.join("manifest.json");
    if manifest_path.is_file() {
        match parse_json_file(&manifest_path) {
            Ok(value) => validate_optimization_round_manifest(round_dir, &value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    if let Some(summary) = parse_hyperfine_summary_if_present(
        &round_dir.join("baseline_hyperfine.json"),
        &mut report.errors,
    ) {
        report.baseline_mean_seconds = Some(summary.mean_seconds);
    }

    if let Some(summary) = parse_hyperfine_summary_if_present(
        &round_dir.join("after_hyperfine.json"),
        &mut report.errors,
    ) {
        report.after_mean_seconds = Some(summary.mean_seconds);
    }

    let isomorphism_path = round_dir.join("isomorphism_check.txt");
    if isomorphism_path.is_file() {
        match fs::read_to_string(&isomorphism_path) {
            Ok(raw) => {
                if !raw.contains("isomorphism_output_match=1") {
                    report.errors.push(format!(
                        "{} must include 'isomorphism_output_match=1'",
                        isomorphism_path.display()
                    ));
                }
            }
            Err(err) => report.errors.push(format!(
                "failed to read {}: {err}",
                isomorphism_path.display()
            )),
        }
    }

    let card_path = round_dir.join("alien_recommendation_card.md");
    if card_path.is_file() {
        match fs::read_to_string(&card_path) {
            Ok(raw) => {
                if let Some(claim_id) = report.claim_id.as_deref()
                    && !raw.contains(claim_id)
                {
                    report.errors.push(format!(
                        "{} must reference claim_id '{claim_id}'",
                        card_path.display()
                    ));
                }
                if let Some(evidence_id) = report.evidence_id.as_deref()
                    && !raw.contains(evidence_id)
                {
                    report.errors.push(format!(
                        "{} must reference evidence_id '{evidence_id}'",
                        card_path.display()
                    ));
                }
            }
            Err(err) => report
                .errors
                .push(format!("failed to read {}: {err}", card_path.display())),
        }
    }

    let report_path = round_dir.join("optimization_report.md");
    if report_path.is_file() {
        match fs::read_to_string(&report_path) {
            Ok(raw) => {
                if !raw.contains("Delta:") {
                    report.errors.push(format!(
                        "{} missing performance delta line",
                        report_path.display()
                    ));
                }
                if !raw.contains("Isomorphism:") {
                    report.errors.push(format!(
                        "{} missing isomorphism line",
                        report_path.display()
                    ));
                }
            }
            Err(err) => report
                .errors
                .push(format!("failed to read {}: {err}", report_path.display())),
        }
    }

    Ok(report)
}

fn validate_optimization_round_manifest(
    round_dir: &Path,
    manifest: &Value,
    report: &mut OptimizationRoundReport,
) {
    let Some(object) = manifest.as_object() else {
        report
            .errors
            .push("optimization manifest must be a JSON object".to_string());
        return;
    };

    match object.get("claim_id").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => report.claim_id = Some(value.to_string()),
        _ => report
            .errors
            .push("manifest.claim_id must be a non-empty string".to_string()),
    }
    match object.get("evidence_id").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => report.evidence_id = Some(value.to_string()),
        _ => report
            .errors
            .push("manifest.evidence_id must be a non-empty string".to_string()),
    }
    match object.get("delta_percent").and_then(Value::as_f64) {
        Some(value) if value.is_finite() => report.delta_percent = Some(value),
        _ => report
            .errors
            .push("manifest.delta_percent must be a finite number".to_string()),
    }

    validate_optimization_manifest_section(round_dir, object, report, "baseline");
    validate_optimization_manifest_section(round_dir, object, report, "after");

    let isomorphism_ref = object.get("isomorphism").and_then(Value::as_str);
    match isomorphism_ref {
        Some(value) if !value.trim().is_empty() => {
            if !round_dir.join(value).is_file() {
                report.errors.push(format!(
                    "manifest.isomorphism references missing file '{}'",
                    value
                ));
            }
        }
        _ => report
            .errors
            .push("manifest.isomorphism must be a non-empty string".to_string()),
    }

    match object.get("source_files").and_then(Value::as_array) {
        Some(files) if !files.is_empty() => {}
        _ => report
            .errors
            .push("manifest.source_files must contain at least one entry".to_string()),
    }
}

fn validate_optimization_manifest_section(
    round_dir: &Path,
    manifest: &serde_json::Map<String, Value>,
    report: &mut OptimizationRoundReport,
    section_name: &'static str,
) {
    let Some(section) = manifest.get(section_name).and_then(Value::as_object) else {
        report
            .errors
            .push(format!("manifest.{section_name} must be an object"));
        return;
    };

    match section.get("hyperfine").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => {
            if !round_dir.join(value).is_file() {
                report.errors.push(format!(
                    "manifest.{section_name}.hyperfine references missing file '{}'",
                    value
                ));
            }
        }
        _ => report.errors.push(format!(
            "manifest.{section_name}.hyperfine must be a non-empty string"
        )),
    }
    match section.get("strace").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => {
            if !round_dir.join(value).is_file() {
                report.errors.push(format!(
                    "manifest.{section_name}.strace references missing file '{}'",
                    value
                ));
            }
        }
        _ => report.errors.push(format!(
            "manifest.{section_name}.strace must be a non-empty string"
        )),
    }
    match section.get("stdout_sha256").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => {}
        _ => report.errors.push(format!(
            "manifest.{section_name}.stdout_sha256 must be a non-empty string"
        )),
    }
    match section.get("mean_seconds").and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value > 0.0_f64 => {}
        _ => report.errors.push(format!(
            "manifest.{section_name}.mean_seconds must be a positive number"
        )),
    }
}

fn parse_hyperfine_summary_if_present(
    path: &Path,
    errors: &mut Vec<String>,
) -> Option<HyperfineSummary> {
    if !path.is_file() {
        return None;
    }
    match parse_hyperfine_summary(path) {
        Ok(summary) => Some(summary),
        Err(err) => {
            errors.push(err);
            None
        }
    }
}

fn parse_hyperfine_summary(path: &Path) -> Result<HyperfineSummary, String> {
    let value = parse_json_file(path)?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "invalid hyperfine JSON {}: missing results[]",
                path.display()
            )
        })?;
    let first = results
        .first()
        .ok_or_else(|| format!("invalid hyperfine JSON {}: empty results[]", path.display()))?;
    let command = first
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("invalid hyperfine JSON {}: missing command", path.display()))?
        .to_string();
    let mean_seconds = first
        .get("mean")
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("invalid hyperfine JSON {}: missing mean", path.display()))?;
    if !mean_seconds.is_finite() || mean_seconds <= 0.0_f64 {
        return Err(format!(
            "invalid hyperfine mean in {}: expected positive finite number, got {mean_seconds}",
            path.display()
        ));
    }
    let sample_count = first
        .get("times")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);

    Ok(HyperfineSummary {
        command,
        mean_seconds,
        sample_count,
    })
}

pub fn validate_phase2c_packet(packet_dir: &Path) -> Result<PacketValidationReport, String> {
    if !packet_dir.exists() {
        return Err(format!(
            "packet directory does not exist: {}",
            packet_dir.display()
        ));
    }
    if !packet_dir.is_dir() {
        return Err(format!(
            "packet path is not a directory: {}",
            packet_dir.display()
        ));
    }

    let packet_id = packet_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid packet directory name: {}", packet_dir.display()))?
        .to_string();

    let mut report = PacketValidationReport {
        packet_id: packet_id.clone(),
        schema_version: None,
        readiness: PacketReadiness::NotReady,
        missing_files: Vec::new(),
        missing_fields: Vec::new(),
        errors: Vec::new(),
        decision_ledger: GateDecisionLedger::default_prior(),
    };

    let file_mask = collect_packet_file_mask(packet_dir)?;

    for required in REQUIRED_PACKET_FILES {
        let required_bit =
            file_presence_bit(required).expect("required phase2c file should have a mapped bit");
        if file_mask & required_bit == 0 {
            report.missing_files.push(required.to_string());
        }
    }

    let mut parity_report_readiness = None::<String>;

    let fixture_manifest_path = packet_dir.join("fixture_manifest.json");
    if file_mask & FILE_BIT_FIXTURE_MANIFEST != 0 {
        match parse_json_file(&fixture_manifest_path) {
            Ok(value) => validate_fixture_manifest(&packet_id, &value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    let parity_report_path = packet_dir.join("parity_report.json");
    if file_mask & FILE_BIT_PARITY_REPORT != 0 {
        match parse_json_file(&parity_report_path) {
            Ok(value) => {
                parity_report_readiness = validate_parity_report(&packet_id, &value, &mut report);
            }
            Err(err) => report.errors.push(err),
        }
    }

    let raptorq_path = packet_dir.join("parity_report.raptorq.json");
    if file_mask & FILE_BIT_RAPTORQ_SIDECAR != 0 {
        match parse_json_file(&raptorq_path) {
            Ok(value) => validate_raptorq_sidecar(&value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    let decode_proof_path = packet_dir.join("parity_report.decode_proof.json");
    if file_mask & FILE_BIT_DECODE_PROOF != 0 {
        match parse_json_file(&decode_proof_path) {
            Ok(value) => validate_decode_proof(&packet_id, &value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    if report.missing_files.is_empty()
        && report.missing_fields.is_empty()
        && report.errors.is_empty()
    {
        report.readiness = PacketReadiness::ReadyForImpl;
    }

    match (report.readiness, parity_report_readiness.as_deref()) {
        (PacketReadiness::ReadyForImpl, Some(READY_FOR_IMPL)) => {}
        (PacketReadiness::ReadyForImpl, Some(other)) => {
            report.errors.push(format!(
                "parity_report.json.readiness expected '{READY_FOR_IMPL}', got '{other}'"
            ));
            report.readiness = PacketReadiness::NotReady;
        }
        (PacketReadiness::ReadyForImpl, None) => {
            report
                .errors
                .push("parity_report.json.readiness missing".to_string());
            report.readiness = PacketReadiness::NotReady;
        }
        (PacketReadiness::NotReady, Some(NOT_READY)) => {}
        (PacketReadiness::NotReady, Some(other)) => {
            report.errors.push(format!(
                "parity_report.json.readiness expected '{NOT_READY}' when mandatory contract data is missing; got '{other}'"
            ));
        }
        (PacketReadiness::NotReady, None) => {}
    }

    report.decision_ledger = build_gate_decision_ledger(&report);

    Ok(report)
}

fn validate_phase2c_packets_serial(
    packet_dirs: &[PathBuf],
) -> Result<Vec<PacketValidationReport>, String> {
    packet_dirs
        .iter()
        .map(|dir| validate_phase2c_packet(dir))
        .collect()
}

fn build_gate_decision_ledger(report: &PacketValidationReport) -> GateDecisionLedger {
    // Prior assumes contract violations are rare if a packet is maintained, but expensive.
    let mut log_odds = (0.01_f64 / 0.99_f64).ln();
    let mut evidence_terms = Vec::new();

    if !report.missing_files.is_empty() {
        let shift = 1.25_f64 * report.missing_files.len() as f64;
        log_odds += shift;
        evidence_terms.push(EvidenceTerm {
            signal: "missing_files",
            count: report.missing_files.len(),
            log_odds_shift: shift,
        });
    }
    if !report.missing_fields.is_empty() {
        let shift = 1.05_f64 * report.missing_fields.len() as f64;
        log_odds += shift;
        evidence_terms.push(EvidenceTerm {
            signal: "missing_fields",
            count: report.missing_fields.len(),
            log_odds_shift: shift,
        });
    }
    if !report.errors.is_empty() {
        let shift = 1.55_f64 * report.errors.len() as f64;
        log_odds += shift;
        evidence_terms.push(EvidenceTerm {
            signal: "validation_errors",
            count: report.errors.len(),
            log_odds_shift: shift,
        });
    }

    let posterior_contract_violation = 1.0_f64 / (1.0_f64 + (-log_odds).exp());
    let expected_loss_proceed = posterior_contract_violation * 100.0_f64;
    let expected_loss_block = (posterior_contract_violation * 1.0_f64)
        + ((1.0_f64 - posterior_contract_violation) * 8.0_f64);
    let recommended_action = if report.readiness == PacketReadiness::NotReady {
        // Fail-closed gate: strict contract violations always block implementation entry.
        GateAction::BlockImpl
    } else if expected_loss_proceed <= expected_loss_block {
        GateAction::ProceedImpl
    } else {
        GateAction::BlockImpl
    };

    GateDecisionLedger {
        posterior_contract_violation,
        expected_loss_proceed,
        expected_loss_block,
        recommended_action,
        evidence_terms,
    }
}

fn validate_fixture_manifest(
    packet_id: &str,
    manifest: &Value,
    report: &mut PacketValidationReport,
) {
    let Some(object) = manifest.as_object() else {
        report
            .errors
            .push("fixture_manifest.json must be a JSON object".to_string());
        return;
    };

    let schema_version = object.get("schema_version").and_then(Value::as_str);
    match schema_version {
        Some(version) => {
            report.schema_version = Some(version.to_string());
            if version != PHASE2C_SCHEMA_VERSION {
                report.errors.push(format!(
                    "fixture_manifest.json.schema_version expected '{PHASE2C_SCHEMA_VERSION}', got '{version}'"
                ));
            }
        }
        None => report
            .missing_fields
            .push("fixture_manifest.schema_version".to_string()),
    }

    for field in REQUIRED_MANIFEST_FIELDS {
        match object.get(field) {
            Some(value) if is_present(value) => {}
            _ => report
                .missing_fields
                .push(format!("fixture_manifest.{field}")),
        }
    }

    if let Some(value) = object.get("packet_id").and_then(Value::as_str)
        && value != packet_id
    {
        report.errors.push(format!(
            "fixture_manifest.packet_id expected '{packet_id}', got '{value}'"
        ));
    }
}

fn validate_parity_report(
    packet_id: &str,
    parity_report: &Value,
    report: &mut PacketValidationReport,
) -> Option<String> {
    let Some(object) = parity_report.as_object() else {
        report
            .errors
            .push("parity_report.json must be a JSON object".to_string());
        return None;
    };

    match object.get("schema_version").and_then(Value::as_str) {
        Some(version) if version == PHASE2C_SCHEMA_VERSION => {}
        Some(version) => report.errors.push(format!(
            "parity_report.json.schema_version expected '{PHASE2C_SCHEMA_VERSION}', got '{version}'"
        )),
        None => report
            .missing_fields
            .push("parity_report.schema_version".to_string()),
    }

    match object.get("packet_id").and_then(Value::as_str) {
        Some(value) if value == packet_id => {}
        Some(value) => report.errors.push(format!(
            "parity_report.json.packet_id expected '{packet_id}', got '{value}'"
        )),
        None => report
            .missing_fields
            .push("parity_report.packet_id".to_string()),
    }

    match object.get("missing_mandatory_fields") {
        Some(Value::Array(_)) => {}
        Some(_) => report
            .errors
            .push("parity_report.json.missing_mandatory_fields must be an array".to_string()),
        None => report
            .missing_fields
            .push("parity_report.missing_mandatory_fields".to_string()),
    }

    match object.get("readiness").and_then(Value::as_str) {
        Some(value) => Some(value.to_string()),
        None => {
            report
                .missing_fields
                .push("parity_report.readiness".to_string());
            None
        }
    }
}

fn validate_raptorq_sidecar(sidecar: &Value, report: &mut PacketValidationReport) {
    let Some(object) = sidecar.as_object() else {
        report
            .errors
            .push("parity_report.raptorq.json must be a JSON object".to_string());
        return;
    };

    for key in [
        "artifact_id",
        "artifact_type",
        "source_hash",
        "raptorq",
        "scrub",
    ] {
        match object.get(key) {
            Some(value) if is_present(value) => {}
            _ => report
                .missing_fields
                .push(format!("parity_report.raptorq.{key}")),
        }
    }

    match object.get("decode_proofs") {
        Some(Value::Array(_)) => {}
        Some(_) => report
            .errors
            .push("parity_report.raptorq.json.decode_proofs must be an array".to_string()),
        None => report
            .missing_fields
            .push("parity_report.raptorq.decode_proofs".to_string()),
    }

    if let Some(raptorq) = object.get("raptorq") {
        let Some(raptorq_obj) = raptorq.as_object() else {
            report
                .errors
                .push("parity_report.raptorq.json.raptorq must be a JSON object".to_string());
            return;
        };
        for key in ["k", "repair_symbols", "overhead_ratio", "symbol_hashes"] {
            match raptorq_obj.get(key) {
                Some(value) if is_present(value) => {}
                _ => report
                    .missing_fields
                    .push(format!("parity_report.raptorq.raptorq.{key}")),
            }
        }
    }

    if let Some(scrub) = object.get("scrub") {
        let Some(scrub_obj) = scrub.as_object() else {
            report
                .errors
                .push("parity_report.raptorq.json.scrub must be a JSON object".to_string());
            return;
        };
        for key in ["last_ok_unix_ms", "status"] {
            match scrub_obj.get(key) {
                Some(value) if is_present(value) => {}
                _ => report
                    .missing_fields
                    .push(format!("parity_report.raptorq.scrub.{key}")),
            }
        }
    }
}

fn validate_decode_proof(
    packet_id: &str,
    decode_proof: &Value,
    report: &mut PacketValidationReport,
) {
    let Some(object) = decode_proof.as_object() else {
        report
            .errors
            .push("parity_report.decode_proof.json must be a JSON object".to_string());
        return;
    };

    match object.get("schema_version").and_then(Value::as_str) {
        Some(version) if version == PHASE2C_SCHEMA_VERSION => {}
        Some(version) => report.errors.push(format!(
            "parity_report.decode_proof.json.schema_version expected '{PHASE2C_SCHEMA_VERSION}', got '{version}'"
        )),
        None => report
            .missing_fields
            .push("parity_report.decode_proof.schema_version".to_string()),
    }

    match object.get("packet_id").and_then(Value::as_str) {
        Some(value) if value == packet_id => {}
        Some(value) => report.errors.push(format!(
            "parity_report.decode_proof.json.packet_id expected '{packet_id}', got '{value}'"
        )),
        None => report
            .missing_fields
            .push("parity_report.decode_proof.packet_id".to_string()),
    }

    match object.get("decode_proofs") {
        Some(Value::Array(_)) => {}
        Some(_) => report
            .errors
            .push("parity_report.decode_proof.json.decode_proofs must be an array".to_string()),
        None => report
            .missing_fields
            .push("parity_report.decode_proof.decode_proofs".to_string()),
    }
}

fn parse_json_file(path: &Path) -> Result<Value, String> {
    let raw = fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_slice::<Value>(&raw)
        .map_err(|err| format!("invalid JSON {}: {err}", path.display()))
}

fn file_presence_bit(file_name: &str) -> Option<u16> {
    match file_name {
        "legacy_anchor_map.md" => Some(FILE_BIT_LEGACY_ANCHOR_MAP),
        "contract_table.md" => Some(FILE_BIT_CONTRACT_TABLE),
        "fixture_manifest.json" => Some(FILE_BIT_FIXTURE_MANIFEST),
        "parity_gate.yaml" => Some(FILE_BIT_PARITY_GATE),
        "risk_note.md" => Some(FILE_BIT_RISK_NOTE),
        "parity_report.json" => Some(FILE_BIT_PARITY_REPORT),
        "parity_report.raptorq.json" => Some(FILE_BIT_RAPTORQ_SIDECAR),
        "parity_report.decode_proof.json" => Some(FILE_BIT_DECODE_PROOF),
        _ => None,
    }
}

fn collect_packet_file_mask(packet_dir: &Path) -> Result<u16, String> {
    let entries = fs::read_dir(packet_dir).map_err(|err| {
        format!(
            "failed to scan packet directory {}: {err}",
            packet_dir.display()
        )
    })?;
    let mut file_mask = 0_u16;

    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read packet entry {}: {err}",
                packet_dir.display()
            )
        })?;
        let file_type = entry.file_type().map_err(|err| {
            format!(
                "failed to inspect packet entry type in {}: {err}",
                packet_dir.display()
            )
        })?;
        let is_file = if file_type.is_file() {
            true
        } else if file_type.is_symlink() {
            // Preserve Path::is_file semantics for symlinked artifacts.
            entry.path().is_file()
        } else {
            false
        };
        if !is_file {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Some(bit) = file_presence_bit(file_name) {
            file_mask |= bit;
        }
    }

    Ok(file_mask)
}

fn is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        GateAction, NOT_READY, OptimizationGateStatus, PacketReadiness, READY_FOR_IMPL,
        discover_phase2c_packets, validate_phase2c_optimization_gate, validate_phase2c_packet,
        validate_phase2c_packets, validate_phase2c_tree,
    };

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/phase2c")
    }

    fn optimization_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../artifacts/optimization/phase2c-gate")
    }

    fn test_packet_dirs() -> Vec<PathBuf> {
        let mut packets = discover_phase2c_packets(&fixture_root()).expect("discover packets");
        packets.retain(|path| {
            path.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|name| name.starts_with("FR-P2C-TEST-"))
        });
        packets
    }

    #[test]
    fn discovers_phase2c_packet_dirs() {
        let packets = test_packet_dirs();
        assert_eq!(packets.len(), 2);
        assert_eq!(
            packets[0].file_name().and_then(|v| v.to_str()),
            Some("FR-P2C-TEST-INVALID")
        );
        assert_eq!(
            packets[1].file_name().and_then(|v| v.to_str()),
            Some("FR-P2C-TEST-VALID")
        );
    }

    #[test]
    fn valid_packet_is_ready_for_impl() {
        let packet = fixture_root().join("FR-P2C-TEST-VALID");
        let report = validate_phase2c_packet(&packet).expect("validate valid packet");
        assert!(report.is_ready_for_impl());
        assert_eq!(report.readiness, PacketReadiness::ReadyForImpl);
        assert_eq!(report.readiness.as_str(), READY_FOR_IMPL);
        assert!(report.missing_files.is_empty());
        assert!(report.missing_fields.is_empty());
        assert!(report.errors.is_empty());
        assert_eq!(
            report.decision_ledger.recommended_action,
            GateAction::ProceedImpl
        );
        assert!(report.decision_ledger.posterior_contract_violation < 0.1_f64);
    }

    #[test]
    fn missing_mandatory_field_marks_packet_not_ready() {
        let packet = fixture_root().join("FR-P2C-TEST-INVALID");
        let report = validate_phase2c_packet(&packet).expect("validate invalid packet");
        assert_eq!(report.readiness, PacketReadiness::NotReady);
        assert_eq!(report.readiness.as_str(), NOT_READY);
        assert!(
            report
                .missing_fields
                .contains(&"fixture_manifest.command_acl_contract".to_string()),
            "expected mandatory field failure, missing_fields={:?}",
            report.missing_fields
        );
        assert_eq!(
            report.decision_ledger.recommended_action,
            GateAction::BlockImpl
        );
        assert!(report.decision_ledger.posterior_contract_violation > 0.01_f64);
    }

    #[test]
    fn tree_validation_includes_all_packets() {
        let reports = validate_phase2c_tree(&fixture_root()).expect("validate tree");
        assert!(reports.len() >= 2);
        let test_reports = reports
            .iter()
            .filter(|report| report.packet_id.starts_with("FR-P2C-TEST-"))
            .collect::<Vec<_>>();
        assert_eq!(test_reports.len(), 2);
        assert!(
            test_reports
                .iter()
                .any(|r| r.readiness == PacketReadiness::ReadyForImpl)
        );
        assert!(
            test_reports
                .iter()
                .any(|r| r.readiness == PacketReadiness::NotReady)
        );
    }

    #[test]
    fn packet_validation_parallel_path_is_deterministic() {
        let mut packet_dirs = test_packet_dirs();
        packet_dirs.reverse();
        let reports = validate_phase2c_packets(&packet_dirs).expect("parallel packet validation");
        let packet_ids = reports
            .iter()
            .map(|report| report.packet_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(packet_ids, vec!["FR-P2C-TEST-INVALID", "FR-P2C-TEST-VALID"]);
    }

    #[test]
    fn all_required_packet_files_have_presence_bits() {
        for file_name in super::REQUIRED_PACKET_FILES {
            assert!(
                super::file_presence_bit(file_name).is_some(),
                "missing bit mapping for required file {file_name}"
            );
        }
    }

    #[test]
    fn optimization_gate_validates_canonical_round_artifacts() {
        let report = validate_phase2c_optimization_gate(&optimization_root())
            .expect("validate optimization gate");
        assert_eq!(report.status, OptimizationGateStatus::Ready);
        assert!(
            report.baseline_mean_seconds.is_some(),
            "expected baseline mean from hyperfine summary"
        );
        assert!(
            report.after_mean_seconds.is_some(),
            "expected after mean from hyperfine summary"
        );
        assert!(
            report
                .rounds
                .iter()
                .any(|round| round.round_id == "round_dir_scan_mask"),
            "expected canonical round_dir_scan_mask to be validated"
        );
        assert!(
            report.rounds.iter().all(|round| round.is_ready()),
            "all canonical rounds should be complete: {:#?}",
            report.rounds
        );
    }

    #[test]
    fn optimization_gate_missing_root_is_not_ready() {
        let missing_root = fixture_root().join("missing_optimization_gate_root");
        let report =
            validate_phase2c_optimization_gate(&missing_root).expect("validate missing root");
        assert_eq!(report.status, OptimizationGateStatus::NotReady);
        assert!(
            report
                .missing_files
                .iter()
                .any(|file| file == "run_gate_bench.sh"),
            "missing_files should include run_gate_bench.sh: {:?}",
            report.missing_files
        );
    }
}
