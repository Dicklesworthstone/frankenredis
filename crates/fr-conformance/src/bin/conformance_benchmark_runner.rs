#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::process::{Command, ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Round {
    Round1,
    Round2,
}

impl Round {
    const fn as_str(self) -> &'static str {
        match self {
            Round::Round1 => "round1",
            Round::Round2 => "round2",
        }
    }

    const fn hyperfine_warmup(self) -> u32 {
        match self {
            Round::Round1 => 2,
            Round::Round2 => 1,
        }
    }

    const fn hyperfine_runs(self) -> u32 {
        match self {
            Round::Round1 => 5,
            Round::Round2 => 3,
        }
    }

    const fn baseline_json(self) -> &'static str {
        match self {
            Round::Round1 => "baselines/round1_conformance_baseline.json",
            Round::Round2 => "baselines/round2_protocol_negative_baseline.json",
        }
    }

    const fn strace_output(self) -> &'static str {
        match self {
            Round::Round1 => "baselines/round1_conformance_strace.txt",
            Round::Round2 => "baselines/round2_protocol_negative_strace.txt",
        }
    }

    const fn test_args(self) -> &'static [&'static str] {
        match self {
            Round::Round1 => &[
                "test",
                "-p",
                "fr-conformance",
                "smoke_report_is_stable",
                "--test",
                "smoke",
                "--",
                "--exact",
                "--nocapture",
            ],
            Round::Round2 => &[
                "test",
                "-p",
                "fr-conformance",
                "tests::conformance_protocol_fixture_passes",
                "--",
                "--exact",
                "--nocapture",
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    round: Round,
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
    let cli = match parse_args(env::args().skip(1).collect())? {
        Some(cli) => cli,
        None => {
            println!("{}", usage());
            return Ok(ExitCode::SUCCESS);
        }
    };

    fs::create_dir_all("baselines")
        .map_err(|err| format!("failed to create baselines directory: {err}"))?;

    run_hyperfine(cli.round)?;
    run_strace(cli.round)?;

    println!("wrote {}", cli.round.baseline_json());
    println!("wrote {}", cli.round.strace_output());
    Ok(ExitCode::SUCCESS)
}

fn parse_args(raw_args: Vec<String>) -> Result<Option<CliArgs>, String> {
    if raw_args.len() == 1 && matches!(raw_args[0].as_str(), "-h" | "--help") {
        return Ok(None);
    }

    let mut round: Option<Round> = None;
    let mut idx = 0usize;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "--round" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --round".to_string())?;
                round = Some(parse_round(value)?);
                idx += 2;
            }
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    let round = round.ok_or_else(|| format!("missing --round\n{}", usage()))?;
    Ok(Some(CliArgs { round }))
}

fn parse_round(raw: &str) -> Result<Round, String> {
    match raw {
        "round1" => Ok(Round::Round1),
        "round2" => Ok(Round::Round2),
        _ => Err(format!(
            "invalid --round value '{raw}': expected round1|round2"
        )),
    }
}

fn run_hyperfine(round: Round) -> Result<(), String> {
    let warmup = round.hyperfine_warmup().to_string();
    let runs = round.hyperfine_runs().to_string();
    let baseline_json = round.baseline_json();

    let command_text = shell_join(
        std::iter::once("cargo")
            .chain(round.test_args().iter().copied())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
            .as_slice(),
    );

    let status = Command::new("hyperfine")
        .args([
            "--warmup",
            &warmup,
            "--runs",
            &runs,
            "--export-json",
            baseline_json,
            &command_text,
        ])
        .status()
        .map_err(|err| format!("failed to execute hyperfine: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "hyperfine failed for {} with exit {}",
            round.as_str(),
            status.code().unwrap_or(1)
        ))
    }
}

fn run_strace(round: Round) -> Result<(), String> {
    let mut cmd = Command::new("strace");
    cmd.arg("-c")
        .arg("-o")
        .arg(round.strace_output())
        .arg("cargo")
        .args(round.test_args());
    let status = cmd
        .status()
        .map_err(|err| format!("failed to execute strace: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "strace command failed for {} with exit {}",
            round.as_str(),
            status.code().unwrap_or(1)
        ))
    }
}

fn usage() -> String {
    "usage: cargo run -p fr-conformance --bin conformance_benchmark_runner -- --round <round1|round2>"
        .to_string()
}

fn shell_join(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|token| shell_escape(token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(token: &str) -> String {
    if token.is_empty() {
        return "''".to_string();
    }
    if token.bytes().all(|ch| {
        matches!(
            ch,
            b'a'..=b'z'
                | b'A'..=b'Z'
                | b'0'..=b'9'
                | b'/'
                | b'.'
                | b'_'
                | b'-'
                | b':'
                | b'='
                | b'+'
        )
    }) {
        return token.to_string();
    }
    let escaped = token.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::{CliArgs, Round, parse_args, parse_round, shell_escape};

    #[test]
    fn parse_args_accepts_round_flag() {
        let parsed = parse_args(vec!["--round".to_string(), "round1".to_string()])
            .expect("arguments parse")
            .expect("help not requested");
        assert_eq!(
            parsed,
            CliArgs {
                round: Round::Round1
            }
        );
    }

    #[test]
    fn parse_round_rejects_invalid_value() {
        let err = parse_round("bad").expect_err("invalid round should fail");
        assert!(err.contains("expected round1|round2"));
    }

    #[test]
    fn shell_escape_quotes_special_tokens() {
        assert_eq!(shell_escape("safe-token"), "safe-token");
        assert_eq!(shell_escape("space token"), "'space token'");
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }
}
