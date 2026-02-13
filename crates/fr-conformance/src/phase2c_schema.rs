use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use serde_json::Value;

pub const PHASE2C_SCHEMA_VERSION: &str = "fr_phase2c_packet_v1";
pub const READY_FOR_IMPL: &str = "READY_FOR_IMPL";
pub const NOT_READY: &str = "NOT READY";

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

    for required in REQUIRED_PACKET_FILES {
        let path = packet_dir.join(required);
        if !path.is_file() {
            report.missing_files.push(required.to_string());
        }
    }

    let mut parity_report_readiness = None::<String>;

    let fixture_manifest_path = packet_dir.join("fixture_manifest.json");
    if fixture_manifest_path.is_file() {
        match parse_json_file(&fixture_manifest_path) {
            Ok(value) => validate_fixture_manifest(&packet_id, &value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    let parity_report_path = packet_dir.join("parity_report.json");
    if parity_report_path.is_file() {
        match parse_json_file(&parity_report_path) {
            Ok(value) => {
                parity_report_readiness = validate_parity_report(&packet_id, &value, &mut report);
            }
            Err(err) => report.errors.push(err),
        }
    }

    let raptorq_path = packet_dir.join("parity_report.raptorq.json");
    if raptorq_path.is_file() {
        match parse_json_file(&raptorq_path) {
            Ok(value) => validate_raptorq_sidecar(&value, &mut report),
            Err(err) => report.errors.push(err),
        }
    }

    let decode_proof_path = packet_dir.join("parity_report.decode_proof.json");
    if decode_proof_path.is_file() {
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
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str::<Value>(&raw)
        .map_err(|err| format!("invalid JSON {}: {err}", path.display()))
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
        GateAction, NOT_READY, PacketReadiness, READY_FOR_IMPL, discover_phase2c_packets,
        validate_phase2c_packet, validate_phase2c_packets, validate_phase2c_tree,
    };

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/phase2c")
    }

    #[test]
    fn discovers_phase2c_packet_dirs() {
        let root = fixture_root();
        let packets = discover_phase2c_packets(&root).expect("discover packets");
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
        assert_eq!(reports.len(), 2);
        assert!(
            reports
                .iter()
                .any(|r| r.readiness == PacketReadiness::ReadyForImpl)
        );
        assert!(
            reports
                .iter()
                .any(|r| r.readiness == PacketReadiness::NotReady)
        );
    }

    #[test]
    fn packet_validation_parallel_path_is_deterministic() {
        let mut packet_dirs = discover_phase2c_packets(&fixture_root()).expect("discover packets");
        packet_dirs.reverse();
        let reports = validate_phase2c_packets(&packet_dirs).expect("parallel packet validation");
        let packet_ids = reports
            .iter()
            .map(|report| report.packet_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(packet_ids, vec!["FR-P2C-TEST-INVALID", "FR-P2C-TEST-VALID"]);
    }
}
