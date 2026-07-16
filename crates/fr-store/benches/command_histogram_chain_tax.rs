//! Measures the TAX the `record_canonical_with_kind` direct-field fast-path imposes on the
//! commands that are NOT in it.
//!
//! The fast-path is a linear chain of `if command == "get" { … } if command == "set" { … } …`
//! comparisons before the `HashMap<String>` fallback. Each direct field added (get/set/lpush/
//! rpush/sadd, then hset/zadd/incr) speeds up ITS OWN command by skipping the foldhash probe —
//! but every command NOT in the chain (expire/ttl/hget/del/zscore/exists/… — many of them hot)
//! must now fail all N string comparisons before reaching the HashMap. So the chain has an
//! OPTIMAL length: past some point the per-non-member strcmp tax on the (many) fall-through
//! commands outweighs the per-member probe savings on the (few) added commands.
//!
//! This A/B isolates that tax. Both arms record the SAME non-fast-path command ("expire") into
//! the SAME warm `HashMap` entry; the only difference is that the reference walks the full
//! direct-field chain first (production `record_canonical_with_kind`) while the candidate goes
//! straight to the HashMap (`bench_record_hashmap_only`). speedup = chain/direct, i.e. how much
//! slower a fall-through command's commandstats update is BECAUSE of the chain it has to skip.
//! A speedup meaningfully > 1 means extending the chain further would net-regress the fall-through
//! commands; ~1 means the chain is effectively free and length is not the constraint (the value
//! ceiling is then just the <1% per-command record cost on generically-dispatched commands).

use std::hint::black_box;
use std::time::Instant;

use fr_store::{CommandHistogramTracker, CommandRecordKind};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
// A representative HOT command that is NOT in the direct-field chain, so it pays the full
// fall-through scan before the HashMap probe.
const CMD: &str = "expire";

/// Reference (production for a fall-through command): walk the full direct-field chain, fail every
/// comparison, then probe the HashMap.
fn full_chain(t: &mut CommandHistogramTracker) {
    t.record_canonical_with_kind(black_box(CMD), 1, CommandRecordKind::Success);
}
/// Candidate: skip the chain, go straight to the warm HashMap get_mut.
fn direct_hashmap(t: &mut CommandHistogramTracker) {
    t.bench_record_hashmap_only(black_box(CMD), 1, CommandRecordKind::Success);
}

fn warm(f: fn(&mut CommandHistogramTracker)) -> CommandHistogramTracker {
    let mut t = CommandHistogramTracker::default();
    f(&mut t); // populate the "expire" HashMap entry so the timed loop never inserts
    t
}

fn timed(f: fn(&mut CommandHistogramTracker), t: &mut CommandHistogramTracker, reps: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..reps {
        f(t);
    }
    black_box(&*t);
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
    // Correctness: both arms land on the same "expire" HashMap bucket (it is not a direct field).
    {
        let mut a = warm(full_chain);
        let mut b = warm(direct_hashmap);
        for _ in 0..9 {
            full_chain(&mut a);
            direct_hashmap(&mut b);
        }
        assert_eq!(
            a.bench_hashmap_get(CMD).map(|h| h.calls),
            b.bench_hashmap_get(CMD).map(|h| h.calls),
            "both arms record the same fall-through command into the HashMap"
        );
        assert!(a.get(CMD).is_some(), "expire resolves via the HashMap, not a direct field");
    }

    // Reference arm = full chain (the slower, production fall-through path).
    let mut store_ref = warm(full_chain);
    let mut store_cand = warm(direct_hashmap);

    let mut reps = 1usize;
    loop {
        let e = timed(full_chain, &mut store_ref, reps);
        if e >= TARGET_SEGMENT_SECS || reps > 1 << 22 {
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
            let c = timed(full_chain, &mut store_ref, reps);
            timed(full_chain, &mut store_ref, reps) / c
        } else {
            let b = timed(full_chain, &mut store_ref, reps);
            b / timed(full_chain, &mut store_ref, reps)
        };
        // speedup = full-chain time / direct-hashmap time = the chain-scan tax factor.
        let sp = if swap {
            let c = timed(direct_hashmap, &mut store_cand, reps);
            timed(full_chain, &mut store_ref, reps) / c
        } else {
            let b = timed(full_chain, &mut store_ref, reps);
            b / timed(direct_hashmap, &mut store_cand, reps)
        };
        if round == 0 {
            continue;
        }
        nulls.push(nn);
        speeds.push(sp);
    }

    let null_med = median(&mut nulls);
    let tax = median(&mut speeds);
    let lo = pct(&nulls, NULL_LO);
    let hi = pct(&nulls, NULL_HI);
    let verdict = if tax > 1.0 && tax > hi {
        "CHAIN TAXES FALL-THROUGH"
    } else if tax < 1.0 && tax < lo {
        "chain faster (unexpected)"
    } else {
        "chain effectively free"
    };
    println!(
        "\n{:<28} {:>8} {:>9} {:>16} {:>8} {:>12} {:>26}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "chain/direct", "verdict"
    );
    println!(
        "{:<28} {:>8} {:>9.4} {:>16} {:>8.2} {:>11.3}x {:>26}",
        "fastpath_chain_tax",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        tax,
        verdict
    );
}
