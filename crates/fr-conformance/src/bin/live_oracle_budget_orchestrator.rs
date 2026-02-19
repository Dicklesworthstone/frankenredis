#![forbid(unsafe_code)]

use std::env;
use std::process::{Command, ExitCode};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    runner: Runner,
    coverage_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    Local,
    Rch,
}

impl Runner {
    fn from_str(raw: &str) -> Result<Self, String> {
        match raw {
            "local" => Ok(Self::Local),
            "rch" => Ok(Self::Rch),
            _ => Err(format!(
                "invalid --runner value '{raw}': expected local|rch"
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Rch => "rch",
        }
    }
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

    let cmd = command_tokens(&cli);
    println!("runner={}", cli.runner.as_str());
    println!("cmd={}", shell_join(&cmd));

    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .status()
        .map_err(|err| format!("failed to execute command: {err}"))?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn parse_args(raw_args: Vec<String>) -> Result<Option<CliArgs>, String> {
    let mut runner =
        Runner::from_str(&env::var("FR_BUDGET_RUNNER").unwrap_or_else(|_| "local".to_string()))?;
    let mut positional = Vec::new();

    let mut idx = 0usize;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "-h" | "--help" if raw_args.len() == 1 => return Ok(None),
            "--runner" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --runner".to_string())?;
                runner = Runner::from_str(value)?;
                idx += 2;
            }
            "--" => {
                positional.extend(raw_args[idx + 1..].iter().cloned());
                break;
            }
            other => {
                positional.push(other.to_string());
                idx += 1;
            }
        }
    }

    if positional.len() != 1 {
        return Err(format!(
            "expected exactly one coverage summary path; got {}\n{}",
            positional.len(),
            usage()
        ));
    }

    Ok(Some(CliArgs {
        runner,
        coverage_summary: positional[0].clone(),
    }))
}

fn command_tokens(cli: &CliArgs) -> Vec<String> {
    let base = vec![
        "cargo".to_string(),
        "run".to_string(),
        "-p".to_string(),
        "fr-conformance".to_string(),
        "--bin".to_string(),
        "live_oracle_budget_gate".to_string(),
        "--".to_string(),
        cli.coverage_summary.clone(),
    ];

    if cli.runner == Runner::Rch {
        let mut with_runner = vec![
            "~/.local/bin/rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
        ];
        with_runner.extend(base);
        with_runner
    } else {
        base
    }
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
                | b'~'
        )
    }) {
        return token.to_string();
    }
    let escaped = token.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn usage() -> String {
    "Usage:\n  cargo run -p fr-conformance --bin live_oracle_budget_orchestrator -- [--runner <local|rch>] <coverage_summary.json>\n\nDescription:\n  Delegates to `live_oracle_budget_gate` with optional remote execution via rch.\n\nEnvironment:\n  FR_BUDGET_RUNNER       default runner (local|rch, default: local)"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{Runner, command_tokens, parse_args};

    #[test]
    fn parse_args_accepts_runner_and_single_positional() {
        let parsed = parse_args(vec![
            "--runner".to_string(),
            "rch".to_string(),
            "cov.json".to_string(),
        ])
        .expect("args parse")
        .expect("help not requested");
        assert_eq!(parsed.runner, Runner::Rch);
        assert_eq!(parsed.coverage_summary, "cov.json".to_string());
    }

    #[test]
    fn parse_args_rejects_wrong_arity() {
        let err = parse_args(vec!["a.json".to_string(), "b.json".to_string()])
            .expect_err("extra args should fail");
        assert!(err.contains("expected exactly one coverage summary path"));
    }

    #[test]
    fn command_tokens_wrap_with_rch_when_requested() {
        let parsed = parse_args(vec![
            "--runner".to_string(),
            "rch".to_string(),
            "cov.json".to_string(),
        ])
        .expect("args parse")
        .expect("help not requested");
        let cmd = command_tokens(&parsed);
        assert_eq!(cmd[0], "~/.local/bin/rch");
        assert!(cmd.contains(&"live_oracle_budget_gate".to_string()));
        assert!(cmd.contains(&"cov.json".to_string()));
    }
}
