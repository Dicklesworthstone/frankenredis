//! 6lgnu VALIDATION (not a product A/B): quantify the rank-treap's per-ZADD maintenance tax.
//!
//! fr's Full zset keeps THREE structures — `dict: IndexMap<Arc<[u8]>,f64>` (member->score),
//! `ordered: BTreeMap/Vec` (score order), and a LAZY order-statistic `rank_tree` treap (O(log n)
//! ZRANK). The treap is `None` until the first rank query, then kept in sync at every mutation. Redis
//! reaches the same with ONE skiplist (order + rank fused). The `6lgnu` bead proposes replacing
//! `ordered`+treap with a unified skiplist; its write-side upside is bounded by what fr pays to keep
//! the treap warm across writes. This bench MEASURES that: identical N-member ZADD builds, treap COLD
//! (never queried → no maintenance) vs WARM (queried once after Full promotion → maintained on every
//! subsequent ZADD). tax = warm/cold − 1.
//!
//! Interpretation guide (recorded so the 6lgnu decision is data-driven, not assumed):
//!   * tax ≈ 0 (indistinguishable)  → the lazy treap is ~free on writes; 6lgnu's write-side win is
//!                                     illusory, only ZRANK read latency is on the table. DEPRIORITIZE.
//!   * tax large & gated            → keeping rank queryable during writes is genuinely expensive;
//!                                     a fused skiplist could recover it for ZADD+ZRANK workloads.
//!
//! Same substrate as the cc bench roster: ONE binary, adjacent-pair interleave, black_box, reps
//! calibrated once, median of paired ratios, null-gated (cold-vs-cold), cv reported never gated.

use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.010;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;
const KEY: &[u8] = b"z";
const NOW: u64 = 1_000;
// Full-promotion happens at zset-max-listpack-entries (default 128); warm the treap comfortably
// past it so the whole measured tail runs on the Full (BTreeMap + treap) representation.
const WARM_AT: usize = 256;

fn member(i: usize) -> Vec<u8> {
    format!("m{i:07}").into_bytes()
}

/// Build an N-member zset (distinct ascending scores) with the treap left COLD (never queried, so
/// `rank_tree` stays None and no ZADD maintains it).
fn build_cold(n: usize) -> usize {
    let mut s = Store::new();
    for i in 0..n {
        let _ = s.zadd(KEY, &[(i as f64, member(i))], NOW);
    }
    s.zcard(KEY, NOW).unwrap_or(0)
}

/// Identical build, but one ZRANK after WARM_AT warms the treap; every later ZADD then maintains it.
fn build_warm(n: usize) -> usize {
    let mut s = Store::new();
    for i in 0..n {
        let _ = s.zadd(KEY, &[(i as f64, member(i))], NOW);
        if i == WARM_AT {
            let _ = s.zrank(KEY, black_box(member(0).as_slice()), NOW); // warm the treap once
        }
    }
    s.zcard(KEY, NOW).unwrap_or(0)
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
    let sizes: &[(&str, usize)] = &[("n1k", 1_000), ("n5k", 5_000), ("n20k", 20_000)];

    // Correctness: cold and warm build byte-identical zsets (same cardinality; warm just also
    // queried rank once). The treap is behavior-invisible, so ZRANK agrees either way.
    for &(label, n) in sizes {
        assert_eq!(build_cold(n), build_warm(n), "{label}: card mismatch");
        assert_eq!(build_cold(n), n, "{label}: expected {n} members");
    }

    println!(
        "\n{:<8} {:>7} {:>9} {:>16} {:>8} {:>12} {:>14}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "warm/cold", "verdict"
    );

    for &(label, n) in sizes {
        let time = |f: fn(usize) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(n)));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };

        let mut reps = 1usize;
        loop {
            let e = time(build_cold, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 2;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut taxes = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            // ratio = warm / cold  (>1 ⇒ the treap adds write cost)
            let pair = |coldf: fn(usize) -> usize, warmf: fn(usize) -> usize| {
                if swap {
                    let w = time(warmf, reps);
                    w / time(coldf, reps)
                } else {
                    let c = time(coldf, reps);
                    time(warmf, reps) / c
                }
            };
            let nn = pair(build_cold, build_cold);
            let tx = pair(build_cold, build_warm);
            if round == 0 {
                continue;
            }
            nulls.push(nn);
            taxes.push(tx);
        }

        let null_med = median(&mut nulls);
        let tax = median(&mut taxes);
        let lo = pct(&nulls, NULL_LO);
        let hi = pct(&nulls, NULL_HI);
        let verdict = if tax > hi {
            "TREAP-TAX-REAL"
        } else if tax < lo {
            "warm-faster?!"
        } else {
            "indistinguishable"
        };
        println!(
            "{:<8} {:>7} {:>9.4} {:>16} {:>8.2} {:>11.3}x {:>14}",
            label,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            tax,
            verdict
        );
    }
}
