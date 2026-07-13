//! Same-binary A/B for `sampled_eviction_candidate_keys` (maxmemory eviction victim sampling),
//! comparing the original vs two combined optimizations for allkeys-* policies (`volatile_only
//! == false`):
//!   (1) eligible count: the filter `!volatile_only || key_has_expiry(k)` is always true, so the
//!       count == whole keyspace — O(1) `HashMap::len()` instead of an O(n) `keys().count()` walk.
//!   (2) select pass: the prior code did a `HashSet::contains(&eligible_idx)` probe for EVERY
//!       entry (O(n) hash lookups to place ≤sample_limit samples). Replaced by drawing the same
//!       distinct indices, sorting them (≤10 elems), and merge-walking `entries.iter().enumerate()`
//!       with one pointer — an integer compare per entry + early break after the last sample.
//! The first profile of (1) alone was ~1.03x (count is ~3% of the fn); (2) attacks the dominant
//! select pass. Both are byte- and RNG-identical (same eligible_len, same draw sequence, same set).
//!
//! ORIG = `sampled_eviction_candidate_keys_orig_bench` (OPT=false: O(n) count + HashSet select).
//! CAND = `sampled_eviction_candidate_keys_new_bench`  (OPT=true:  O(1) len + merge-walk select).
//! Both return byte-identical sampled keys for the same RNG state (same `eligible_len` value ⇒
//! same `next_rand() % eligible_len` sequence). The function is NON-destructive (read+clone,
//! no removal), so one store is built once and probed repeatedly — low-noise, like the KEYS A/B.
//! No key carries a TTL, so the volatile filter would count every key too (eligible_len == n).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 61;
const TARGET_SEGMENT_SECS: f64 = 0.05;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
// Redis default `maxmemory-samples`.
const SAMPLE_LIMIT: usize = 5;

fn build(n: usize) -> Store {
    let mut s = Store::new();
    for i in 0..n {
        s.set(format!("key:{i:07}").into_bytes(), b"v".to_vec(), None, 1);
    }
    s
}

#[inline(never)]
fn run_orig(s: &mut Store) -> usize {
    s.sampled_eviction_candidate_keys_orig_bench(false, SAMPLE_LIMIT)
        .len()
}
#[inline(never)]
fn run_new(s: &mut Store) -> usize {
    s.sampled_eviction_candidate_keys_new_bench(false, SAMPLE_LIMIT)
        .len()
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
        let e = time(reps, &mut s, run_orig);
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
        let nn = pair(run_orig, run_orig);
        let sp = pair(run_orig, run_new);
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
    bench("n50000", 50000);
}
