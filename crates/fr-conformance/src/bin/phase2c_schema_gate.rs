#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use fr_conformance::phase2c_schema::{
    NOT_READY, PacketReadiness, discover_phase2c_packets, validate_phase2c_packets,
};

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
    let mut emit_decision_ledger = false;
    let mut args = Vec::new();
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--decision-ledger" | "--galaxy-brain" => {
                emit_decision_ledger = true;
            }
            _ => args.push(arg),
        }
    }
    let default_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../artifacts/phase2c");

    let packet_dirs = if args.is_empty() {
        discover_phase2c_packets(&default_root)?
    } else {
        args.into_iter().map(PathBuf::from).collect::<Vec<_>>()
    };

    if packet_dirs.is_empty() {
        println!(
            "status: {NOT_READY}\nreason: no packet directories found (expected FR-P2C-* under {})",
            default_root.display()
        );
        return Ok(ExitCode::from(1));
    }

    let mut has_not_ready = false;

    let reports = validate_phase2c_packets(&packet_dirs)?;
    for report in reports {
        println!("packet: {}", report.packet_id);
        println!(
            "schema_version: {}",
            report.schema_version.as_deref().unwrap_or("<missing>")
        );
        println!("status: {}", report.readiness.as_str());
        if !report.missing_files.is_empty() {
            println!("missing_files: {}", report.missing_files.join(", "));
        }
        if !report.missing_fields.is_empty() {
            println!("missing_fields: {}", report.missing_fields.join(", "));
        }
        if !report.errors.is_empty() {
            println!("errors: {}", report.errors.join(" | "));
        }
        if emit_decision_ledger {
            println!(
                "decision.posterior_contract_violation: {:.6}",
                report.decision_ledger.posterior_contract_violation
            );
            println!(
                "decision.expected_loss.proceed_impl: {:.6}",
                report.decision_ledger.expected_loss_proceed
            );
            println!(
                "decision.expected_loss.block_impl: {:.6}",
                report.decision_ledger.expected_loss_block
            );
            println!(
                "decision.recommended_action: {}",
                report.decision_ledger.recommended_action.as_str()
            );
            if !report.decision_ledger.evidence_terms.is_empty() {
                let summary = report
                    .decision_ledger
                    .evidence_terms
                    .iter()
                    .map(|term| {
                        format!(
                            "{}(count={},log_odds_shift={:.3})",
                            term.signal, term.count, term.log_odds_shift
                        )
                    })
                    .collect::<Vec<_>>();
                println!("decision.evidence_terms: {}", summary.join(", "));
            }
        }
        println!("---");

        if report.readiness == PacketReadiness::NotReady {
            has_not_ready = true;
        }
    }

    if has_not_ready {
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
