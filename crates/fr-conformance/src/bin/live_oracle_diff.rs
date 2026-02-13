#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use fr_conformance::{
    HarnessConfig, LiveOracleConfig, run_live_redis_diff, run_live_redis_protocol_diff,
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
    let mut args = env::args().skip(1);
    let mode = args
        .next()
        .ok_or_else(|| usage("missing mode; expected 'command' or 'protocol'"))?;
    let fixture = args
        .next()
        .ok_or_else(|| usage("missing fixture file name (e.g. core_errors.json)"))?;

    let host = args.next().unwrap_or_else(|| "127.0.0.1".to_string());
    let port = match args.next() {
        Some(text) => text
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{text}'"))?,
        None => 6379,
    };

    let cfg = HarnessConfig::default_paths();
    let oracle = LiveOracleConfig {
        host,
        port,
        ..LiveOracleConfig::default()
    };

    let report = match mode.as_str() {
        "command" => run_live_redis_diff(&cfg, &fixture, &oracle)?,
        "protocol" => run_live_redis_protocol_diff(&cfg, &fixture, &oracle)?,
        _ => return Err(usage("mode must be 'command' or 'protocol'")),
    };

    println!("suite: {}", report.suite);
    println!("total: {}", report.total);
    println!("passed: {}", report.passed);
    println!("failed: {}", report.failed.len());

    if !report.failed.is_empty() {
        for failure in &report.failed {
            println!("---");
            println!("case: {}", failure.name);
            println!("expected(redis): {:?}", failure.expected);
            println!("actual(runtime):  {:?}", failure.actual);
        }
        return Ok(ExitCode::from(1));
    }

    Ok(ExitCode::SUCCESS)
}

fn usage(reason: &str) -> String {
    format!(
        "{reason}\nusage: cargo run -p fr-conformance --bin live_oracle_diff -- <command|protocol> <fixture.json> [host] [port]"
    )
}
