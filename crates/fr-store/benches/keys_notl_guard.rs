//! Same-binary A/B for `keys_in_db`'s no-TTL guard. Pre-guard: build the physical-key vector,
//! reap every key, then build it AGAIN for the result — two O(N) `Vec<Vec<u8>>` builds + N
//! `drop_if_expired` probes. Guarded (`expires_count == 0`): the first build + reap loop are
//! dead (no key can evict), so only the result build runs. Byte-identical (gated by the
//! `keys`/`scan` lib tests); the delta is one eliminated O(N) key-vector build + N lookups.
//!
//! ORIG = `keys_in_db_unguarded_ref` (GUARD=false).  CAND = `keys_in_db` (GUARD=true, shipped).
//! KEYS is NON-destructive, so one store is built ONCE and probed repeatedly — a clean, low-noise
//! measurement (unlike the destructive DEL A/B). No key carries a TTL (`expires_count == 0`).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.05;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

fn build(n: usize) -> Store {
    let mut s = Store::new();
    for i in 0..n {
        s.set(format!("key:{i:07}").into_bytes(), b"v".to_vec(), None, 1);
    }
    s
}

#[inline(never)]
fn run_guarded(s: &mut Store) -> usize {
    s.keys_in_db(0, 2).len()
}
#[inline(never)]
fn run_unguarded(s: &mut Store) -> usize {
    s.keys_in_db_unguarded_ref(0, 2).len()
}

fn time(reps: usize, s: &mut Store, f: fn(&mut Store) -> usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(s)));
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
    let mut s = build(n);

    let mut reps = 1usize;
    loop {
        let e = time(reps, &mut s, run_unguarded);
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
        let mut pair = |bf: fn(&mut Store) -> usize, cf: fn(&mut Store) -> usize| {
            if swap {
                let c = time(reps, &mut s, cf);
                time(reps, &mut s, bf) / c
            } else {
                let b = time(reps, &mut s, bf);
                b / time(reps, &mut s, cf)
            }
        };
        let nn = pair(run_unguarded, run_unguarded);
        let sp = pair(run_unguarded, run_guarded);
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
        "WIN(guard)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{:<10} {:>7} {:>9.4} {:>16} {:>8.2} {:>10.3}x {:>16}",
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
        "\n{:<10} {:>7} {:>9} {:>16} {:>8} {:>11} {:>16}",
        "n_keys", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("n256", 256);
    bench("n2000", 2000);
    bench("n10000", 10000);
}
