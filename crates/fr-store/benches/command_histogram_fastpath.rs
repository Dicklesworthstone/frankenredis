//! Same-binary A/B for extending the `CommandHistogramTracker` direct-field fast-path to the
//! next-hottest borrowed-write commands (hset / zadd / incr).
//!
//! Every command records its latency into the per-command histogram via
//! `record_canonical_with_kind`. get/set/lpush/rpush/sadd already resolve to a direct
//! `Option<CommandHistogram>` field; every OTHER command fell through to
//! `self.histograms.get_mut(command)` — a `HashMap<String, _, foldhash>` hash + probe on EVERY
//! record. A live perf-record put that probe at ~1% self-time on the hset/zadd/incr hot paths
//! (get/set are fast-pathed, which is why only the other commands carried it). Adding direct
//! fields for hset/zadd/incr removes the hash+probe for those commands.
//!
//! Reference = `bench_record_hashmap_only` (mirrors the pre-change fallback: warm
//! `histograms.get_mut` foldhash probe). Candidate = `record_canonical_with_kind` (warm direct
//! field). Both arms are pre-warmed so no allocation/insert happens in the timed loop — the only
//! per-call difference is the eliminated foldhash+probe. Both mutate one stable histogram slot
//! (calls counter increments), so keyspace/state is non-growing across reps. Byte-identical
//! histogram state is asserted by the fr-store unit test
//! `command_histogram_new_direct_fields_report_consistently_with_hashmap_reference`.

use std::hint::black_box;
use std::time::Instant;

use fr_store::{CommandHistogramTracker, CommandRecordKind};

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.003;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const CMD: &str = "hset";

/// Candidate: direct-field fast path (warm — the `hset` field is already `Some`).
fn direct_field(t: &mut CommandHistogramTracker) {
    t.record_canonical_with_kind(black_box(CMD), 1, CommandRecordKind::Success);
}
/// Reference: warm `HashMap<String>` get_mut foldhash probe (the pre-change fallback).
fn hashmap_probe(t: &mut CommandHistogramTracker) {
    t.bench_record_hashmap_only(black_box(CMD), 1, CommandRecordKind::Success);
}

fn warm_field() -> CommandHistogramTracker {
    let mut t = CommandHistogramTracker::default();
    direct_field(&mut t); // populate the direct field so the timed loop never inserts
    t
}
fn warm_hashmap() -> CommandHistogramTracker {
    let mut t = CommandHistogramTracker::default();
    hashmap_probe(&mut t); // populate the HashMap entry so the timed loop never inserts
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
    // Correctness: both arms land on the same command bucket ("hset") and increment its call count.
    {
        let mut a = warm_field();
        let mut b = warm_hashmap();
        for _ in 0..9 {
            direct_field(&mut a);
            hashmap_probe(&mut b);
        }
        // The direct-field arm is read via get(); the hashmap arm must be read via the raw
        // HashMap accessor, since get("hset") now correctly short-circuits to the (empty) field.
        assert_eq!(
            a.get(CMD).map(|h| h.calls),
            b.bench_hashmap_get(CMD).map(|h| h.calls),
            "direct-field and hashmap arms must record identical call counts"
        );
    }

    let mut store_o = warm_hashmap();
    let mut store_n = warm_field();

    let mut reps = 1usize;
    loop {
        let e = timed(hashmap_probe, &mut store_o, reps);
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
            let c = timed(hashmap_probe, &mut store_o, reps);
            timed(hashmap_probe, &mut store_o, reps) / c
        } else {
            let b = timed(hashmap_probe, &mut store_o, reps);
            b / timed(hashmap_probe, &mut store_o, reps)
        };
        let sp = if swap {
            let c = timed(direct_field, &mut store_n, reps);
            timed(hashmap_probe, &mut store_o, reps) / c
        } else {
            let b = timed(hashmap_probe, &mut store_o, reps);
            b / timed(direct_field, &mut store_n, reps)
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
        "\n{:<28} {:>8} {:>9} {:>16} {:>8} {:>10} {:>12}",
        "op", "reps", "NULL med", "null p5..p95", "null cv%", "speedup", "verdict"
    );
    println!(
        "{:<28} {:>8} {:>9.4} {:>16} {:>8.2} {:>9.3}x {:>12}",
        "histogram_direct_field",
        reps,
        null_med,
        format!("[{lo:.3}, {hi:.3}]"),
        cv(&nulls),
        speedup,
        verdict
    );
}
