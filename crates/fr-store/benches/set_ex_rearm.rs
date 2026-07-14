//! Same-binary A/B for `SET key value EX ttl` RE-ARMING a key that already has a live TTL (the
//! session-refresh / rate-limit pattern): the pre-elision insert that always clones the owned key
//! for `expiry_deadlines.insert` (`set_orig`, GATE=false) vs the guarded insert that elides that
//! clone — and the `get_key_value` lookup that fed it — because the key is already in the deadline
//! map, so the re-arm updates the deadline IN PLACE (`set`, GATE=true). Null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread.
//!
//! Re-arming the SAME key to the SAME far-future deadline is idempotent, so each store stays a
//! single stable TTL'd entry across all reps. NOTE: `set` takes OWNED `Vec<u8>` args, so both arms
//! pay two per-call `to_vec` allocations the live BORROWED path does not — this DILUTES the ratio,
//! a conservative lower bound. Byte-identical effect (asserted by `set_gated_expiry_key_matches_orig`,
//! which includes the `rearm` case).

use std::env;
use std::hint::black_box;
use std::path::Path;
use std::process::{self, Command};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fr_store::{MaxmemoryPolicy, Store};

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.03;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"session:user:0000000042";
const VAL: &[u8] = b"the-current-session-value-payload";
const DEADLINE: u64 = 1_000_000_000_000; // far-future absolute expires_at_ms (never fires)
const KEEPTTL_PROFILE_REPEATS: usize = 2_000_000;
const KEEPTTL_STAT_REPEATS: usize = 500_000;
const KEEPTTL_STAT_ROUNDS: usize = 9;

#[derive(Clone, Copy)]
enum KeepTtlArm {
    OwnedRoundtrip,
    BorrowedFused,
}

impl KeepTtlArm {
    fn name(self) -> &'static str {
        match self {
            Self::OwnedRoundtrip => "owned-roundtrip",
            Self::BorrowedFused => "borrowed-fused",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "owned-roundtrip" => Some(Self::OwnedRoundtrip),
            "borrowed-fused" => Some(Self::BorrowedFused),
            _ => None,
        }
    }
}

fn build_store() -> Store {
    let mut s = Store::new();
    // Seed WITH a TTL so every timed call is a re-arm (old_expiry.is_some()).
    s.set(KEY.to_vec(), VAL.to_vec(), Some(DEADLINE), 2_000);
    s
}

fn set_orig(s: &mut Store) {
    s.set_orig(
        black_box(KEY).to_vec(),
        black_box(VAL).to_vec(),
        Some(DEADLINE),
        2_000,
    );
}
fn set_new(s: &mut Store) {
    s.set(
        black_box(KEY).to_vec(),
        black_box(VAL).to_vec(),
        Some(DEADLINE),
        2_000,
    );
}

fn timed(f: fn(&mut Store), s: &mut Store, reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(s);
    }
    black_box(&*s);
    start.elapsed().as_secs_f64()
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

fn build_keepttl_store() -> Store {
    let mut store = Store::new();
    store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
    store.lfu_decay_time = 0;
    store.set(KEY.to_vec(), VAL.to_vec(), Some(DEADLINE), 2_000);
    store
}

fn apply_keepttl(store: &mut Store, arm: KeepTtlArm) {
    match arm {
        KeepTtlArm::OwnedRoundtrip => {
            store.set_keep_ttl_owned_roundtrip_bench(
                black_box(KEY),
                black_box(VAL),
                black_box(2_000),
            );
        }
        KeepTtlArm::BorrowedFused => {
            store.set_keep_ttl_borrowed(black_box(KEY), black_box(VAL), black_box(2_000));
        }
    }
}

fn run_keepttl_loop(arm: KeepTtlArm, repeats: usize) {
    let mut store = build_keepttl_store();
    for _ in 0..repeats {
        apply_keepttl(black_box(&mut store), arm);
    }
    black_box((store.dirty, store));
}

fn keepttl_correctness_gate() {
    let mut control = build_keepttl_store();
    let mut fused = build_keepttl_store();
    apply_keepttl(&mut control, KeepTtlArm::OwnedRoundtrip);
    apply_keepttl(&mut fused, KeepTtlArm::BorrowedFused);
    assert_eq!(fused.get(KEY, 2_001), control.get(KEY, 2_001));
    assert_eq!(fused.pttl(KEY, 2_001), control.pttl(KEY, 2_001));
    assert_eq!(
        fused.object_encoding(KEY, 2_001),
        control.object_encoding(KEY, 2_001)
    );
    assert_eq!(
        fused.object_freq(KEY, 2_001),
        control.object_freq(KEY, 2_001)
    );
    assert_eq!(
        fused.memory_usage_for_key(KEY, 2_001),
        control.memory_usage_for_key(KEY, 2_001)
    );
    assert_eq!(fused.state_digest(), control.state_digest());
    assert_eq!(fused.dirty, control.dirty);
    println!("SET_KEEPTTL_CORRECTNESS value_ttl_encoding_lfu_memory_digest_dirty=identical");
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

fn keepttl_profile(executable: &Path, arm: KeepTtlArm) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_set_keepttl_{}_{}_{}.data",
        process::id(),
        arm.name(),
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
            "--keepttl-child",
            arm.name(),
            &KEEPTTL_PROFILE_REPEATS.to_string(),
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
            "0.05",
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
    println!(
        "SET_KEEPTTL_PROFILE_BEGIN arm={}\n{stdout}\nSET_KEEPTTL_PROFILE_END arm={}",
        arm.name(),
        arm.name()
    );
    let lost = stdout
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .ok_or("perf report omitted Total Lost Samples")?
        .rsplit(':')
        .next()
        .ok_or("missing lost-sample count")?
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid lost-sample count: {error}"))?;
    if lost != 0 {
        return Err(format!("profile lost {lost} samples"));
    }
    let symbol = match arm {
        KeepTtlArm::OwnedRoundtrip => "set_keep_ttl_owned_roundtrip_bench",
        KeepTtlArm::BorrowedFused => "set_keep_ttl_borrowed",
    };
    let self_pct = stdout
        .lines()
        .find(|line| {
            line.contains(symbol)
                && line
                    .trim_start()
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_digit)
        })
        .ok_or_else(|| format!("profile did not execute {symbol} with non-zero self-time"))?
        .split_whitespace()
        .next()
        .ok_or("missing profile self percentage")?
        .trim_end_matches('%')
        .parse::<f64>()
        .map_err(|error| format!("invalid profile self percentage: {error}"))?;
    if self_pct <= 0.0 {
        return Err(format!("{symbol} self-time was zero"));
    }
    Ok(self_pct)
}

fn keepttl_instructions(executable: &Path, arm: KeepTtlArm, repeats: usize) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--keepttl-child", arm.name(), &repeats.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat failed: {stderr}"));
    }
    stderr
        .lines()
        .find_map(|line| {
            let columns: Vec<_> = line.split(',').collect();
            columns
                .iter()
                .any(|field| field.trim().contains("instructions"))
                .then(|| columns[0].trim().parse::<u64>())
        })
        .transpose()
        .map_err(|error| format!("invalid perf instruction count: {error}"))?
        .ok_or_else(|| format!("instructions:u missing from perf output: {stderr}"))
}

fn keepttl_gate(executable: &Path) -> Result<(), String> {
    keepttl_correctness_gate();
    let hostname = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("WORKER_ID {hostname}");
    println!("BINARY_SHA256 same_binary={}", binary_sha256(executable)?);

    for arm in [KeepTtlArm::OwnedRoundtrip, KeepTtlArm::BorrowedFused] {
        let warm = Command::new(executable)
            .args(["--keepttl-child", arm.name(), "10000"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed", arm.name()));
        }
        let self_pct = keepttl_profile(executable, arm)?;
        println!(
            "SET_KEEPTTL_PROFILE_SELF arm={} operation_self_pct={self_pct:.4}",
            arm.name()
        );
    }

    let mut nulls = Vec::with_capacity(KEEPTTL_STAT_ROUNDS);
    let mut effects = Vec::with_capacity(KEEPTTL_STAT_ROUNDS);
    for round in 0..KEEPTTL_STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                KeepTtlArm::OwnedRoundtrip
            } else {
                KeepTtlArm::BorrowedFused
            };
            counts[slot] = keepttl_instructions(executable, arm, KEEPTTL_STAT_REPEATS)?;
        }
        let null = counts[0] as f64 / counts[1] as f64;
        let effect = counts[2] as f64 / counts[0] as f64;
        println!(
            "SET_KEEPTTL_INSTRUCTIONS round={} order={order:?} fused_a={} fused_b={} owned={} null={null:.9} owned_over_fused={effect:.9}",
            round + 1,
            counts[0],
            counts[1],
            counts[2]
        );
        nulls.push(null);
        effects.push(effect);
    }
    let null_cv = cv(&nulls);
    let effect_cv = cv(&effects);
    let null_median = median(&mut nulls);
    let effect_median = median(&mut effects);
    let null_p05 = pct(&nulls, 0.05);
    let null_p95 = pct(&nulls, 0.95);
    println!(
        "SET_KEEPTTL_INSTRUCTIONS_SUMMARY rounds={KEEPTTL_STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv:.6} owned_over_fused_median={effect_median:.9} effect_cv_pct={effect_cv:.6}"
    );
    if (null_median - 1.0).abs() >= 0.02 {
        return Err(format!(
            "null median exposes harness bias: {null_median:.9}"
        ));
    }
    if effect_median <= null_p95 || effect_median <= 1.01 {
        return Err(format!(
            "fused path does not clear the null/1% gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
        ));
    }
    Ok(())
}

fn dispatch_keepttl(args: &[String]) -> bool {
    match args.get(1).map(String::as_str) {
        Some("--keepttl-child") => {
            let arm = KeepTtlArm::parse(args.get(2).expect("missing KEEPTTL child arm"))
                .expect("invalid KEEPTTL child arm");
            let repeats = args
                .get(3)
                .expect("missing KEEPTTL repeat count")
                .parse()
                .expect("invalid KEEPTTL repeat count");
            run_keepttl_loop(arm, repeats);
            true
        }
        Some("--keepttl-borrowed") => {
            let executable = env::current_exe().expect("current bench executable path");
            keepttl_gate(&executable)
                .unwrap_or_else(|error| panic!("SET KEEPTTL A/B INVALID: {error}"));
            true
        }
        _ => false,
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if dispatch_keepttl(&args) {
        return;
    }
    // (cc_fr) Noise-immune instruction-count mode for `perf stat -e instructions:u`: with the
    // machine under concurrent load the wallclock null band is too wide to gate a ~1.1x alloc
    // elision, but the eliminated key clone + get_key_value probe is a deterministic instruction
    // delta. `FR_PERF_MODE=orig|new ./set_ex_rearm` runs a fixed 20M re-arms of ONE variant.
    if let Ok(mode) = std::env::var("FR_PERF_MODE") {
        let mut s = build_store();
        let f: fn(&mut Store) = if mode == "new" { set_new } else { set_orig };
        for _ in 0..20_000_000u64 {
            f(black_box(&mut s));
        }
        black_box(&s);
        return;
    }

    let mut store_o = build_store();
    let mut store_n = build_store();

    let mut reps = 1usize;
    loop {
        let e = timed(set_orig, &mut store_o, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 20 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let nn = if swap {
            let c = timed(set_orig, &mut store_o, reps);
            timed(set_orig, &mut store_o, reps) / c
        } else {
            let b = timed(set_orig, &mut store_o, reps);
            b / timed(set_orig, &mut store_o, reps)
        };
        let sp = if swap {
            let c = timed(set_new, &mut store_n, reps);
            timed(set_orig, &mut store_o, reps) / c
        } else {
            let b = timed(set_orig, &mut store_o, reps);
            b / timed(set_new, &mut store_n, reps)
        };
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
        "\n{:<22} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<22} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "set_ex_rearm",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
