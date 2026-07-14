//! Same-binary A/B for packed LPUSH prepend: production reserves/resizes the destination buffer,
//! shifts its live bytes once, and writes the varint+element directly; the reference retains the
//! former temporary encoded `Vec` plus `Vec::splice`. The complete Store LPUSH path is timed and
//! byte-identical order/content is asserted via LLEN+LRANGE before measurement.
//!
//! Substrate: ONE binary, adjacent-pair interleave, black_box, reps calibrated once, median of paired
//! ratios, null-gated (orig-vs-orig), cv reported never gated. Push-dominated (many keys x many
//! elements per fresh store) so Store::new/drop variance doesn't swamp the per-element signal. Small
//! elements (<=64B) + <128 entries/list keep every list on the Packed repr (where the win lives; the
//! large-list Deque repr moves the owned Vec, so it is unaffected either way).

use std::hint::black_box;
use std::process::Command;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.008;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const NOW: u64 = 1_000;
const KEYS: usize = 50; // lists per fresh store
const PER_LIST: usize = 100; // elements per list (< 128 ⇒ stays Packed)
const PROFILE_PASSES: usize = 256;

fn elems() -> Vec<Vec<u8>> {
    // 24-byte elements: small enough to keep the list on the Packed repr.
    (0..PER_LIST)
        .map(|i| format!("elem-{i:018}").into_bytes())
        .collect()
}

fn build_orig(items: &[Vec<u8>]) -> usize {
    let mut s = Store::new();
    let mut acc = 0usize;
    for k in 0..KEYS {
        let key = [b"l", k.to_le_bytes().as_slice()].concat();
        acc += s.lpush_splice_bench(&key, items, NOW).unwrap_or(0);
    }
    acc
}
#[inline(never)]
fn build_borrow(items: &[Vec<u8>]) -> usize {
    let mut s = Store::new();
    let mut acc = 0usize;
    for k in 0..KEYS {
        let key = [b"l", k.to_le_bytes().as_slice()].concat();
        acc += s.lpush(&key, items, NOW).unwrap_or(0);
    }
    acc
}

fn run_profile_if_requested() -> bool {
    if std::env::var_os("LPUSH_BORROW_PROFILE_CHILD").is_some() {
        let items = elems();
        let mut acc = 0_usize;
        for _ in 0..PROFILE_PASSES {
            acc = acc.wrapping_add(build_borrow(black_box(items.as_slice())));
        }
        black_box(acc);
        return true;
    }
    if std::env::var_os("LPUSH_BORROW_PROFILE").is_none() {
        return false;
    }

    let exe = std::env::current_exe().expect("current benchmark executable");
    let data = "/tmp/lpush_borrow.perf.data";
    let status = Command::new("perf")
        .args([
            "record", "-q", "-e", "cycles:u", "-F", "999", "-o", data, "--",
        ])
        .arg(exe)
        .env("LPUSH_BORROW_PROFILE_CHILD", "1")
        .status()
        .expect("run perf record");
    assert!(status.success(), "perf record failed: {status}");

    let report = Command::new("perf")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--sort=symbol",
            "--percent-limit=0.1",
            "-i",
            data,
        ])
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        report.status
    );
    print!("{}", String::from_utf8_lossy(&report.stdout));
    true
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

fn print_provenance() {
    let exe = std::env::current_exe().expect("current benchmark executable");
    let output = Command::new("sha256sum")
        .arg(&exe)
        .output()
        .expect("run sha256sum");
    assert!(output.status.success(), "sha256sum failed");
    print!("binary {}", String::from_utf8_lossy(&output.stdout));
}

fn main() {
    if run_profile_if_requested() {
        return;
    }
    let items = elems();

    // Byte-identical: borrowed and owned LPUSH build the same list (order + contents).
    {
        let mut a = Store::new();
        let mut b = Store::new();
        let _ = a.lpush_splice_bench(b"k", &items, NOW);
        let _ = b.lpush(b"k", &items, NOW);
        assert_eq!(
            a.llen(b"k", NOW).unwrap(),
            b.llen(b"k", NOW).unwrap(),
            "llen mismatch"
        );
        assert_eq!(
            a.lrange(b"k", 0, -1, NOW).unwrap(),
            b.lrange(b"k", 0, -1, NOW).unwrap(),
            "lrange contents mismatch"
        );
    }

    let time = |f: fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
        let start = Instant::now();
        let mut acc = 0usize;
        for _ in 0..reps {
            acc = acc.wrapping_add(f(black_box(items.as_slice())));
        }
        black_box(acc);
        start.elapsed().as_secs_f64()
    };

    let mut reps = 1usize;
    loop {
        let e = time(build_orig, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 2;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let pair = |bf: fn(&[Vec<u8>]) -> usize, cf: fn(&[Vec<u8>]) -> usize| {
            if swap {
                let c = time(cf, reps);
                time(bf, reps) / c
            } else {
                let b = time(bf, reps);
                b / time(cf, reps)
            }
        };
        let nn = pair(build_orig, build_orig);
        let sp = pair(build_orig, build_borrow);
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
        "WIN(direct)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    print_provenance();
    println!(
        "\n{:<12} {:>7} {:>9} {:>16} {:>8} {:>10} {:>13} {:>16}",
        "op",
        "reps",
        "NULL med",
        "null p5..p95",
        "null cv%",
        "effect cv%",
        "direct/splice",
        "verdict"
    );
    println!(
        "{:<12} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.2} {:>12.4}x {:>16}",
        "lpush_packed",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        cv(&speeds),
        speedup,
        verdict
    );
}
