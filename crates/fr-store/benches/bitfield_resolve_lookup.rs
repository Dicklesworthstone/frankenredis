//! Isolated same-binary A/B for the keyspace lookup BITFIELD's "resolve once" preamble elided:
//! the old `entries.get(key).is_some()` + `entries.get_mut(key)` (TWO foldhash lookups) vs the fused
//! single `entries.get_mut(key)` (ONE). The store's `entries` map uses foldhash, replicated here.
//! This measures ONLY the eliminated lookup, so the ratio is the resolve-lookup delta — the E2E
//! BITFIELD win is a much smaller (Pareto-safe) fraction, since the field ops + reply dominate.
//!
//! ORIG = get().is_some() + get_mut()  (2 lookups).   CAND = get_mut()  (1 lookup).

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use foldhash::quality::RandomState;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build(n: usize) -> (HashMap<Vec<u8>, u64, RandomState>, Vec<u8>) {
    let mut m: HashMap<Vec<u8>, u64, RandomState> = HashMap::with_hasher(RandomState::default());
    for i in 0..n {
        m.insert(format!("bitmap:key:{i:07}").into_bytes(), i as u64);
    }
    let target = format!("bitmap:key:{:07}", n / 2).into_bytes();
    (m, target)
}

type BenchMap = HashMap<Vec<u8>, u64, RandomState>;

#[inline(never)]
#[allow(clippy::collapsible_if)] // the double probe IS the reference arm under test
fn resolve_double(m: &mut BenchMap, k: &[u8]) -> u64 {
    let present = m.get(k).is_some();
    if present {
        if let Some(v) = m.get_mut(k) {
            *v = v.wrapping_add(1);
            return *v;
        }
    }
    0
}
#[inline(never)]
fn resolve_single(m: &mut BenchMap, k: &[u8]) -> u64 {
    match m.get_mut(k) {
        Some(v) => {
            *v = v.wrapping_add(1);
            *v
        }
        None => 0,
    }
}

fn time(reps: usize, m: &mut BenchMap, k: &[u8], f: fn(&mut BenchMap, &[u8]) -> u64) -> f64 {
    let start = Instant::now();
    let mut acc = 0u64;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(m, black_box(k)));
    }
    black_box(acc);
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

fn bench(label: &str, n: usize) {
    let (mut m, target) = build(n);

    let mut reps = 1usize;
    loop {
        let e = time(reps, &mut m, &target, resolve_double);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |bf: fn(&mut BenchMap, &[u8]) -> u64,
                        cf: fn(&mut BenchMap, &[u8]) -> u64| {
            if swap {
                let c = time(reps, &mut m, &target, cf);
                time(reps, &mut m, &target, bf) / c
            } else {
                let b = time(reps, &mut m, &target, bf);
                b / time(reps, &mut m, &target, cf)
            }
        };
        let nn = pair(resolve_double, resolve_double);
        let sp = pair(resolve_double, resolve_single);
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
        "WIN(1 lookup)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{:<12} {:>8} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
        label,
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}

fn main() {
    println!(
        "\n{:<12} {:>8} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "keyspace", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("1k", 1000);
    bench("100k", 100_000);
}
