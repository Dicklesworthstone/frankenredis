//! (frankenredis-smovefast) Same-binary A/B for SMOVE's destination add. SMOVE moves a member
//! between sets by calling `self.sadd(dst, &[member], now)`; `sadd` is generic over `AsRef<[u8]>`
//! and copies the member straight into the destination set via `insert_borrowed`. The old code
//! passed `&[member.to_vec()]` — a redundant intermediate allocation + copy of the member bytes
//! that upstream `t_set.c::smoveCommand` never makes. This bench isolates exactly that call:
//! `sadd(dst, &[member.to_vec()])` (owned) vs `sadd(dst, &[member])` (borrowed), re-adding an
//! ALREADY-PRESENT member so the set stays a single stable entry of constant size (both arms pay
//! the same contains-probe; the isolated difference is the owned intermediate).
//!
//! The saving is proportional to the member size (→ 1x for a tiny member, decisive for a large
//! one), so it is measured for both a small packed-set member and a large generic-set member.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated once, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated). Byte-identical result: `sadd` is generic, so owned/borrowed members
//! reach the identical `insert_borrowed` path.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"s:dst";

fn member(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % 26) as u8).collect()
}

/// Build a set that already contains `m` plus a few other members, at the encoding implied by the
/// member length (>64 bytes promotes out of the packed set to the generic hashset).
fn build(m: &[u8]) -> Store {
    let mut s = Store::new();
    for i in 0..8u32 {
        let mut other = m.to_vec();
        other.extend_from_slice(format!(":{i}").as_bytes());
        s.sadd(KEY, &[other], 2_000).expect("sadd");
    }
    s.sadd(KEY, &[m.to_vec()], 2_000).expect("sadd");
    s
}

fn add_owned(s: &mut Store, m: &[u8]) {
    let _ = s.sadd(black_box(KEY), &[black_box(m).to_vec()], 2_000);
}
fn add_borrowed(s: &mut Store, m: &[u8]) {
    let _ = s.sadd(black_box(KEY), &[black_box(m)], 2_000);
}

fn timed(f: fn(&mut Store, &[u8]), s: &mut Store, m: &[u8], reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(s, m);
    }
    black_box(&*s);
    start.elapsed().as_secs_f64()
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    r[r.len() / 2]
}
fn cv(r: &[f64]) -> f64 {
    let mean = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / mean
}
fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn run_ab(label: &str, mlen: usize) {
    let m = member(mlen);
    let mut store_o = build(&m);
    let mut store_n = build(&m);

    let mut reps = 1usize;
    loop {
        let e = timed(add_owned, &mut store_o, &m, reps);
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
        let nn = if swap {
            let c = timed(add_owned, &mut store_o, &m, reps);
            timed(add_owned, &mut store_o, &m, reps) / c
        } else {
            let b = timed(add_owned, &mut store_o, &m, reps);
            b / timed(add_owned, &mut store_o, &m, reps)
        };
        let sp = if swap {
            let c = timed(add_borrowed, &mut store_n, &m, reps);
            timed(add_owned, &mut store_o, &m, reps) / c
        } else {
            let b = timed(add_owned, &mut store_o, &m, reps);
            b / timed(add_borrowed, &mut store_n, &m, reps)
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
        "{:<20} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
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
        "\n{:<20} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    run_ab("smove_packed_32", 32);
    run_ab("smove_generic_1024", 1024);
}
