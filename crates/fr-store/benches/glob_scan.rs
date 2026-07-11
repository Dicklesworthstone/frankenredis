//! Same-binary A/B for SCAN-style glob matching: classify-per-call (`glob_match`) vs classify-once
//! (`glob_prepare` + `PreparedGlob::matches`), null-gated on the median.
//!
//! Substrate matches the other benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated per size, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread.
//!
//! Models `SCAN MATCH <pattern>` over a keyspace: a fixed pattern matched against every key. ORIG
//! re-classifies the pattern per key (the shipped `scan_pattern_matches` path); CAND classifies once
//! and matches per key. Both return identical results (asserted before timing).

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use fr_store::{Store, glob_match, glob_prepare};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NKEYS: usize = 20_000;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const ZSCAN_MEMBERS: usize = 8_192;
const ZSCAN_PROFILE_REPEATS: usize = 1_000;
const ZSCAN_PROFILE_TRIALS: usize = 5;
const ZSCAN_STAT_REPEATS: usize = 200;
const ZSCAN_STAT_ROUNDS: usize = 24;

type ZscanFn = for<'a> fn(
    &'a mut Store,
    &[u8],
    u64,
    Option<&[u8]>,
    usize,
    u64,
) -> Result<(u64, Vec<(Vec<u8>, f64)>), fr_store::StoreError>;

#[derive(Clone, Copy, Debug)]
enum ZscanArm {
    Candidate,
    Reference,
}

impl ZscanArm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Reference => "reference",
        }
    }

    const fn symbol(self) -> &'static str {
        match self {
            Self::Candidate => "<fr_store::Store>::zscan",
            Self::Reference => "<fr_store::Store>::zscan_classify_per_member_reference",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unknown ZSCAN child arm {value:?}")),
        }
    }

    const fn function(self) -> ZscanFn {
        match self {
            Self::Candidate => Store::zscan,
            Self::Reference => Store::zscan_classify_per_member_reference,
        }
    }
}

fn seed_zscan_store() -> Store {
    let mut store = Store::new();
    let pairs = (0..ZSCAN_MEMBERS)
        .map(|i| {
            let class = if i % 2 == 0 { "hit" } else { "miss" };
            (i as f64, format!("{class}:{i:08}:tag").into_bytes())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.zadd(b"z", &pairs, 1).expect("seed zset"),
        ZSCAN_MEMBERS
    );
    store
}

#[inline(never)]
fn run_zscan_arm(arm: ZscanArm, repeats: usize) {
    let mut store = seed_zscan_store();
    let scan: ZscanFn = black_box(arm.function());
    let mut checksum = 0_u64;
    for now_ms in 2..2 + repeats as u64 {
        let (cursor, pairs) = scan(
            black_box(&mut store),
            black_box(b"z"),
            black_box(0),
            black_box(Some(b"hit:*")),
            black_box(ZSCAN_MEMBERS),
            black_box(now_ms),
        )
        .expect("profile ZSCAN");
        checksum ^= cursor;
        for (member, score) in pairs {
            checksum = checksum
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(member.len() as u64)
                ^ score.to_bits();
        }
    }
    black_box(checksum);
}

fn zscan_child() -> Result<Option<(ZscanArm, usize)>, String> {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) != Some("--zscan-child") {
        return Ok(None);
    }
    let arm = ZscanArm::parse(args.get(2).ok_or("missing ZSCAN child arm")?)?;
    let repeats = args
        .get(3)
        .ok_or("missing ZSCAN child repeat count")?
        .parse()
        .map_err(|error| format!("invalid ZSCAN child repeat count: {error}"))?;
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

fn profile_self_pct(report: &str, needle: &str) -> f64 {
    report
        .lines()
        .filter(|line| line.contains(needle))
        .find_map(|line| {
            line.split_whitespace()
                .next()?
                .strip_suffix('%')?
                .parse()
                .ok()
        })
        .unwrap_or(0.0)
}

fn profile_zscan_trial(
    executable: &Path,
    arm: ZscanArm,
    trial: usize,
) -> Result<(f64, f64), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_zscan_match_{}_{}_{}_{}.data",
        process::id(),
        arm.name(),
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
            "--zscan-child",
            arm.name(),
            &ZSCAN_PROFILE_REPEATS.to_string(),
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
    println!("ZSCAN_PROFILE_TABLE_BEGIN arm={} trial={trial}", arm.name());
    for line in stdout.lines().filter(|line| {
        line.contains("Total Lost Samples")
            || line.contains("<fr_store::Store>::zscan")
            || line.contains("fr_store::glob_match")
    }) {
        println!("{line}");
    }
    println!("ZSCAN_PROFILE_TABLE_END arm={} trial={trial}", arm.name());
    Ok((
        profile_self_pct(stdout.as_ref(), arm.symbol()),
        profile_self_pct(stdout.as_ref(), "fr_store::glob_match"),
    ))
}

fn run_zscan_profile(executable: &Path) -> Result<(), String> {
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("ZSCAN_WORKER {hostname}");
    println!(
        "ZSCAN_BINARY_SHA256 both_arms={}",
        binary_sha256(executable)?
    );

    for arm in [ZscanArm::Reference, ZscanArm::Candidate] {
        let warm = Command::new(executable)
            .args(["--zscan-child", arm.name(), "10"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }

        let mut wrapper_samples = Vec::with_capacity(ZSCAN_PROFILE_TRIALS);
        let mut glob_samples = Vec::with_capacity(ZSCAN_PROFILE_TRIALS);
        for trial in 1..=ZSCAN_PROFILE_TRIALS {
            let (wrapper, glob) = profile_zscan_trial(executable, arm, trial)?;
            println!(
                "ZSCAN_PROFILE_SELF arm={} trial={trial} wrapper_self_pct={wrapper:.4} glob_match_self_pct={glob:.4}",
                arm.name()
            );
            wrapper_samples.push(wrapper);
            glob_samples.push(glob);
        }
        let wrapper_median = median(&mut wrapper_samples);
        let glob_median = median(&mut glob_samples);
        println!(
            "ZSCAN_PROFILE_SUMMARY arm={} trials={ZSCAN_PROFILE_TRIALS} wrapper_median_self_pct={wrapper_median:.4} wrapper_samples={wrapper_samples:?} glob_match_median_self_pct={glob_median:.4} glob_match_samples={glob_samples:?} report_floor_pct=0.1000",
            arm.name()
        );
        if wrapper_median <= 0.1 {
            return Err(format!(
                "{} wrapper median self-time {wrapper_median:.4}% did not clear the 0.1% execution floor",
                arm.name()
            ));
        }
        if matches!(arm, ZscanArm::Reference) && glob_median <= 0.1 {
            return Err(format!(
                "reference glob_match median self-time {glob_median:.4}% did not clear the 0.1% execution floor"
            ));
        }
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: ZscanArm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--zscan-child", arm.name(), &ZSCAN_STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat for {}: {error}", arm.name()))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat for {} failed: {stderr}", arm.name()));
    }
    for line in stderr.lines() {
        let columns = line.split(',').collect::<Vec<_>>();
        if columns
            .iter()
            .any(|field| field.trim().contains("instructions"))
        {
            let raw = columns[0].trim();
            if raw.starts_with('<') {
                return Err(format!("perf counter unavailable: {line}"));
            }
            return raw
                .parse()
                .map_err(|error| format!("invalid perf count {raw:?}: {error}"));
        }
    }
    Err(format!("instructions:u missing from perf output: {stderr}"))
}

fn zscan_correctness_gate() {
    let mut candidate = seed_zscan_store();
    let mut reference = seed_zscan_store();
    let got = candidate
        .zscan(b"z", 0, Some(b"hit:*"), ZSCAN_MEMBERS, 2)
        .expect("candidate ZSCAN");
    let expected = reference
        .zscan_classify_per_member_reference(b"z", 0, Some(b"hit:*"), ZSCAN_MEMBERS, 2)
        .expect("reference ZSCAN");
    let score_bits = |result: (u64, Vec<(Vec<u8>, f64)>)| {
        (
            result.0,
            result
                .1
                .into_iter()
                .map(|(member, score)| (member, score.to_bits()))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(score_bits(got), score_bits(expected));
    println!(
        "ZSCAN_CORRECTNESS_GATE full_members={ZSCAN_MEMBERS} pattern=hit:* cursor_member_order_score_bits=identical"
    );
}

fn run_zscan_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut null_ratios = Vec::with_capacity(ZSCAN_STAT_ROUNDS);
    let mut speedups = Vec::with_capacity(ZSCAN_STAT_ROUNDS);
    for round in 0..ZSCAN_STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                ZscanArm::Reference
            } else {
                ZscanArm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null_ratio = counts[1] as f64 / counts[0] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "ZSCAN_INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} reference={} candidate_b_over_a={null_ratio:.9} reference_over_candidate={speedup:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        null_ratios.push(null_ratio);
        speedups.push(speedup);
    }

    let null_cv_pct = cv(&null_ratios);
    let speedup_cv_pct = cv(&speedups);
    let null_median = median(&mut null_ratios);
    let speedup_median = median(&mut speedups);
    let null_p05 = pct(&null_ratios, NULL_LO);
    let null_p95 = pct(&null_ratios, NULL_HI);
    println!(
        "ZSCAN_INSTRUCTIONS_SUMMARY rounds={ZSCAN_STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} reference_over_candidate_median={speedup_median:.9} candidate_over_reference_median={:.9} speedup_cv_pct={speedup_cv_pct:.6}",
        1.0 / speedup_median
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if speedup_median <= null_p95 {
        return Err(format!(
            "candidate median does not clear null spread: speedup={speedup_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    if speedup_median <= 1.01 {
        return Err(format!(
            "1% instruction keep gate failed: {speedup_median:.9}x"
        ));
    }
    Ok(())
}

fn keys() -> Vec<Vec<u8>> {
    (0..NKEYS)
        .map(|i| format!("key:{i:08}:tag").into_bytes())
        .collect()
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    r[r.len() / 2]
}
fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}
fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    match zscan_child() {
        Ok(Some((arm, repeats))) => {
            run_zscan_arm(arm, repeats);
            return;
        }
        Ok(None) => {}
        Err(error) => panic!("invalid ZSCAN child arguments: {error}"),
    }
    let executable = env::current_exe().expect("current bench executable path");
    zscan_correctness_gate();
    run_zscan_profile(&executable).unwrap_or_else(|error| panic!("ZSCAN PROFILE INVALID: {error}"));
    run_zscan_instruction_ab(&executable)
        .unwrap_or_else(|error| panic!("ZSCAN A/B INVALID: {error}"));

    let ks = keys();
    // A prefix pattern matching ~10 of 20k keys — the dominant SCAN-MATCH shape (namespace scan).
    let patterns: &[(&str, &[u8])] = &[
        ("prefix", b"key:0001*"),
        ("suffix", b"*:tag"),
        ("general", b"key:*5:tag"),
    ];

    for (label, pat) in patterns {
        // Correctness gate.
        let prepared = glob_prepare(pat);
        let orig_hits: usize = ks.iter().filter(|k| glob_match(pat, k)).count();
        let cand_hits: usize = ks.iter().filter(|k| prepared.matches(k)).count();
        assert_eq!(orig_hits, cand_hits, "{label}: hit count diverged");

        let per_call = |ks: &[Vec<u8>]| ks.iter().filter(|k| glob_match(black_box(pat), k)).count();
        let prep = |ks: &[Vec<u8>]| {
            let m = glob_prepare(black_box(pat));
            ks.iter().filter(|k| m.matches(k)).count()
        };
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(&ks)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&per_call, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
                reps =
                    ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> usize, cf: &dyn Fn(&[Vec<u8>]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(&per_call, &per_call);
            let sp = pair(&per_call, &prep);
            if round == 0 {
                continue;
            }
            nulls.push(nn);
            speeds.push(sp);
        }

        let null_med = median(&mut nulls);
        let speedup = median(&mut speeds);
        let lo = pct(&nulls, NULL_LO);
        let hi = pct(&nulls, NULL_HI);
        let verdict = if speedup > 1.0 && speedup > hi {
            "WIN"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<9} reps={:<6} NULL {:.4} [{:.3},{:.3}] cv {:.2}%  speedup {:.3}x  {}",
            label,
            reps,
            null_med,
            lo,
            hi,
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
