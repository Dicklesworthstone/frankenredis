//! Same-binary A/B for a no-TTL SET overwriting an existing key (the common `SET key value`
//! case, and the shared `internal_entries_insert_with_expiry` path behind MSET/GETSET/COPY/
//! RENAME/RESTORE): the pre-elision insert that always clones the owned key for
//! `expiry_deadlines` (`set_orig`, GATE=false) vs the guarded insert that elides that clone —
//! and, on an overwrite, the `get_key_value` lookup that fed it — when there is no TTL
//! (`set`, GATE=true). Null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! Overwriting the SAME key with the SAME (no-TTL) value is idempotent, so each store stays a single
//! stable entry across all reps. NOTE: `set` takes OWNED `Vec<u8>` args, so both arms pay two
//! per-call `to_vec` allocations the live BORROWED SET path (`set_plain_borrowed`) does not — this
//! DILUTES the ratio, making it a conservative lower bound on the real gain. Byte-identical effect
//! (asserted by the `set_gated_expiry_key_matches_orig` unit test).

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"session:user:0000000042";
const VAL: &[u8] = b"the-current-session-value-payload";

fn build_store() -> Store {
    let mut s = Store::new();
    s.set(KEY.to_vec(), VAL.to_vec(), None, 2_000); // seed so every timed call is an overwrite
    s
}

fn set_orig(s: &mut Store) {
    s.set_orig(black_box(KEY).to_vec(), black_box(VAL).to_vec(), None, 2_000);
}
fn set_new(s: &mut Store) {
    s.set(black_box(KEY).to_vec(), black_box(VAL).to_vec(), None, 2_000);
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

fn main() {
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
        "set_overwrite_no_ttl",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
