//! Same-binary A/B for the zset RDB-load member-clone elision (frankenredis-zsetrdbmove).
//!
//! On RDB load the `RdbValue::SortedSet` arm produces OWNED (score, member) pairs, then inserts
//! them into a fresh key. The old wiring called the borrowed `zadd`, whose fresh-key path CLONES
//! every member into its collapse `HashMap<Vec<u8>, f64>` (`member.clone()`) — N allocs+copies per
//! zset. The new wiring calls `zadd_plain_owned` (the owned-input ZADD the ZADD command already
//! uses), which MOVES each member into the map. Both produce a BYTE-IDENTICAL SortedSet (locked by
//! `zadd_plain_owned_matches_zadd_for_rdb_load_shapes`), so the ratio isolates the eliminated clone.
//!
//! Faithful RDB-load model: each timed call DELs the key, then `template.clone()`s the pairs (the
//! RDB decode materializes owned members in BOTH the old and new code — this is not A/B inflation,
//! it is the shared per-key load cost), then inserts. So the measured ratio is the real end-to-end
//! RDB-load-insert speedup for one zset.
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

/// `n` unique members; `w`-byte values (w>64 forces skiplist by value width); mixed score kinds.
fn template(n: usize, w: usize) -> Vec<(f64, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let score = match i % 4 {
                0 => i as f64,
                1 => -(i as f64) - 0.5,
                2 => i as f64 + 0.25,
                _ => 1e18 + i as f64,
            };
            let mut m = format!("member{i:06}:").into_bytes();
            m.resize(w.max(m.len()), b'q');
            (score, m)
        })
        .collect()
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
    println!(
        "\n{:<16} {:>7} {:>9} {:>16} {:>8} {:>13} {:>14}",
        "zset", "reps", "NULL med", "null p5..p95", "null cv%", "clone/move", "verdict"
    );

    let cases: &[(&str, usize, usize)] = &[
        ("n128_w12_lp", 128, 12),  // listpack (<=128 entries, short values)
        ("n600_w12_sk", 600, 12),  // skiplist by count
        ("n2000_w12_sk", 2000, 12),
        ("n300_w80_sk", 300, 80),  // skiplist by value width, longer member copies
    ];

    for &(label, n, w) in cases {
        let tmpl = template(n, w);

        // Byte-exactness spot check before timing.
        let da = {
            let mut s = Store::new();
            s.zadd(b"z", &tmpl, TS).unwrap();
            s.dump_key(b"z", TS)
        };
        let db = {
            let mut s = Store::new();
            s.zadd_plain_owned(b"z", tmpl.clone(), TS).unwrap();
            s.dump_key(b"z", TS)
        };
        assert_eq!(da, db, "{label}: zadd vs zadd_plain_owned DUMP diverged");

        let orig = |s: &mut Store| -> usize {
            s.del(&[b"z".to_vec()], TS);
            let owned = tmpl.clone();
            s.zadd(b"z", &owned, TS).unwrap()
        };
        let cand = |s: &mut Store| -> usize {
            s.del(&[b"z".to_vec()], TS);
            let owned = tmpl.clone();
            s.zadd_plain_owned(b"z", owned, TS).unwrap()
        };
        let time = |f: &dyn Fn(&mut Store) -> usize, s: &mut Store, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(s)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };

        let mut store = Store::new();
        let mut reps = 1usize;
        loop {
            let e = time(&orig, &mut store, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 18 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 4;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut speeds = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: &dyn Fn(&mut Store) -> usize, cf: &dyn Fn(&mut Store) -> usize, s: &mut Store| {
                if swap {
                    let c = time(cf, s, reps);
                    time(bf, s, reps) / c
                } else {
                    let bt = time(bf, s, reps);
                    bt / time(cf, s, reps)
                }
            };
            let nn = pair(&orig, &orig, &mut store);
            let sp = pair(&orig, &cand, &mut store);
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
            "WIN(move)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<16} {:>7} {:>9.4} {:>16} {:>8.2} {:>12.3}x {:>14}",
            label,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            speedup,
            verdict
        );
    }
}
