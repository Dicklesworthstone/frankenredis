//! Same-binary proof for the inline command tokenizer's ordinary-token path.
//!
//! Both arms live in one release binary. The parent profiles exact helper symbols, proves exact
//! output parity over edge cases, and then runs position-balanced candidate A/A and reference/A
//! instruction measurements.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_server::{bench_split_inline_args_reference, split_inline_args};

const PROFILE_REPEATS: usize = 300_000;
const STAT_REPEATS: usize = 20_000;
const STAT_ROUNDS: usize = 9;

#[derive(Clone, Copy)]
enum Arm {
    Candidate,
    Reference,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unknown arm {value:?}")),
        }
    }

    const fn profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "fr_server::split_inline_args",
            Self::Reference => "fr_server::bench_split_inline_args_reference",
        }
    }

    const fn wrong_profile_symbol(self) -> &'static str {
        match self {
            Self::Candidate => "bench_split_inline_args_reference",
            Self::Reference => "fr_server::split_inline_args",
        }
    }
}

fn timing_corpus() -> Vec<Vec<u8>> {
    let long_key = "k".repeat(96);
    let long_value = "v".repeat(384);
    vec![
        b"PING".to_vec(),
        b"GET session:18446744073709551615".to_vec(),
        b"SET customer:10392 active NX GET".to_vec(),
        b"MGET alpha beta gamma delta epsilon zeta eta theta".to_vec(),
        b"HSET profile:928 name Ada role engineer region west active 1".to_vec(),
        b"ZADD leaderboard 42 alice 17 bob 105 carol 88 dave".to_vec(),
        format!("SET {long_key} {long_value} EX 60 NX").into_bytes(),
        b"  XADD\tstream:events\rMAXLEN\n1000 * kind update object 9482  ".to_vec(),
    ]
}

fn tokenize(line: &[u8], arm: Arm) -> Result<Vec<Vec<u8>>, &'static str> {
    match arm {
        Arm::Candidate => split_inline_args(line),
        Arm::Reference => bench_split_inline_args_reference(line),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let corpus = timing_corpus();
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        for line in &corpus {
            let outcome = tokenize(black_box(line.as_slice()), black_box(arm));
            match black_box(&outcome) {
                Ok(args) => {
                    checksum = checksum.wrapping_add(args.len() as u64);
                    for arg in args {
                        checksum = checksum
                            .wrapping_add(arg.len() as u64)
                            .wrapping_add(u64::from(arg.first().copied().unwrap_or(0)))
                            .wrapping_add(u64::from(arg.last().copied().unwrap_or(0)));
                    }
                }
                Err(message) => checksum = checksum.wrapping_add(message.len() as u64),
            }
            let _ = black_box(outcome);
        }
    }
    black_box(checksum);
}

fn generated_correctness_cases() -> Vec<Vec<u8>> {
    let mut cases = vec![
        Vec::new(),
        b"   \t\r\n".to_vec(),
        b"PING".to_vec(),
        b"SET key value".to_vec(),
        b"SET\tkey\rvalue\nEX 10".to_vec(),
        b"SET key \"hello world\"".to_vec(),
        b"SET key 'hello world'".to_vec(),
        b"SET ab\"c d\" e".to_vec(),
        b"SET key \"line\\nfeed\\x21\"".to_vec(),
        b"SET key 'it\\'s fine'".to_vec(),
        b"SET key \"unclosed".to_vec(),
        b"SET key 'unclosed".to_vec(),
        b"SET key \"closed\"junk".to_vec(),
        b"PING\0ignored arguments".to_vec(),
        b"SET key \"quoted\0tail".to_vec(),
        vec![b'S', b'E', b'T', b' ', 0xff, b' ', 0x80],
    ];
    for token_len in [1, 2, 7, 16, 63, 256, 1_024] {
        let mut line = b"MSET ".to_vec();
        line.extend(std::iter::repeat_n(b'k', token_len));
        line.push(b' ');
        line.extend(std::iter::repeat_n(b'v', token_len));
        cases.push(line);
    }
    for count in [1, 2, 8, 32, 128] {
        let mut line = Vec::new();
        for index in 0..count {
            if index != 0 {
                line.push(if index % 2 == 0 { b'\t' } else { b' ' });
            }
            line.extend_from_slice(format!("arg{index:03}").as_bytes());
        }
        cases.push(line);
    }
    cases
}

fn correctness_gate() {
    let mut cases = generated_correctness_cases();
    cases.extend(timing_corpus());
    for (index, line) in cases.iter().enumerate() {
        assert_eq!(
            tokenize(line, Arm::Candidate),
            tokenize(line, Arm::Reference),
            "inline tokenizer differs for case {index}: {line:?}"
        );
    }
    let trigger_bytes = timing_corpus().iter().map(Vec::len).sum::<usize>();
    println!(
        "CORRECTNESS_GATE exact_results=identical cases={} timing_corpus_lines=8 timing_corpus_bytes={trigger_bytes}",
        cases.len()
    );
}

fn child_args() -> Result<Option<(Arm, usize)>, String> {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("--child") {
        return Ok(None);
    }
    let arm = Arm::parse(args.get(2).ok_or("missing child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing child repeat count")?
        .parse()
        .map_err(|error| format!("invalid repeat count: {error}"))?;
    Ok(Some((arm, repeats)))
}

fn binary_sha256(executable: &Path) -> Result<String, String> {
    let output = Command::new("sha256sum")
        .arg(executable)
        .output()
        .map_err(|error| format!("could not launch sha256sum: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum emitted no digest".to_owned())
}

fn worker_id() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn measured_output(mut command: Command, label: &str) -> Result<std::process::Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not launch {label}: {error}"))?;
    if output.status.code() == Some(124) {
        return Err(format!("{label} exceeded its measurement cap"));
    }
    Ok(output)
}

fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_server_inline_args_{}_{}_{}.data",
        process::id(),
        arm.name(),
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let mut record = Command::new("timeout");
    record
        .args([
            "--foreground",
            "45s",
            "perf",
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "instructions:u",
            "-g",
            "-o",
        ])
        .arg(&data)
        .arg("--")
        .arg(executable)
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()]);
    let recorded = measured_output(record, "perf record")?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let mut report = Command::new("timeout");
    report.args([
        "--foreground",
        "15s",
        "perf",
        "report",
        "-i",
        data.to_str().ok_or("non-UTF-8 perf.data path")?,
        "--stdio",
        "--no-children",
        "-g",
        "none",
        "--percent-limit",
        "0.01",
    ]);
    let report = measured_output(report, "perf report")?;
    if !report.status.success() {
        return Err(format!(
            "perf report failed: {}",
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!(
        "PROFILE_TABLE_BEGIN arm={}\n{stdout}\nPROFILE_TABLE_END arm={}",
        arm.name(),
        arm.name()
    );
    let lost_samples = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples; profile provenance INVALID")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost_samples != 0 {
        return Err(format!("profile lost {lost_samples} samples"));
    }
    if stdout
        .lines()
        .any(|line| line.contains(arm.wrong_profile_symbol()))
    {
        return Err(format!(
            "{} profile executed wrong helper {}",
            arm.name(),
            arm.wrong_profile_symbol()
        ));
    }
    let line = stdout
        .lines()
        .find(|line| line.contains(arm.profile_symbol()) && !line.contains("closure#"))
        .ok_or_else(|| {
            format!(
                "profile has no exact {} helper frame; workload INVALID",
                arm.name()
            )
        })?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{} helper has zero self-time", arm.name()));
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path, arms: &[Arm]) -> Result<(), String> {
    let corpus = timing_corpus();
    println!("WORKER_ID {}", worker_id());
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    println!(
        "TRIGGER lines={} bytes={} plain_token_fast_path=8_of_8",
        corpus.len(),
        corpus.iter().map(Vec::len).sum::<usize>()
    );
    for &arm in arms {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = profile_trial(executable, arm)?;
        println!("PROFILE_SELF arm={} self_pct={self_pct:.4}", arm.name());
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let mut command = Command::new("timeout");
    command
        .args([
            "--foreground",
            "30s",
            "perf",
            "stat",
            "--no-big-num",
            "-x,",
            "-e",
            "instructions:u",
            "--",
        ])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()]);
    let output = measured_output(command, "perf stat")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(',').collect();
            fields
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| fields[0].trim())
        })
        .ok_or_else(|| format!("instructions:u missing: {stderr}"))?
        .parse()
        .map_err(|error| format!("invalid instruction count: {error}"))
}

fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
    samples[samples.len() / 2]
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    let mut candidate_counts = Vec::with_capacity(STAT_ROUNDS);
    let mut reference_counts = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Reference
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
        candidate_counts.push(counts[0] as f64);
        reference_counts.push(counts[2] as f64);
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let candidate_median = median(&mut candidate_counts);
    let reference_median = median(&mut reference_counts);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} candidate_median={candidate_median:.0} reference_median={reference_median:.0} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "candidate failed keep gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        run_loop(arm, repeats);
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    correctness_gate();
    let candidate_profile_only = env::args().any(|arg| arg == "--profile-candidate-only");
    if candidate_profile_only {
        return run_profile(&executable, &[Arm::Candidate])
            .map_err(|error| format!("PROFILE INVALID: {error}"));
    }
    run_profile(&executable, &[Arm::Candidate, Arm::Reference])
        .map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
