//! # fr-fec CLI — RaptorQ-Everywhere durability sidecar utility (spec §9/§19)
//!
//! Commands:
//! - `fr-fec sidecar [--type <type>] [--repair-symbols <N>] [--symbol-size <S>] [--chunk-size <bytes>] [--generation-bound] <paths...>`
//! - `fr-fec scrub <paths...>`
//! - `fr-fec verify-gate [--generation-bound] <paths...>`
//! - `fr-fec recover --output <new-directory> <artifact>`
//! - `fr-fec recover --in-place <artifact>`
//! - `fr-fec retire-superseded <paths...>`
#![forbid(unsafe_code)]

use fr_fec::{
    ChunkedEncodeOptions, ScrubOutcome, load_existing_generation_auth_key,
    load_or_create_generation_auth_key, recover_sidecar_in_place, recover_sidecar_to_directory,
    retire_superseded_sidecar, scrub_sidecar, verify_generation_bound_sidecar_gate,
    verify_sidecar_gate, write_generation_bound_sidecar, write_sidecar, write_sidecar_chunked,
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
    s.ends_with(".envelope.json")
        || s.ends_with(".symbols")
        || s.ends_with(".generation.json")
        || s.contains(".recovery-proof.")
}

fn print_usage() {
    eprintln!(
        "Usage:
  fr-fec sidecar [--type <benchmark|conformance|manifest|ledger|state>] [--repair-symbols <N>] [--symbol-size <S>] [--chunk-size <bytes>] [--generation-bound] <paths...>
  fr-fec scrub <paths...>
  fr-fec verify-gate [--generation-bound] <paths...>
  fr-fec recover --output <new-directory> <artifact>
  fr-fec recover --in-place <artifact>
  fr-fec retire-superseded <paths...>
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
            let mut chunk_size: Option<u64> = None;
            let mut generation_bound = false;
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
                    "--chunk-size" => {
                        idx += 1;
                        if idx < args.len() {
                            chunk_size = Some(args[idx].parse().map_err(|_| {
                                format!("invalid --chunk-size value '{}'", args[idx])
                            })?);
                        }
                    }
                    "--generation-bound" => {
                        generation_bound = true;
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
                if generation_bound {
                    let parent = path
                        .parent()
                        .filter(|d| !d.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."));
                    let key = load_or_create_generation_auth_key(parent)?;
                    match write_generation_bound_sidecar(
                        path,
                        &artifact_type,
                        repair_symbols,
                        symbol_size,
                        chunk_size,
                        &key,
                        now,
                    ) {
                        Ok((record, env, gen_path, env_path, sym_path)) => {
                            println!(
                                "sidecar OK: {} (gen={}, seq={}, schema={}, k={}, repair={}, generation={}, envelope={}, symbols={})",
                                path.display(),
                                record.generation_id,
                                record.generation_seq,
                                env.schema_version,
                                env.raptorq.k,
                                env.raptorq.repair_symbols,
                                gen_path.display(),
                                env_path.display(),
                                sym_path.display(),
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "error generating generation-bound sidecar for {}: {err}",
                                path.display()
                            );
                            std::process::exit(1);
                        }
                    }
                } else if let Some(cs) = chunk_size {
                    match write_sidecar_chunked(
                        path,
                        &artifact_type,
                        ChunkedEncodeOptions {
                            chunk_size: cs,
                            repair_symbols,
                            symbol_size,
                        },
                        now,
                    ) {
                        Ok((env, env_path, sym_path)) => {
                            println!(
                                "sidecar OK: {} (schema={}, k={}, repair={}, envelope={}, symbols={})",
                                path.display(),
                                env.schema_version,
                                env.raptorq.k,
                                env.raptorq.repair_symbols,
                                env_path.display(),
                                sym_path.display(),
                            );
                        }
                        Err(err) => {
                            eprintln!(
                                "error generating chunked sidecar for {}: {err}",
                                path.display()
                            );
                            std::process::exit(1);
                        }
                    }
                } else {
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
            let mut generation_bound = false;
            let mut paths = Vec::new();
            for arg in &args[1..] {
                if arg == "--generation-bound" {
                    generation_bound = true;
                } else {
                    paths.push(arg.as_str());
                }
            }
            if paths.is_empty() {
                eprintln!("error: no paths specified for verify-gate");
                std::process::exit(1);
            }
            let mut any_failed = false;
            for p in paths {
                let path = Path::new(p);
                if is_sidecar_file(path) {
                    continue;
                }
                let result = if generation_bound {
                    verify_generation_bound_sidecar_gate(path, None, now).map(|_| ())
                } else {
                    verify_sidecar_gate(path, now)
                };
                match result {
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
        "recover" => match args.as_slice() {
            [_, option, output, artifact] if option == "--output" => {
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
            [_, option, artifact] if option == "--in-place" => {
                let path = Path::new(artifact);
                let proof = recover_sidecar_in_place(path, None, now)?;
                println!(
                    "in-place recovery OK: {} (generation_id={}, seq={}, backup={}, proof={}, sha256={})",
                    path.display(),
                    proof.generation_id,
                    proof.generation_seq,
                    proof.preserved_corrupt_primary_path,
                    proof.proof_path,
                    proof.recovered_primary_sha256,
                );
            }
            _ => {
                eprintln!(
                    "error: recover requires --output <new-directory> <artifact> or --in-place <artifact>"
                );
                std::process::exit(1);
            }
        },
        "retire-superseded" => {
            if args.len() < 2 {
                eprintln!("error: no paths specified for retire-superseded");
                std::process::exit(1);
            }
            let mut any_failed = false;
            for p in &args[1..] {
                let path = Path::new(p);
                if is_sidecar_file(path) {
                    continue;
                }
                let parent = path
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let result = load_existing_generation_auth_key(parent)
                    .and_then(|key| retire_superseded_sidecar(path, &key));
                match result {
                    Ok(retired) => {
                        println!(
                            "retired superseded sidecars for {}: envelope={:?}, symbols={:?}",
                            path.display(),
                            retired.retired_envelope,
                            retired.retired_symbols
                        );
                    }
                    Err(err) => {
                        eprintln!("retire-superseded FAILED for {}: {err}", path.display());
                        any_failed = true;
                    }
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}
