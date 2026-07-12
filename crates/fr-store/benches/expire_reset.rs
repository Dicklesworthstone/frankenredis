//! Same-binary A/B for `expire_milliseconds` re-setting a TTL on an existing key (the hot
//! EXPIRE/PEXPIRE path): the current-head reference re-probes `entries` through an empty
//! `with_mutated_entry`, while the candidate directly performs the identical digest-mutation
//! bump after key presence has already been proven. This isolates one redundant keyspace lookup
//! (and, when the digest is fresh, two unchanged whole-entry hashes), null-gated on the median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! Re-setting the SAME TTL is idempotent (same deadline, same expires_count), so each store stays
//! in a stable state across all reps — the timing reflects the per-call work only. Both arms leave
//! byte-identical observable state (asserted by the
//! `expire_digest_direct_bump_matches_entry_lookup_reference` unit test); this bench measures only
//! the digest-bookkeeping lookup elision.

use std::{env, hint::black_box, process::Command, time::Instant};

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.010;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"expire:key";

fn build_store() -> Store {
    let mut s = Store::new();
    s.set(KEY.to_vec(), b"v".to_vec(), None, 2_000);
    // Seed the TTL with the same (ttl, now) the bench re-applies, so every timed call is a
    // pure idempotent re-set (deadline unchanged) and the store never mutates shape.
    s.expire_milliseconds(KEY, 100_000, 2_000);
    s
}

fn expire_orig(s: &mut Store) -> bool {
    s.expire_milliseconds_digest_lookup_ref(black_box(KEY), black_box(100_000), black_box(2_000))
}
fn expire_new(s: &mut Store) -> bool {
    s.expire_milliseconds(black_box(KEY), black_box(100_000), black_box(2_000))
}

fn timed(f: fn(&mut Store) -> bool, s: &mut Store, reps: usize) -> f64 {
    let start = Instant::now();
    let mut acc = false;
    for _ in 0..reps {
        acc ^= f(s);
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

fn main() {
    let executable = env::current_exe().expect("current benchmark executable");
    let sha = Command::new("sha256sum")
        .arg(&executable)
        .output()
        .expect("run sha256sum");
    assert!(sha.status.success(), "sha256sum failed");
    let sha = String::from_utf8_lossy(&sha.stdout)
        .split_whitespace()
        .next()
        .expect("sha256sum digest")
        .to_owned();
    let hostname = Command::new("hostname").output().expect("run hostname");
    assert!(hostname.status.success(), "hostname failed");
    println!(
        "WORKER_ID {}\nBINARY_SHA256 one_binary_both_arms={sha}",
        String::from_utf8_lossy(&hostname.stdout).trim()
    );

    let mut store_o = build_store();
    let mut store_n = build_store();

    // Calibrate reps on the orig arm.
    let mut reps = 1usize;
    loop {
        let e = timed(expire_orig, &mut store_o, reps);
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
        // NULL: orig vs orig (both on store_o, sequential).
        let nn = if swap {
            let c = timed(expire_orig, &mut store_o, reps);
            timed(expire_orig, &mut store_o, reps) / c
        } else {
            let b = timed(expire_orig, &mut store_o, reps);
            b / timed(expire_orig, &mut store_o, reps)
        };
        // SPEED: orig (store_o) vs new (store_n).
        let sp = if swap {
            let c = timed(expire_new, &mut store_n, reps);
            timed(expire_orig, &mut store_o, reps) / c
        } else {
            let b = timed(expire_orig, &mut store_o, reps);
            b / timed(expire_new, &mut store_n, reps)
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
        "\n{:<18} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<18} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "expire_reset",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
