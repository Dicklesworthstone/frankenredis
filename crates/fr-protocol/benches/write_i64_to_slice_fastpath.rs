//! Meta-Lever 1 resurrection of the direct `write_i64_to_slice` digit path.
//!
//! The reference writes digits into a temporary `[u8; 20]` and copies them into the caller
//! buffer. The candidate writes the identical bytes directly into their final caller-slice
//! positions. One executable performs correctness, three exact-reference profiles, an A/A null,
//! and A/B. The decision threshold is the larger of twice the bootstrap 95% null-median CI
//! radius and the absolute 1.01 floor; CV is provenance only.

use std::{
    env, fs,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_protocol::bench_write_i64_to_slice;
use sha2::{Digest, Sha256};

const PROFILE_REPEATS: usize = 5_000_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 2_000_000;
const STAT_ROUNDS: usize = 24;
const BOOTSTRAP_RESAMPLES: usize = 20_000;
const TARGET_SYMBOL: &str = "fr_protocol::bench_write_i64_to_slice";

const CORPUS: [i64; 16] = [
    0,
    1,
    -1,
    7,
    42,
    -42,
    99,
    100,
    -100,
    999,
    1_024,
    -4_096,
    99_999,
    -1_000_000,
    2_147_483_647,
    i64::MIN,
];

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
}

fn render(n: i64, arm: Arm, buf: &mut [u8; 24]) -> usize {
    match arm {
        Arm::Candidate => bench_write_i64_to_slice::<true>(n, buf),
        Arm::Reference => bench_write_i64_to_slice::<false>(n, buf),
    }
}

fn run_loop(arm: Arm, repeats: usize) -> u64 {
    let mut buf = [0_u8; 24];
    let mut checksum = 0_u64;
    for _ in 0..repeats {
        for n in black_box(CORPUS) {
            let len = render(black_box(n), arm, black_box(&mut buf));
            checksum = checksum
                .rotate_left(5)
                .wrapping_add(len as u64)
                .wrapping_add(u64::from(buf[0]))
                .wrapping_add(u64::from(buf[len - 1]));
        }
    }
    black_box(checksum)
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

fn binary_identity(executable: &Path) -> Result<(String, usize), String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = fs::read(executable)
        .map_err(|error| format!("could not read {}: {error}", executable.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let mut digest = String::with_capacity(64);
    for byte in hasher.finalize() {
        digest.push(char::from(HEX[usize::from(byte >> 4)]));
        digest.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok((digest, bytes.len()))
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
    samples.sort_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2.0
    } else {
        samples[middle]
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    let mut state = 0x5eed_f00d_cafe_babe_u64;
    for sample in samples {
        state ^= sample.to_bits().wrapping_mul(0x9e37_79b9_7f4a_7c15);
        state = state.rotate_left(17);
    }
    let mut scratch = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(BOOTSTRAP_RESAMPLES);
    for _ in 0..BOOTSTRAP_RESAMPLES {
        for value in &mut scratch {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&mut scratch));
    }
    medians.sort_by(f64::total_cmp);
    (percentile(&medians, 0.025), percentile(&medians, 0.975))
}

fn correctness_gate() {
    let mut candidate = [0_u8; 24];
    let mut reference = [0_u8; 24];
    let mut checksum = 0_u64;
    for n in -300_000_i64..=300_000 {
        let candidate_len = render(n, Arm::Candidate, &mut candidate);
        let reference_len = render(n, Arm::Reference, &mut reference);
        assert_eq!(candidate_len, reference_len, "length differs for {n}");
        assert_eq!(
            &candidate[..candidate_len],
            &reference[..reference_len],
            "bytes differ for {n}"
        );
        checksum = checksum
            .rotate_left(7)
            .wrapping_add(candidate_len as u64)
            .wrapping_add(u64::from(candidate[0]));
    }
    for n in [
        i64::MIN,
        i64::MIN + 1,
        i64::MAX,
        i64::MAX - 1,
        i64::from(i32::MIN),
        i64::from(i32::MAX),
        -9_999_999_999,
        9_999_999_999,
    ] {
        let candidate_len = render(n, Arm::Candidate, &mut candidate);
        let reference_len = render(n, Arm::Reference, &mut reference);
        assert_eq!(
            candidate_len, reference_len,
            "boundary length differs for {n}"
        );
        assert_eq!(
            &candidate[..candidate_len],
            &reference[..reference_len],
            "boundary bytes differ for {n}"
        );
    }
    println!(
        "CORRECTNESS_GATE direct_write_matches_tmp_copy=bit_identical checksum={checksum:016x}"
    );
}

fn profile_trial(executable: &Path, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_write_i64_{}_{}_{}.data",
        process::id(),
        trial,
        stamp
    ));
    if data.exists() {
        return Err(format!("refusing to overwrite {}", data.display()));
    }
    let recorded = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
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
        .args([
            "--child",
            Arm::Reference.name(),
            &PROFILE_REPEATS.to_string(),
        ])
        .output()
        .map_err(|error| format!("could not launch perf record: {error}"))?;
    if !recorded.status.success() {
        return Err(format!(
            "perf record failed: {}",
            String::from_utf8_lossy(&recorded.stderr)
        ));
    }
    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "-i",
            data.to_str().ok_or("non-UTF-8 perf.data path")?,
            "--stdio",
            "--no-children",
            "--percent-limit",
            "0.1",
        ])
        .output()
        .map_err(|error| format!("could not launch perf report: {error}"))?;
    if !report.status.success() {
        return Err(format!(
            "perf report failed: {}",
            String::from_utf8_lossy(&report.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&report.stdout);
    println!("PROFILE_TABLE_BEGIN trial={trial}\n{stdout}\nPROFILE_TABLE_END trial={trial}");
    let self_pct = stdout
        .lines()
        .filter(|line| line.contains(TARGET_SYMBOL))
        .find_map(|line| {
            line.split_whitespace()
                .next()?
                .trim_end_matches('%')
                .parse::<f64>()
                .ok()
        })
        .ok_or("profile has no numeric exact reference frame; workload INVALID")?;
    if self_pct <= 0.0 {
        return Err("reference frame has zero self-time; workload INVALID".into());
    }
    Ok(self_pct)
}

fn run_profiles(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");
    let mut samples = Vec::with_capacity(PROFILE_TRIALS);
    for trial in 1..=PROFILE_TRIALS {
        let self_pct = profile_trial(executable, trial)?;
        println!("PROFILE_SELF arm=reference trial={trial} self_pct={self_pct:.4}");
        samples.push(self_pct);
    }
    let self_cv_pct = cv(&samples);
    let median_self_pct = median(&mut samples);
    println!(
        "PROFILE_SELF_SUMMARY arm=reference trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} self_cv_pct={self_cv_pct:.4} samples={samples:?} lost_samples=0"
    );
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
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

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut nulls = Vec::with_capacity(STAT_ROUNDS);
    let mut effects = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Candidate
            } else {
                Arm::Reference
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[0] as f64 / counts[2] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} reference_a={} reference_b={} candidate={} null_ratio={null:.9} reference_over_candidate={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let (null_ci95_low, null_ci95_high) = bootstrap_median_ci(&nulls);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let radius = (null_ci95_low - 1.0)
        .abs()
        .max((null_ci95_high - 1.0).abs());
    let decisive_threshold = (1.0 + 2.0 * radius).max(1.01);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_ci95_low={null_ci95_low:.9} null_ci95_high={null_ci95_high:.9} bootstrap_resamples={BOOTSTRAP_RESAMPLES} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} effect_cv_pct={effect_cv_pct:.6} decisive_threshold={decisive_threshold:.9}"
    );
    if null_ci95_low > 1.0 || null_ci95_high < 1.0 {
        return Err(format!(
            "A/A null CI does not bracket 1.0: [{null_ci95_low:.9}, {null_ci95_high:.9}]"
        ));
    }
    let verdict = if effect_median >= decisive_threshold {
        "KEEP"
    } else if effect_median.recip() >= decisive_threshold {
        "REJECT"
    } else {
        "NULL"
    };
    println!(
        "DECISION verdict={verdict} gate=median_ci_95 two_x_margin=true absolute_floor=1.01 cv_used_as_provenance_only=true"
    );
    Ok(())
}

fn main() -> Result<(), String> {
    if let Some((arm, repeats)) = child_args()? {
        black_box(run_loop(arm, repeats));
        return Ok(());
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve bench executable: {error}"))?;
    let (sha256, bytes) = binary_identity(&executable)?;
    println!(
        "bench_elf_sha256={sha256} ({bytes} bytes) {}",
        executable.display()
    );
    correctness_gate();
    run_profiles(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
