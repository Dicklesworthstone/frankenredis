//! Same-binary A/B for the live variadic HSET/HMSET encoding refresh.
//!
//! ORIG = `hset_borrowed_many_rescan`: after applying four short field/value pairs, walk every
//! field/value in the hash to re-derive the sticky listpack/hashtable flag. CAND =
//! `hset_borrowed_many`: inspect the four incoming pairs plus the final count, falling back to the
//! exact final-map scan only for an oversized input. Both arms mutate the same stable existing hash
//! and preserve its cardinality, so the ratio isolates replacing O(hash width) with O(command
//! width). The A/A null and A/B candidate are interleaved in alternating order within each round.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.02;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
static UPDATE: [&[u8]; 8] = [
    b"f00000", b"w0", b"f00001", b"w1", b"f00002", b"w2", b"f00003", b"w3",
];

fn build(fields: usize) -> Store {
    let mut store = Store::new();
    for i in 0..fields {
        store
            .hset_borrowed(b"h", format!("f{i:05}").as_bytes(), b"old".to_vec(), 1)
            .unwrap();
    }
    store
}

#[inline(never)]
fn run_incremental(store: &mut Store) -> usize {
    store
        .hset_borrowed_many(black_box(b"h"), black_box(&UPDATE), 2)
        .unwrap()
}

#[inline(never)]
fn run_rescan(store: &mut Store) -> usize {
    store
        .hset_borrowed_many_rescan(black_box(b"h"), black_box(&UPDATE), 2)
        .unwrap()
}

fn time(reps: usize, store: &mut Store, f: fn(&mut Store) -> usize) -> f64 {
    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..reps {
        acc = acc.wrapping_add(f(black_box(store)));
    }
    black_box(acc);
    start.elapsed().as_secs_f64()
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    values[values.len() / 2]
}

fn cv(values: &[f64]) -> f64 {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    100.0 * (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
        / mean
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn bench(label: &str, fields: usize) {
    let mut store = build(fields);
    let mut reps = 1usize;
    loop {
        let elapsed = time(reps, &mut store, run_rescan);
        if elapsed >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
            reps = ((reps as f64) * (TARGET_SEGMENT_SECS / elapsed.max(1e-9)).max(1.0)).ceil()
                as usize;
            break;
        }
        reps *= 4;
    }

    let mut nulls = Vec::with_capacity(ROUNDS);
    let mut speeds = Vec::with_capacity(ROUNDS);
    for round in 0..=ROUNDS {
        let swap = round % 2 == 1;
        let mut pair = |base: fn(&mut Store) -> usize, cand: fn(&mut Store) -> usize| {
            if swap {
                let candidate = time(reps, &mut store, cand);
                time(reps, &mut store, base) / candidate
            } else {
                let baseline = time(reps, &mut store, base);
                baseline / time(reps, &mut store, cand)
            }
        };
        let null = pair(run_rescan, run_rescan);
        let speed = pair(run_rescan, run_incremental);
        if round != 0 {
            nulls.push(null);
            speeds.push(speed);
        }
    }

    let null_median = median(&mut nulls);
    let speedup = median(&mut speeds);
    let low = percentile(&nulls, NULL_LO);
    let high = percentile(&nulls, NULL_HI);
    let verdict = if speedup > 1.0 && speedup > high {
        "WIN(incremental)"
    } else if speedup < 1.0 && speedup < low {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    println!(
        "{label:<12} {reps:>8} {null_median:>9.4} {:>16} {:>8.2} {speedup:>10.3}x {verdict:>18}",
        format!("[{low:.3}, {high:.3}]"),
        cv(&nulls),
    );
}

fn main() {
    println!(
        "\n{:<12} {:>8} {:>9} {:>16} {:>8} {:>11} {:>18}",
        "hash_fields", "reps", "NULL med", "null p5..p95", "null cv%", "cand/orig", "verdict"
    );
    bench("f8_k4", 8);
    bench("f64_k4", 64);
    bench("f256_k4", 256);
    bench("f511_k4", 511);
}
