//! Same-binary A/B for the small HASH/SET DUMP payload memoization (frankenredis-hsdumpcache).
//!
//! The DUMP-payload cache covered list + zset (zset shipped as lever6 at -6.7..-11.6% instr on a
//! repeat-DUMP blast); hash + set were left out even though they encode via the same listpack path.
//! Now a repeated DUMP of an unchanged small hash/set is a `Vec::clone` of the cached bytes instead
//! of a full listpack re-encode (+ CRC64). ORIG = `bench_dump_{hash,set}_reencode` (fresh encode),
//! CAND = `bench_dump_list_cache_get` (the generic cache-hit clone). Both produce BYTE-IDENTICAL
//! payloads (asserted here + `hash_set_dump_cache_invalidates_on_writes_and_config`), so the ratio
//! isolates the eliminated re-encode.
//!
//! Substrate = the cc bench roster: ONE binary, adjacent-pair interleave (swap on odd rounds),
//! black_box, reps calibrated per input, median of 41 paired ratios, gated on the candidate median
//! outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const TS: u64 = 2;

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

fn run(
    label: &str,
    store: &Store,
    key: &[u8],
    orig: &dyn Fn(&Store) -> usize,
    cand: &dyn Fn(&Store) -> usize,
) {
    let time = |f: &dyn Fn(&Store) -> usize, s: &Store, reps: usize| -> f64 {
        let start = Instant::now();
        let mut acc = 0usize;
        for _ in 0..reps {
            acc = acc.wrapping_add(f(black_box(s)));
        }
        black_box(acc);
        start.elapsed().as_secs_f64()
    };
    let mut reps = 1usize;
    loop {
        let e = time(orig, store, reps);
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
        let pair = |bf: &dyn Fn(&Store) -> usize, cf: &dyn Fn(&Store) -> usize| {
            if swap {
                let c = time(cf, store, reps);
                time(bf, store, reps) / c
            } else {
                let b = time(bf, store, reps);
                b / time(cf, store, reps)
            }
        };
        let nn = pair(orig, orig);
        let sp = pair(orig, cand);
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
        "WIN(cache)"
    } else if speedup < 1.0 && speedup < lo {
        "REGRESSION"
    } else {
        "indistinguishable"
    };
    let _ = key;
    println!(
        "{:<16} {:>7} {:>9.4} {:>16} {:>8.2} {:>12.2}x {:>14}",
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
        "\n{:<16} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "reenc/clone", "verdict"
    );

    for &n in &[8usize, 40, 120] {
        // HASH (small listpack): n fields, short field/value.
        let mut hs = Store::new();
        for i in 0..n {
            hs.hset(b"h", format!("f{i:04}").into_bytes(), format!("v{i:04}").into_bytes(), TS)
                .unwrap();
        }
        let seed = hs.dump_key(b"h", TS).expect("hash dump populates cache");
        let re = hs.bench_dump_hash_reencode(b"h").expect("hash reencode");
        let cg = hs.bench_dump_list_cache_get(b"h").expect("cache get");
        assert_eq!(seed, re, "hash n={n}: dump_key vs reencode diverged");
        assert_eq!(re, cg, "hash n={n}: reencode vs cache-clone diverged");
        run(
            &format!("hash_{n}"),
            &hs,
            b"h",
            &|s: &Store| s.bench_dump_hash_reencode(b"h").map_or(0, |v| v.len()),
            &|s: &Store| s.bench_dump_list_cache_get(b"h").map_or(0, |v| v.len()),
        );

        // SET (small listpack): n short string members.
        let mut ss = Store::new();
        let members: Vec<Vec<u8>> = (0..n).map(|i| format!("m{i:05}").into_bytes()).collect();
        ss.sadd(b"s", &members, TS).unwrap();
        let seed = ss.dump_key(b"s", TS).expect("set dump populates cache");
        let re = ss.bench_dump_set_reencode(b"s").expect("set reencode");
        let cg = ss.bench_dump_list_cache_get(b"s").expect("cache get");
        assert_eq!(seed, re, "set n={n}: dump_key vs reencode diverged");
        assert_eq!(re, cg, "set n={n}: reencode vs cache-clone diverged");
        run(
            &format!("set_{n}"),
            &ss,
            b"s",
            &|s: &Store| s.bench_dump_set_reencode(b"s").map_or(0, |v| v.len()),
            &|s: &Store| s.bench_dump_list_cache_get(b"s").map_or(0, |v| v.len()),
        );
    }
}
