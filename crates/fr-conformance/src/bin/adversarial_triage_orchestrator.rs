#![forbid(unsafe_code)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    manifest: PathBuf,
    output_root: PathBuf,
    run_id: String,
    runner: Runner,
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
    let mut manifest = env::var("FR_ADV_MANIFEST").unwrap_or_else(|_| {
        "crates/fr-conformance/fixtures/adversarial_corpus_v1.json".to_string()
    });
    let mut output_root = env::var("FR_ADV_OUTPUT_ROOT")
        .unwrap_or_else(|_| "artifacts/adversarial_triage".to_string());
    let mut run_id = env::var("FR_ADV_RUN_ID").unwrap_or_else(|_| compact_utc_timestamp());
    let mut runner =
        Runner::from_str(&env::var("FR_ADV_RUNNER").unwrap_or_else(|_| "rch".to_string()))?;

    let mut idx = 0usize;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "-h" | "--help" => return Ok(None),
            "--manifest" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --manifest".to_string())?;
                manifest = value.clone();
                idx += 2;
            }
            "--output-root" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --output-root".to_string())?;
                output_root = value.clone();
                idx += 2;
            }
            "--run-id" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --run-id".to_string())?;
                run_id = value.clone();
                idx += 2;
            }
            "--runner" => {
                let value = raw_args
                    .get(idx + 1)
                    .ok_or_else(|| "missing value after --runner".to_string())?;
                runner = Runner::from_str(value)?;
                idx += 2;
            }
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    Ok(Some(CliArgs {
        manifest: PathBuf::from(manifest),
        output_root: PathBuf::from(output_root),
        run_id,
        runner,
    }))
}

fn command_tokens(cli: &CliArgs) -> Vec<String> {
    let base = vec![
        "cargo".to_string(),
        "run".to_string(),
        "-p".to_string(),
        "fr-conformance".to_string(),
        "--bin".to_string(),
        "adversarial_triage".to_string(),
        "--".to_string(),
        "--manifest".to_string(),
        path_display(&cli.manifest),
        "--output-root".to_string(),
        path_display(&cli.output_root),
        "--run-id".to_string(),
        cli.run_id.clone(),
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

fn path_display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn compact_utc_timestamp() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output();
    if let Ok(output) = output
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    "19700101T000000Z".to_string()
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
    "Usage:\n  cargo run -p fr-conformance --bin adversarial_triage_orchestrator -- [--manifest <path>] [--output-root <dir>] [--run-id <id>] [--runner <rch|local>]\n\nDescription:\n  Runs adversarial triage with deterministic defaults and optional remote execution via rch.\n\nEnvironment:\n  FR_ADV_MANIFEST       default manifest path\n  FR_ADV_OUTPUT_ROOT    default output root\n  FR_ADV_RUN_ID         default run id\n  FR_ADV_RUNNER         default runner (rch|local, default: rch)"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{Runner, command_tokens, parse_args};

    #[test]
    fn parse_args_accepts_explicit_runner() {
        let cli = parse_args(vec![
            "--manifest".to_string(),
            "a.json".to_string(),
            "--output-root".to_string(),
            "out".to_string(),
            "--run-id".to_string(),
            "RID".to_string(),
            "--runner".to_string(),
            "local".to_string(),
        ])
        .expect("parse should succeed")
        .expect("help should not be requested");
        assert_eq!(cli.runner, Runner::Local);
        assert_eq!(cli.run_id, "RID");
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(vec!["--bad".to_string()]).expect_err("unknown flag should fail");
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn command_tokens_wrap_with_rch_when_requested() {
        let cli = parse_args(vec![
            "--manifest".to_string(),
            "m.json".to_string(),
            "--output-root".to_string(),
            "artifacts/x".to_string(),
            "--run-id".to_string(),
            "run-1".to_string(),
            "--runner".to_string(),
            "rch".to_string(),
        ])
        .expect("parse should succeed")
        .expect("help should not be requested");

        let cmd = command_tokens(&cli);
        assert_eq!(cmd[0], "~/.local/bin/rch");
        assert_eq!(cmd[1], "exec");
        assert!(cmd.contains(&"adversarial_triage".to_string()));
    }
}
