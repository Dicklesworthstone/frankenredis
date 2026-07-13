//! Same-binary A/B for the script_cache hasher: foldhash (candidate = production) vs std SipHash
//! (reference), over SHA1-hex String-key lookups (the EVALSHA path). Same keys, same gets; only the
//! BuildHasher differs. SHA1 keys aren't attacker-collidable, so foldhash adds no DoS surface.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use std::collections::HashMap;

type FoldMap = HashMap<String, u64, foldhash::quality::RandomState>;
type SipMap = HashMap<String, u64>;

const PROFILE_REPEATS: usize = 5_000_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 3_000_000;
const STAT_ROUNDS: usize = 24;

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

// Realistic multibulk count lines (`*<N>\r\n`), positioned at the count digits as
// `parse_multibulk_count` receives them (just past the `*`): common command arities.
const CORPUS: [&[u8]; 16] = [
    b"e0f579367dfba48849b901f2b410d722632c0963",
    b"a4e7ea12fc4777eb99919f1375f785477be36ef3",
    b"5f78a8dd0f44c471efc2a21445ae19ad415a89d3",
    b"c557baa72314824f8b98241f29937a89d1322df2",
    b"9f4088ab954d206d45307cb0a361147d8afe4d2b",
    b"2b93519a24f2149254492ce54e393d15375a9aca",
    b"72b7475eacfba61a652d3c57985146afbd6ff915",
    b"1ae2f61d2119a9a3e806a301a42d8dec443fd00d",
    b"47f18270f2f8fd20f14a08382febf6757ea771f6",
    b"d9dfc3f769bb3c606c2a0d193b6d6f663d4647ff",
    b"e261649d6873b2b02b5211fcb3ed6223e99bab3b",
    b"eced38922a4af6c26078668fb771d74cbad543f3",
    b"aaba022d5e89fe1f6afd56791554380b86b6d5c0",
    b"07b73c24f7772ae552b87366e205ea7df9d9db67",
    b"870d7b42421341bd7657bcb3167d39131e2def09",
    b"8f9f9db0e5092dc4b89ef4d4d63f710aefae8f53",
];

#[inline(never)]
fn probe_candidate_foldhash(m: &FoldMap, key: &str) -> u64 {
    m.get(key).copied().unwrap_or(0)
}

#[inline(never)]
fn probe_reference_sip(m: &SipMap, key: &str) -> u64 {
    m.get(key).copied().unwrap_or(0)
}

fn run_loop(arm: Arm, repeats: usize) {
    // Pre-populate a map of each hasher with the same keys, then repeatedly get() every key. Same
    // keys, same lookups; only the BuildHasher differs, so the delta is purely the hash cost.
    let mut checksum = 0_u64;
    match arm {
        Arm::Candidate => {
            let mut m = FoldMap::default();
            for (i, k) in CORPUS.iter().enumerate() {
                m.insert(std::str::from_utf8(k).unwrap().to_owned(), i as u64);
            }
            for _ in 0..repeats {
                for key in black_box(CORPUS) {
                    let ks = std::str::from_utf8(key).unwrap();
                    checksum = checksum.wrapping_add(probe_candidate_foldhash(&m, black_box(ks)));
                }
            }
        }
        Arm::Reference => {
            let mut m = SipMap::new();
            for (i, k) in CORPUS.iter().enumerate() {
                m.insert(std::str::from_utf8(k).unwrap().to_owned(), i as u64);
            }
            for _ in 0..repeats {
                for key in black_box(CORPUS) {
                    let ks = std::str::from_utf8(key).unwrap();
                    checksum = checksum.wrapping_add(probe_reference_sip(&m, black_box(ks)));
                }
            }
        }
    }
    black_box(checksum);
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

fn profile_trial(executable: &Path, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_script_cache_hasher_{}_{}_{}.data",
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
    let line = stdout
        .lines()
        .find(|line| line.contains("probe_reference_sip"))
        .ok_or("profile has no exact old parser frame; workload INVALID")?;
    let self_pct = line
        .split_whitespace()
        .next()
        .ok_or("missing self-time")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid self-time: {error}"))?;
    if self_pct <= 0.0 {
        return Err("old parser has zero self-time; workload INVALID".into());
    }
    Ok(self_pct)
}

fn run_profile(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 both_arms={}", binary_sha256(executable)?);
    for arm in [Arm::Reference, Arm::Candidate] {
        let status = Command::new(executable)
            .args(["--child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch warm-up: {error}"))?;
        if !status.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
    }
    let mut samples = Vec::with_capacity(PROFILE_TRIALS);
    for trial in 1..=PROFILE_TRIALS {
        let self_pct = profile_trial(executable, trial)?;
        println!("PROFILE_SELF arm=reference trial={trial} self_pct={self_pct:.4}");
        samples.push(self_pct);
    }
    let self_cv_pct = cv(&samples);
    let median_self_pct = median(&mut samples);
    println!(
        "PROFILE_SELF_SUMMARY arm=reference trials={PROFILE_TRIALS} median_self_pct={median_self_pct:.4} self_cv_pct={self_cv_pct:.4} samples={samples:?}"
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

fn correctness_gate() {
    let mut fold = FoldMap::default();
    let mut sip = SipMap::new();
    for (i, k) in CORPUS.iter().enumerate() {
        let ks = std::str::from_utf8(k).unwrap().to_owned();
        fold.insert(ks.clone(), i as u64);
        sip.insert(ks, i as u64);
    }
    for key in CORPUS {
        let ks = std::str::from_utf8(key).unwrap();
        assert_eq!(probe_candidate_foldhash(&fold, ks), probe_reference_sip(&sip, ks));
    }
    for miss in ["absent", "", "0000000000000000000000000000000000000000"] {
        assert_eq!(probe_candidate_foldhash(&fold, miss), probe_reference_sip(&sip, miss));
    }
    println!("CORRECTNESS_GATE script_cache_hasher_lookups_identical=identical");
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
    }
    let null_cv_pct = cv(&nulls);
    let effect_cv_pct = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let null_p05 = percentile(&nulls, 0.05);
    let null_p95 = percentile(&nulls, 0.95);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={effect_median:.9} speedup_cv_pct={effect_cv_pct:.6}"
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
    run_profile(&executable).map_err(|error| format!("PROFILE INVALID: {error}"))?;
    run_instruction_ab(&executable).map_err(|error| format!("A/B INVALID: {error}"))
}
