//! # fr-fec CLI — RaptorQ-Everywhere durability sidecar utility (spec §9/§19)
//!
//! Commands:
//! - `fr-fec sidecar [--type <type>] [--repair-symbols <N>] [--symbol-size <S>] <paths...>`
//! - `fr-fec scrub <paths...>`
//! - `fr-fec verify-gate <paths...>`
//! - `fr-fec recover --output <new-directory> <artifact>`
#![forbid(unsafe_code)]

use fr_fec::{
    ScrubOutcome, recover_sidecar_to_directory, scrub_sidecar, verify_sidecar_gate, write_sidecar,
};
use std::path::Path;
use std::time::SystemTime;

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_sidecar_file(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.ends_with(".envelope.json") || s.ends_with(".symbols")
}

fn print_usage() {
    eprintln!(
        "Usage:
  fr-fec sidecar [--type <benchmark|conformance|manifest|ledger|state>] [--repair-symbols <N>] [--symbol-size <S>] <paths...>
  fr-fec scrub <paths...>
  fr-fec verify-gate <paths...>
  fr-fec recover --output <new-directory> <artifact>
"
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let cmd = &args[0];
    let now = current_unix_ms();

    match cmd.as_str() {
        "sidecar" => {
            let mut artifact_type = "benchmark".to_string();
            let mut repair_symbols = 8_usize;
            let mut symbol_size = 512_u16;
            let mut paths = Vec::new();

            let mut idx = 1;
            while idx < args.len() {
                match args[idx].as_str() {
                    "--type" => {
                        idx += 1;
                        if idx < args.len() {
                            artifact_type = args[idx].clone();
                        }
                    }
                    "--repair-symbols" => {
                        idx += 1;
                        if idx < args.len() {
                            repair_symbols = args[idx].parse().unwrap_or(8);
                        }
                    }
                    "--symbol-size" => {
                        idx += 1;
                        if idx < args.len() {
                            symbol_size = args[idx].parse().unwrap_or(512);
                        }
                    }
                    path => {
                        paths.push(path.to_string());
                    }
                }
                idx += 1;
            }

            if paths.is_empty() {
                eprintln!("error: no paths specified for sidecar generation");
                std::process::exit(1);
            }

            for p in &paths {
                let path = Path::new(p);
                if is_sidecar_file(path) {
                    continue;
                }
                match write_sidecar(path, &artifact_type, repair_symbols, symbol_size, now) {
                    Ok((env, env_path, sym_path)) => {
                        println!(
                            "sidecar OK: {} (type={}, k={}, repair={}, envelope={}, symbols={})",
                            path.display(),
                            env.artifact_type,
                            env.raptorq.k,
                            env.raptorq.repair_symbols,
                            env_path.display(),
                            sym_path.display(),
                        );
                    }
                    Err(err) => {
                        eprintln!("error generating sidecar for {}: {err}", path.display());
                        std::process::exit(1);
                    }
                }
            }
        }
        "scrub" => {
            if args.len() < 2 {
                eprintln!("error: no paths specified for scrub");
                std::process::exit(1);
            }
            let mut any_failed = false;
            for p in &args[1..] {
                let path = Path::new(p);
                if is_sidecar_file(path) {
                    continue;
                }
                match scrub_sidecar(path, now) {
                    Ok(ScrubOutcome::Clean) => {
                        println!("scrub CLEAN: {}", path.display());
                    }
                    Ok(ScrubOutcome::Recovered { proof, .. }) => {
                        println!("scrub RECOVERED: {} ({})", path.display(), proof.reason);
                    }
                    Ok(ScrubOutcome::Failed { reason }) => {
                        eprintln!("scrub FAILED: {} ({reason})", path.display());
                        any_failed = true;
                    }
                    Err(err) => {
                        eprintln!("scrub ERROR for {}: {err}", path.display());
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        "verify-gate" => {
            if args.len() < 2 {
                eprintln!("error: no paths specified for verify-gate");
                std::process::exit(1);
            }
            let mut any_failed = false;
            for p in &args[1..] {
                let path = Path::new(p);
                if is_sidecar_file(path) {
                    continue;
                }
                match verify_sidecar_gate(path, now) {
                    Ok(()) => {
                        println!("gate OK: {}", path.display());
                    }
                    Err(err) => {
                        eprintln!("gate FAILED: {err}");
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        "recover" => {
            let [_, option, output, artifact] = args.as_slice() else {
                eprintln!("error: recover requires --output <new-directory> <artifact>");
                std::process::exit(1);
            };
            if option != "--output" {
                eprintln!("error: recover requires --output <new-directory> <artifact>");
                std::process::exit(1);
            }
            let path = Path::new(artifact);
            let output = Path::new(output);
            let proof = recover_sidecar_to_directory(path, output, now)?;
            println!(
                "recovery exported: {} -> {} (proof={}, sha256={})",
                path.display(),
                output.join("artifact").display(),
                output.join("proof.json").display(),
                proof.proof_hash
            );
            println!(
                "Review the recovered generation before installing it; the primary is unchanged."
            );
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}
