//! FrankenRedis standalone server binary.
//!
//! This is the dedicated entry point for running FrankenRedis as a process.
//! It constructs the top-level server objects from the library crates and
//! hands off to a placeholder networking path.
//!
//! Current scope: minimal bootstrap that validates the wiring is real.
//! Full networking (TCP accept loop, per-client sessions, event loop
//! integration) is deferred to subsequent beads.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use fr_config::RuntimePolicy;
use fr_protocol::RespFrame;
use fr_runtime::Runtime;

/// Default port matching Redis convention.
const DEFAULT_PORT: u16 = 6379;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Minimal CLI: --port <N>, --mode strict|hardened, --help
    let mut port = DEFAULT_PORT;
    let mut mode_str = "hardened";
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --port requires a value");
                    return ExitCode::from(1);
                }
                port = match args[i].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        eprintln!("error: invalid port number: {}", args[i]);
                        return ExitCode::from(1);
                    }
                };
            }
            "--mode" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("error: --mode requires a value (strict or hardened)");
                    return ExitCode::from(1);
                }
                mode_str = match args[i].as_str() {
                    "strict" | "hardened" => &args[i],
                    other => {
                        eprintln!("error: unknown mode '{other}' (expected: strict, hardened)");
                        return ExitCode::from(1);
                    }
                };
            }
            "--help" | "-h" => {
                println!("frankenredis — FrankenRedis server");
                println!();
                println!("USAGE: frankenredis [OPTIONS]");
                println!();
                println!("OPTIONS:");
                println!("  --port <PORT>   Listen port (default: {DEFAULT_PORT})");
                println!("  --mode <MODE>   Runtime mode: strict or hardened (default: hardened)");
                println!("  --help          Show this help");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument: {other}");
                eprintln!("Try 'frankenredis --help' for usage.");
                return ExitCode::from(1);
            }
        }
        i += 1;
    }

    // Construct runtime from library crates.
    let policy = match mode_str {
        "strict" => RuntimePolicy::default(),
        _ => RuntimePolicy::hardened(),
    };
    let mut runtime = Runtime::new(policy);

    eprintln!(
        "FrankenRedis v{} starting (mode={mode_str}, port={port})",
        env!("CARGO_PKG_VERSION"),
    );
    // Placeholder: stdin RESP loop for smoke-testing the bootstrap path.
    // A real TCP listener is deferred to bd-zuyq.3.1.
    eprintln!("Accepting inline commands on stdin (one per line). Ctrl-D to quit.");
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    };

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Inline command format: space-separated arguments → RESP array
        let argv: Vec<RespFrame> = trimmed
            .split_whitespace()
            .map(|s| RespFrame::BulkString(Some(s.as_bytes().to_vec())))
            .collect();
        let frame = RespFrame::Array(Some(argv));
        let response = runtime.execute_frame(frame, now_ms());
        let _ = stdout.write_all(&response.to_bytes());
        let _ = stdout.flush();
    }

    eprintln!("FrankenRedis shutting down.");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use fr_config::RuntimePolicy;
    use fr_runtime::Runtime;

    #[test]
    fn server_bootstrap_creates_runtime() {
        // Smoke test: verify the Runtime can be constructed from both modes.
        let _strict = Runtime::new(RuntimePolicy::default());
        let _hardened = Runtime::new(RuntimePolicy::hardened());
    }

    #[test]
    fn server_bootstrap_processes_ping() {
        let mut runtime = Runtime::new(RuntimePolicy::hardened());
        let now_ms = 1_000_000u64;
        // RESP array format: *1\r\n$4\r\nPING\r\n
        let response = runtime.execute_bytes(b"*1\r\n$4\r\nPING\r\n", now_ms);
        let response_str = String::from_utf8_lossy(&response);
        assert!(
            response_str.contains("PONG"),
            "PING should return PONG, got: {response_str}"
        );
    }
}
