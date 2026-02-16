#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use fr_conformance::phase2c_schema::{
    NOT_READY, OptimizationGateStatus, PacketReadiness, discover_phase2c_packets,
    validate_phase2c_optimization_gate, validate_phase2c_packets,
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
    let mut optimization_gate = false;
    let mut args = Vec::new();
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--decision-ledger" | "--galaxy-brain" => {
                emit_decision_ledger = true;
            }
            "--optimization-gate" | "--perf-gate" => {
                optimization_gate = true;
            }
            _ => args.push(arg),
        }
    }

    if optimization_gate {
        return run_optimization_gate(args);
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

fn run_optimization_gate(args: Vec<String>) -> Result<ExitCode, String> {
    let default_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../artifacts/optimization/phase2c-gate");
    let gate_root = match args.as_slice() {
        [] => default_root,
        [root] => PathBuf::from(root),
        _ => {
            return Err(
                "optimization gate accepts at most one optional path argument: <optimization_root>"
                    .to_string(),
            );
        }
    };

    let report = validate_phase2c_optimization_gate(&gate_root)?;
    println!("optimization_root: {}", report.root.display());
    println!("status: {}", report.status.as_str());
    if let Some(mean) = report.baseline_mean_seconds {
        println!("baseline_mean_seconds: {mean:.9}");
    }
    if let Some(mean) = report.after_mean_seconds {
        println!("after_mean_seconds: {mean:.9}");
    }
    if !report.missing_files.is_empty() {
        println!("missing_files: {}", report.missing_files.join(", "));
    }
    if !report.errors.is_empty() {
        println!("errors: {}", report.errors.join(" | "));
    }
    for round in &report.rounds {
        println!("round: {}", round.round_id);
        println!(
            "round_status: {}",
            if round.is_ready() {
                OptimizationGateStatus::Ready.as_str()
            } else {
                OptimizationGateStatus::NotReady.as_str()
            }
        );
        if let Some(claim_id) = round.claim_id.as_deref() {
            println!("round_claim_id: {claim_id}");
        }
        if let Some(evidence_id) = round.evidence_id.as_deref() {
            println!("round_evidence_id: {evidence_id}");
        }
        if let Some(delta_percent) = round.delta_percent {
            println!("round_delta_percent: {delta_percent:.3}");
        }
        if !round.missing_files.is_empty() {
            println!("round_missing_files: {}", round.missing_files.join(", "));
        }
        if !round.errors.is_empty() {
            println!("round_errors: {}", round.errors.join(" | "));
        }
        println!("---");
    }

    if report.status == OptimizationGateStatus::Ready {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
