//! Same-binary A/B for the listpack per-entry backlen decode: the original reverse-7-bit
//! validation loop vs the single-byte fast path (`entry_len_with_backlen`), null-gated on the
//! median.
//!
//! Substrate matches the other cc benches: ONE binary / ONE invocation, adjacent-pair interleaving
//! (order swapped on odd rounds), `black_box`, reps calibrated per input, median of paired per-round
//! ratios, gated on the candidate median lying outside the null control's p5..p95 spread (`cv`
//! reported, never gated).
//!
//! `bench_backlen_walk(data, orig)` walks a listpack summing per-entry lengths; `entry_data_len`
//! (identical for both arms) supplies `data_len`, so the timing difference isolates ONLY the backlen
//! path. Inputs model the listpack shapes RESTORE/DUMP decode hits: a small-string set, an
//! integer-heavy set (all 1-byte backlens), and a field/value hash — every entry `data_len <= 127`,
//! the fast-path case. Both arms return the identical sum (asserted before timing).

use std::hint::black_box;
use std::time::Instant;

use fr_persist::listpack::bench_backlen_walk;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

const LISTPACK_HEADER_SIZE: usize = 6;
const LISTPACK_EOF: u8 = 0xFF;

/// Append a listpack string entry (6-bit or 12-bit encoding + 1-byte backlen; all payloads
/// here keep `data_len <= 127`).
fn push_str_entry(out: &mut Vec<u8>, s: &[u8]) {
    assert!(s.len() <= 63, "test payloads stay in the 6-bit-str / 1-byte-backlen range");
    let data_len = 1 + s.len();
    out.push(0x80 | (s.len() as u8));
    out.extend_from_slice(s);
    out.push(data_len as u8); // data_len <= 127 => single-byte backlen == data_len
}

/// Append a listpack 32-bit int entry (5-byte data + 1-byte backlen).
fn push_int_entry(out: &mut Vec<u8>, v: i32) {
    out.push(0xF3);
    out.extend_from_slice(&v.to_le_bytes());
    out.push(5);
}

fn assemble(entry_bytes: &[u8], num_elements: usize) -> Vec<u8> {
    let total_bytes = (LISTPACK_HEADER_SIZE + entry_bytes.len() + 1) as u32;
    let mut out = Vec::with_capacity(total_bytes as usize);
    out.extend_from_slice(&total_bytes.to_le_bytes());
    out.extend_from_slice(&(num_elements.min(u16::MAX as usize) as u16).to_le_bytes());
    out.extend_from_slice(entry_bytes);
    out.push(LISTPACK_EOF);
    out
}

fn string_set(n: usize) -> Vec<u8> {
    let mut e = Vec::new();
    for i in 0..n {
        push_str_entry(&mut e, format!("member:{i:06}").as_bytes());
    }
    assemble(&e, n)
}

fn int_set(n: usize) -> Vec<u8> {
    let mut e = Vec::new();
    for i in 0..n {
        push_int_entry(&mut e, (i as i32).wrapping_mul(2_654_435));
    }
    assemble(&e, n)
}

fn field_value_hash(pairs: usize) -> Vec<u8> {
    let mut e = Vec::new();
    for i in 0..pairs {
        push_str_entry(&mut e, format!("field:{i:05}").as_bytes());
        push_str_entry(&mut e, format!("value:{i:05}:data").as_bytes());
    }
    assemble(&e, pairs * 2)
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
        "\n{:<16} {:>7} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "listpack", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );

    let cases: &[(&str, Vec<u8>)] = &[
        ("string_set_256", string_set(256)),
        ("int_set_512", int_set(512)),
        ("hash_128f", field_value_hash(128)),
    ];

    for (label, lp) in cases {
        // Correctness gate: both arms sum identically.
        assert_eq!(
            bench_backlen_walk(lp, true).unwrap(),
            bench_backlen_walk(lp, false).unwrap(),
            "{label}: orig/new backlen sums diverged"
        );

        let orig = |d: &[u8]| bench_backlen_walk(d, true).unwrap();
        let cand = |d: &[u8]| bench_backlen_walk(d, false).unwrap();
        let time = |f: &dyn Fn(&[u8]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(lp)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };
        let mut reps = 1usize;
        loop {
            let e = time(&orig, reps);
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
            let pair = |bf: &dyn Fn(&[u8]) -> usize, cf: &dyn Fn(&[u8]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    time(bf, reps) / c
                } else {
                    let b = time(bf, reps);
                    b / time(cf, reps)
                }
            };
            let nn = pair(&orig, &orig);
            let sp = pair(&orig, &cand);
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
            "{:<16} {:>7} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
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
