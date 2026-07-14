//! Profile-first harness for MOVE's heap-string transfer.
//!
//! The fallback is the exact shipped `copy_no_stat` + `del` sequence. The
//! candidate consumes/re-keys only a heap-backed string. Both live in one
//! binary, profiles prove reachability, and an A/A null control licenses the
//! balanced interleaved instruction-count median.

use std::{
    env,
    hint::black_box,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use fr_store::Store;

const VALUE_BYTES: usize = 64 * 1024;
const PROFILE_REPEATS: usize = 500_000;
const PROFILE_TRIALS: usize = 3;
const STAT_REPEATS: usize = 20_000;
const STAT_ROUNDS: usize = 24;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY_A: &[u8] = b"move:key";
const KEY_B: &[u8] = b"\0frdb\0\x00\x00\x00\x00\x00\x00\x00\x01move:key";

#[derive(Clone, Copy, Debug)]
enum Arm {
    Candidate,
    Fallback,
}

impl Arm {
    const fn name(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Fallback => "fallback",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "fallback" => Ok(Self::Fallback),
            _ => Err(format!("unknown child arm {value:?}")),
        }
    }
}

fn seed_store() -> Store {
    let mut store = Store::new();
    let value: Vec<u8> = (0..VALUE_BYTES)
        .map(|index| b'a' + (index % 23) as u8)
        .collect();
    store.set(KEY_A.to_vec(), value, None, 1_000);
    store
}

fn fallback_move(store: &mut Store, source: &[u8], destination: &[u8], now_ms: u64) {
    assert!(store.exists_no_stat(black_box(source), now_ms));
    assert!(!store.exists_no_stat(black_box(destination), now_ms));
    let dirty_before = store.dirty;
    assert!(
        store
            .copy_no_stat(black_box(source), black_box(destination), false, now_ms,)
            .expect("fallback copy")
    );
    assert_eq!(store.del(&[source.to_vec()], now_ms), 1);
    store.dirty = dirty_before.saturating_add(1);
}

fn candidate_move(store: &mut Store, source: &[u8], destination: &[u8], now_ms: u64) {
    assert!(store.exists_no_stat(black_box(source), now_ms));
    assert!(!store.exists_no_stat(black_box(destination), now_ms));
    assert!(
        store
            .move_existing_no_stat(black_box(source), black_box(destination), now_ms)
            .expect("candidate move")
    );
}

fn apply_move(store: &mut Store, arm: Arm, source: &[u8], destination: &[u8]) {
    match arm {
        Arm::Candidate => candidate_move(store, source, destination, 2_000),
        Arm::Fallback => fallback_move(store, source, destination, 2_000),
    }
}

fn run_loop(arm: Arm, repeats: usize) {
    let mut store = seed_store();
    for iteration in 0..repeats {
        let (source, destination) = if iteration & 1 == 0 {
            (KEY_A, KEY_B)
        } else {
            (KEY_B, KEY_A)
        };
        apply_move(&mut store, arm, source, destination);
    }
    black_box(store.key_is_present(if repeats & 1 == 0 { KEY_A } else { KEY_B }));
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

fn profile_trial(executable: &Path, arm: Arm, trial: usize) -> Result<f64, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("invalid system time: {error}"))?
        .as_nanos();
    let data = env::temp_dir().join(format!(
        "fr_move_key_relink_{}_{}_{}_{}.data",
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
        .args(["--child", arm.name(), &PROFILE_REPEATS.to_string()])
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
    println!(
        "PROFILE_TABLE_BEGIN arm={} trial={trial}\n{stdout}\nPROFILE_TABLE_END arm={} trial={trial}",
        arm.name(),
        arm.name()
    );
    let line = stdout.lines().find(|line| {
        line.contains("<fr_store::Entry>::duplicate_for_copy")
            && line
                .trim_start()
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
    });
    let self_pct = line
        .map(|line| {
            line.split_whitespace()
                .next()
                .ok_or("missing duplicate_for_copy self percentage")?
                .trim_end_matches('%')
                .parse::<f64>()
                .map_err(|error| format!("invalid duplicate_for_copy self percentage: {error}"))
        })
        .transpose()?
        .unwrap_or(0.0);
    Ok(self_pct)
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|left, right| {
        left.partial_cmp(right)
            .expect("profile self-time is not NaN")
    });
    samples[samples.len() / 2]
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
    println!("BINARY_SHA256 same_binary={}", binary_sha256(executable)?);
    for arm in [Arm::Fallback, Arm::Candidate] {
        let warm = Command::new(executable)
            .args(["--child", arm.name(), "100"])
            .status()
            .map_err(|error| format!("could not launch {} warm-up: {error}", arm.name()))?;
        if !warm.success() {
            return Err(format!("{} warm-up failed with {warm}", arm.name()));
        }
    }

    let mut fallback_samples = Vec::with_capacity(PROFILE_TRIALS);
    let mut candidate_samples = Vec::with_capacity(PROFILE_TRIALS);
    for arm in [Arm::Fallback, Arm::Candidate] {
        for trial in 1..=PROFILE_TRIALS {
            let self_pct = profile_trial(executable, arm, trial)?;
            println!(
                "PROFILE_SELF arm={} trial={trial} duplicate_for_copy_self_pct={self_pct:.4}",
                arm.name()
            );
            match arm {
                Arm::Fallback => fallback_samples.push(self_pct),
                Arm::Candidate => candidate_samples.push(self_pct),
            }
        }
    }
    let median_self_pct = median(&mut fallback_samples);
    let candidate_median_self_pct = median(&mut candidate_samples);
    println!(
        "PROFILE_SELF_SUMMARY trials={PROFILE_TRIALS} fallback_median_duplicate_for_copy_self_pct={median_self_pct:.4} fallback_samples={fallback_samples:?} candidate_median_duplicate_for_copy_self_pct={candidate_median_self_pct:.4} candidate_samples={candidate_samples:?} report_floor_pct=0.1000"
    );
    if median_self_pct <= 0.1 {
        return Err(format!(
            "median duplicate_for_copy self-time {median_self_pct:.4}% does not clear the 0.1% attribution floor"
        ));
    }
    if candidate_median_self_pct > 0.0 {
        return Err(format!(
            "candidate still executes duplicate_for_copy at {candidate_median_self_pct:.4}% median self-time"
        ));
    }
    Ok(())
}

fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
    let output = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
        .arg(executable)
        .args(["--child", arm.name(), &STAT_REPEATS.to_string()])
        .output()
        .map_err(|error| format!("could not launch perf stat for {}: {error}", arm.name()))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("perf stat for {} failed: {stderr}", arm.name()));
    }
    for line in stderr.lines() {
        let columns: Vec<_> = line.split(',').collect();
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

fn cv(samples: &[f64]) -> f64 {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    100.0 * variance.sqrt() / mean
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn correctness_gate() {
    let mut candidate = seed_store();
    let mut fallback = seed_store();
    // The unit test proves allocation identity directly inside fr-store. This
    // external gate proves every public reply/state observation is identical.
    candidate_move(&mut candidate, KEY_A, KEY_B, 2_000);
    fallback_move(&mut fallback, KEY_A, KEY_B, 2_000);
    assert_eq!(candidate.get(KEY_A, 2_000), fallback.get(KEY_A, 2_000));
    assert_eq!(candidate.get(KEY_B, 2_000), fallback.get(KEY_B, 2_000));
    assert_eq!(candidate.pttl(KEY_B, 2_000), fallback.pttl(KEY_B, 2_000));
    assert_eq!(
        candidate.object_encoding(KEY_B, 2_000),
        fallback.object_encoding(KEY_B, 2_000)
    );
    assert_eq!(candidate.state_digest(), fallback.state_digest());
    assert_eq!(candidate.dirty, fallback.dirty);
    println!(
        "CORRECTNESS_GATE move_reply_value_ttl_encoding_digest_dirty=identical value_bytes={VALUE_BYTES}"
    );
}

fn run_instruction_ab(executable: &Path) -> Result<(), String> {
    let mut null_ratios = Vec::with_capacity(STAT_ROUNDS);
    let mut speedups = Vec::with_capacity(STAT_ROUNDS);
    for round in 0..STAT_ROUNDS {
        let mut counts = [0_u64; 3];
        let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
        if round % 2 == 1 {
            order.reverse();
        }
        for slot in order {
            let arm = if slot == 2 {
                Arm::Fallback
            } else {
                Arm::Candidate
            };
            counts[slot] = perf_instructions(executable, arm)?;
        }
        let null_ratio = counts[0] as f64 / counts[1] as f64;
        let speedup = counts[2] as f64 / counts[0] as f64;
        println!(
            "INSTRUCTIONS round={} order={order:?} candidate_a={} candidate_b={} fallback={} null_ratio={null_ratio:.9} fallback_over_candidate={speedup:.9}",
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
    let null_p05 = percentile(&null_ratios, NULL_LO);
    let null_p95 = percentile(&null_ratios, NULL_HI);
    println!(
        "INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv_pct:.6} fallback_over_candidate_median={speedup_median:.9} speedup_cv_pct={speedup_cv_pct:.6}"
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

fn main() {
    let args: Vec<String> = env::args().collect();
    if rpoplpush_reply::dispatch(&args) {
        return;
    }
    if args.get(1).map(String::as_str) == Some("--child") {
        let arm = Arm::parse(args.get(2).expect("missing child arm")).expect("invalid child arm");
        let repeats = args
            .get(3)
            .expect("missing child repeat count")
            .parse()
            .expect("invalid child repeat count");
        run_loop(arm, repeats);
        return;
    }
    let executable = env::current_exe().expect("current bench executable path");
    correctness_gate();
    run_profile(&executable).unwrap_or_else(|error| panic!("PROFILE INVALID: {error}"));
    run_instruction_ab(&executable).unwrap_or_else(|error| panic!("A/B INVALID: {error}"));
}

/// Profile-first RPOPLPUSH reply-sink mode. Kept in this already-warm clone-vs-move bench binary
/// so the one-turn gate does not introduce a fresh release-profile LTO target.
mod rpoplpush_reply {
    use std::{
        env,
        hint::black_box,
        path::Path,
        process::{self, Command},
        time::{SystemTime, UNIX_EPOCH},
    };

    use fr_protocol::encode_bulk_string_slice;
    use fr_store::Store;

    const KEY: &[u8] = b"rpoplpush:reply";
    const VALUE_BYTES: usize = 4 * 1024;
    const LIST_LEN: usize = 16;
    const PROFILE_REPEATS: usize = 250_000;
    const STAT_REPEATS: usize = 50_000;
    const STAT_ROUNDS: usize = 12;

    #[derive(Clone, Copy)]
    enum Arm {
        Direct,
        Frame,
    }

    impl Arm {
        const fn name(self) -> &'static str {
            match self {
                Self::Direct => "direct",
                Self::Frame => "frame",
            }
        }

        fn parse(value: &str) -> Result<Self, String> {
            match value {
                "direct" => Ok(Self::Direct),
                "frame" => Ok(Self::Frame),
                _ => Err(format!("unknown RPOPLPUSH arm {value:?}")),
            }
        }
    }

    fn seed_store() -> Store {
        let mut store = Store::new();
        let values: Vec<Vec<u8>> = (0..LIST_LEN)
            .map(|index| {
                let mut value = vec![b'a' + (index % 23) as u8; VALUE_BYTES];
                value[..8].copy_from_slice(&(index as u64).to_le_bytes());
                value
            })
            .collect();
        store.rpush(KEY, &values, 1).expect("seed list");
        store
    }

    #[inline(never)]
    fn apply(store: &mut Store, arm: Arm, out: &mut Vec<u8>) {
        out.clear();
        match arm {
            Arm::Direct => {
                let moved = store
                    .rpoplpush_with(black_box(KEY), black_box(KEY), 2, |value| {
                        encode_bulk_string_slice(Some(black_box(value)), false, out);
                    })
                    .expect("direct RPOPLPUSH");
                assert!(moved);
            }
            Arm::Frame => {
                let value = store
                    .rpoplpush(black_box(KEY), black_box(KEY), 2)
                    .expect("frame RPOPLPUSH");
                encode_bulk_string_slice(value.as_deref(), false, out);
                black_box(value);
            }
        }
        black_box(out.as_slice());
    }

    fn run_loop(arm: Arm, repeats: usize) {
        let mut store = seed_store();
        let mut out = Vec::with_capacity(VALUE_BYTES + 32);
        let mut checksum = 0usize;
        for _ in 0..repeats {
            apply(&mut store, arm, &mut out);
            checksum = checksum.wrapping_add(out.len());
        }
        black_box((store, checksum));
    }

    fn correctness_gate() {
        let mut direct = seed_store();
        let mut frame = seed_store();
        let (mut direct_out, mut frame_out) = (Vec::new(), Vec::new());
        apply(&mut direct, Arm::Direct, &mut direct_out);
        apply(&mut frame, Arm::Frame, &mut frame_out);
        assert_eq!(direct_out, frame_out);
        assert_eq!(direct.lrange(KEY, 0, -1, 2), frame.lrange(KEY, 0, -1, 2));
        assert_eq!(
            direct.object_encoding(KEY, 2),
            frame.object_encoding(KEY, 2)
        );
        assert_eq!(direct.state_digest(), frame.state_digest());
        assert_eq!(direct.dirty, frame.dirty);
        println!(
            "RPOPLPUSH_CORRECTNESS reply_list_encoding_digest_dirty=identical value_bytes={VALUE_BYTES} list_len={LIST_LEN}"
        );
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

    fn profile_trial(executable: &Path, arm: Arm) -> Result<f64, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("invalid system time: {error}"))?
            .as_nanos();
        let data = env::temp_dir().join(format!(
            "fr_rpoplpush_reply_{}_{}_{}.data",
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
                "--rpoplpush-child",
                arm.name(),
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
            "RPOPLPUSH_PROFILE_BEGIN arm={}\n{stdout}\nRPOPLPUSH_PROFILE_END arm={}",
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
        let self_pct = stdout
            .lines()
            .find(|line| {
                line.contains("rpoplpush_reply::apply")
                    && line
                        .trim_start()
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_digit)
            })
            .ok_or("profile did not execute rpoplpush_reply::apply with non-zero self-time")?
            .split_whitespace()
            .next()
            .ok_or("missing apply self percentage")?
            .trim_end_matches('%')
            .parse::<f64>()
            .map_err(|error| format!("invalid apply self percentage: {error}"))?;
        if self_pct <= 0.0 {
            return Err("apply self-time was zero".to_owned());
        }
        Ok(self_pct)
    }

    fn perf_instructions(executable: &Path, arm: Arm) -> Result<u64, String> {
        let output = Command::new("perf")
            .env("LC_ALL", "C")
            .args(["stat", "--no-big-num", "-x,", "-e", "instructions:u", "--"])
            .arg(executable)
            .args(["--rpoplpush-child", arm.name(), &STAT_REPEATS.to_string()])
            .output()
            .map_err(|error| format!("could not launch perf stat: {error}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(format!("perf stat failed: {stderr}"));
        }
        stderr
            .lines()
            .find(|line| line.contains("instructions"))
            .and_then(|line| line.split(',').next())
            .ok_or_else(|| format!("instructions:u missing from perf output: {stderr}"))?
            .trim()
            .parse()
            .map_err(|error| format!("invalid perf count: {error}"))
    }

    fn median(samples: &mut [f64]) -> f64 {
        samples.sort_by(|left, right| left.partial_cmp(right).expect("sample is not NaN"));
        samples[samples.len() / 2]
    }

    fn percentile(sorted: &[f64], percentile: f64) -> f64 {
        sorted[((sorted.len() - 1) as f64 * percentile).round() as usize]
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

    fn instruction_gate(executable: &Path) -> Result<(), String> {
        let (mut nulls, mut effects) = (
            Vec::with_capacity(STAT_ROUNDS),
            Vec::with_capacity(STAT_ROUNDS),
        );
        for round in 0..STAT_ROUNDS {
            let mut counts = [0_u64; 3];
            let mut order = [round % 3, (round + 1) % 3, (round + 2) % 3];
            if round % 2 == 1 {
                order.reverse();
            }
            for slot in order {
                let arm = if slot == 2 { Arm::Frame } else { Arm::Direct };
                counts[slot] = perf_instructions(executable, arm)?;
            }
            let null = counts[0] as f64 / counts[1] as f64;
            let effect = counts[2] as f64 / counts[0] as f64;
            println!(
                "RPOPLPUSH_INSTRUCTIONS round={} order={order:?} direct_a={} direct_b={} frame={} null={null:.9} frame_over_direct={effect:.9}",
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
        let null_p05 = percentile(&nulls, 0.05);
        let null_p95 = percentile(&nulls, 0.95);
        println!(
            "RPOPLPUSH_INSTRUCTIONS_SUMMARY rounds={STAT_ROUNDS} null_median={null_median:.9} null_p05={null_p05:.9} null_p95={null_p95:.9} null_cv_pct={null_cv:.6} frame_over_direct_median={effect_median:.9} effect_cv_pct={effect_cv:.6}"
        );
        if (null_median - 1.0).abs() >= 0.02 {
            return Err(format!(
                "null median exposes harness bias: {null_median:.9}"
            ));
        }
        if effect_median <= null_p95 || effect_median <= 1.01 {
            return Err(format!(
                "direct reply does not clear the null/1% gate: effect={effect_median:.9}, null_p95={null_p95:.9}"
            ));
        }
        Ok(())
    }

    pub fn dispatch(args: &[String]) -> bool {
        match args.get(1).map(String::as_str) {
            Some("--rpoplpush-child") => {
                let arm = Arm::parse(args.get(2).expect("missing RPOPLPUSH child arm"))
                    .expect("invalid RPOPLPUSH child arm");
                let repeats = args
                    .get(3)
                    .expect("missing RPOPLPUSH repeat count")
                    .parse()
                    .expect("invalid RPOPLPUSH repeat count");
                run_loop(arm, repeats);
                true
            }
            Some("--rpoplpush-reply") => {
                correctness_gate();
                let executable = env::current_exe().expect("current bench executable path");
                println!(
                    "WORKER_ID {}",
                    String::from_utf8_lossy(
                        &Command::new("hostname").output().expect("hostname").stdout
                    )
                    .trim()
                );
                println!(
                    "BINARY_SHA256 same_binary={}",
                    binary_sha256(&executable).expect("bench binary sha256")
                );
                for arm in [Arm::Frame, Arm::Direct] {
                    let warm = Command::new(&executable)
                        .args(["--rpoplpush-child", arm.name(), "1000"])
                        .status()
                        .expect("RPOPLPUSH profile warm-up");
                    assert!(warm.success(), "{} warm-up failed", arm.name());
                    let self_pct = profile_trial(&executable, arm)
                        .unwrap_or_else(|error| panic!("RPOPLPUSH PROFILE INVALID: {error}"));
                    println!(
                        "RPOPLPUSH_PROFILE_SELF arm={} apply_self_pct={self_pct:.4}",
                        arm.name()
                    );
                }
                instruction_gate(&executable)
                    .unwrap_or_else(|error| panic!("RPOPLPUSH A/B INVALID: {error}"));
                true
            }
            _ => false,
        }
    }
}
