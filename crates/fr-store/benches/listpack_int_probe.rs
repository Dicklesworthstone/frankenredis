//! Same-binary A/B for the listpack-sizing canonical-integer probe (frankenredis-bssrh).
//!
//! `list_lp_int` (via `list_lp_entry_bytes`, called PER ELEMENT during list/set listpack sizing
//! on LPUSH/RPUSH/SADD/RESTORE) decides whether a listpack element is a canonical integer.
//! ORIG parsed the i64 then confirmed canonicity with `value.to_string().as_bytes() == entry`
//! — a String heap ALLOCATION per integer-looking element, purely to compare. bssrh
//! (`9583877a8`) replaced it with the allocation-free `listpack_int_bytes_are_canonical`
//! predicate (mirrored here verbatim from fr-store lib.rs). Both make the byte-identical
//! accept/reject+value decision (asserted); NEW does zero heap work for the check.
//!
//! ORIG = to_string round-trip (alloc); CAND = canonical predicate (alloc-free).
//! verdict WIN => the alloc-free probe is faster => bssrh is a real win.
//!
//! Substrate = the cc bench roster (quicklist_encode.rs): ONE binary, adjacent-pair interleave
//! (swap on odd rounds), black_box, reps calibrated per input, median of 41 paired ratios,
//! gated on the candidate median outside the null (orig-vs-orig) p5..p95.

use std::hint::black_box;
use std::time::Instant;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.004;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

/// ORIG: parse then confirm canonicity by rendering the value back to a String (heap alloc).
fn probe_to_string(entry: &[u8]) -> Option<i64> {
    if entry.is_empty() || entry.len() >= 21 {
        return None;
    }
    let value: i64 = std::str::from_utf8(entry).ok()?.parse().ok()?;
    if value.to_string().as_bytes() == entry {
        Some(value)
    } else {
        None
    }
}

/// CAND: allocation-free canonical predicate (verbatim from fr-store `listpack_int_bytes_are_canonical`),
/// then parse. Byte-identical accept/reject to ORIG.
fn is_canonical(entry: &[u8]) -> bool {
    let digits = match entry.first() {
        Some(b'-') => &entry[1..],
        Some(_) => entry,
        None => return false,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    if digits[0] == b'0' && digits.len() > 1 {
        return false;
    }
    if entry[0] == b'-' && digits == b"0" {
        return false;
    }
    true
}
fn probe_canonical(entry: &[u8]) -> Option<i64> {
    if entry.is_empty() || entry.len() >= 21 || !is_canonical(entry) {
        return None;
    }
    std::str::from_utf8(entry).ok()?.parse().ok()
}

/// A batch of listpack-element byte strings: `n` canonical ints of magnitude ~`10^digits`.
fn int_entries(n: usize, digits: u32) -> Vec<Vec<u8>> {
    let base = 10i64.pow(digits.saturating_sub(1)).max(1);
    (0..n)
        .map(|i| {
            let v = base + (i as i64) * 7 - if i % 3 == 0 { base } else { 0 };
            v.to_string().into_bytes()
        })
        .collect()
}
/// Mixed batch: canonical ints interleaved with non-canonical entries ("007", "-0", "12a", "").
fn mixed_entries(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| match i % 5 {
            0 => (i as i64 * 131 - 40).to_string().into_bytes(),
            1 => format!("00{i}").into_bytes(),
            2 => b"-0".to_vec(),
            3 => format!("{i}x").into_bytes(),
            _ => (i as i64 * -977).to_string().into_bytes(),
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
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        ("int_1d_128", int_entries(128, 1)),
        ("int_6d_128", int_entries(128, 6)),
        ("int_18d_128", int_entries(128, 18)),
        ("mixed_128", mixed_entries(128)),
    ];

    // Correctness gate: both probes make the byte-identical decision on every entry.
    for (label, entries) in &cases {
        for e in entries {
            assert_eq!(probe_to_string(e), probe_canonical(e), "{label}: probe diverged on {e:?}");
        }
    }

    println!(
        "\n{:<14} {:>7} {:>9} {:>16} {:>8} {:>12} {:>14}",
        "workload", "reps", "NULL med", "null p5..p95", "null cv%", "canon/tostr", "verdict"
    );

    for (label, entries) in &cases {
        let orig = |es: &[Vec<u8>]| es.iter().filter_map(|e| probe_to_string(e)).count();
        let cand = |es: &[Vec<u8>]| es.iter().filter_map(|e| probe_canonical(e)).count();
        let time = |f: &dyn Fn(&[Vec<u8>]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(entries)));
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
            let pair = |bf: &dyn Fn(&[Vec<u8>]) -> usize, cf: &dyn Fn(&[Vec<u8>]) -> usize| {
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
            "WIN(canon)"
        } else if speedup < 1.0 && speedup < lo {
            "REGRESSION"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<14} {:>7} {:>9.4} {:>16} {:>8.2} {:>11.3}x {:>14}",
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
