//! Callgrind A/B driver for the LZF literal-run batching (frankenredis-qj6jn slice 2).
//!
//! Wall clock is unusable on this host (loadavg swings past 70 within minutes and a
//! client-side null has already failed at 0.7491x), so this driver is built to be read
//! by callgrind, which counts retired instructions deterministically and is therefore
//! load-immune. It does ONE thing per process: run one arm on one payload N times.
//!
//! Per-op cost comes from the SLOPE, not from a single run: the same arm is measured at
//! two op counts and the totals are differenced, so process startup, payload
//! construction and teardown are identical in both and cancel exactly. That removes the
//! need for callgrind_control, which is unreliable here (vgdb FIFO races).
//!
//!     lzf_literal_ab <arm:batch|push> <payload> <reps>
//!
//! Both arms MUST emit byte-identical output; the driver asserts it before timing so a
//! divergent build cannot report a speedup.

use std::hint::black_box;

/// Listpack-shaped payload: small string entries each followed by their backlen byte.
/// This is the shape the RESTORE/DUMP path actually compresses, and the one the
/// 1.76x-vs-redis kernel gap was measured on (200 keys x 40 fields).
fn listpack_like(fields: u32) -> Vec<u8> {
    let mut p = Vec::new();
    for i in 0..fields {
        for element in [format!("f{i}"), format!("v{i}")] {
            p.push(0x80 | u8::try_from(element.len()).expect("short element"));
            p.extend_from_slice(element.as_bytes());
            p.push(u8::try_from(element.len() + 1).expect("short element"));
        }
    }
    p
}

/// Incompressible pseudo-random bytes: the literal path runs at EVERY position, so this
/// is the arm's best case and bounds the lever from above.
fn incompressible(len: usize) -> Vec<u8> {
    let mut s: u32 = 0xC0FF_EE00;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

/// Long repeated runs: almost every position is a match, so literal batching should do
/// essentially NOTHING here. This is the guard payload -- a lever that "wins" on this
/// one is measuring noise, not literals.
fn run_heavy(len: usize) -> Vec<u8> {
    let unit = incompressible(64);
    unit.iter().copied().cycle().take(len).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: lzf_literal_ab <batch|push|guard|noguard|tag|xortag|exact|tier> <listpack|random|runs> <reps>");
        std::process::exit(2);
    }
    // slice 2 arms: batch|push. slice 3 arms: guard|noguard (the per-literal-byte
    // budget test, present or deleted). All four run the same driver so one binary
    // can answer both questions.
    let arm = args[1].as_str();
    if !matches!(
        arm,
        "batch" | "push" | "guard" | "noguard" | "tag" | "xortag" | "exact" | "tier"
    ) {
        eprintln!("unknown arm {arm}");
        std::process::exit(2);
    }
    let batch = arm == "batch";
    let slice3 = matches!(arm, "guard" | "noguard");
    // slice 5 arms: tag|xortag -- the packed table's epoch probe.
    let slice5 = matches!(arm, "tag" | "xortag");
    // slice 6 arms: exact|tier -- the match-path budget test.
    let slice6 = matches!(arm, "exact" | "tier");
    let payload = match args[2].as_str() {
        "listpack" => listpack_like(40),
        "random" => incompressible(3000),
        "runs" => run_heavy(4096),
        other => {
            eprintln!("unknown payload {other}");
            std::process::exit(2);
        }
    };
    let reps: usize = args[3].parse().expect("reps");
    let budget = payload.len().saturating_sub(4);

    // Equivalence gate: refuse to report a number for a build whose arms disagree.
    let a = fr_persist::bench_lzf_compress_literals::<false>(&payload, budget);
    let b = fr_persist::bench_lzf_compress_literals::<true>(&payload, budget);
    assert_eq!(a, b, "arms diverged; a speedup here would be meaningless");
    let g = fr_persist::bench_lzf_compress_guard::<true>(&payload, budget);
    let u = fr_persist::bench_lzf_compress_guard::<false>(&payload, budget);
    assert_eq!(g, u, "guard arms diverged; a speedup here would be meaningless");
    let t0 = fr_persist::bench_lzf_compress_xortag::<false>(&payload, budget);
    let t1 = fr_persist::bench_lzf_compress_xortag::<true>(&payload, budget);
    assert_eq!(t0, t1, "xor-tag arms diverged; a speedup here would be meaningless");
    let e0 = fr_persist::bench_lzf_compress_tier::<false>(&payload, budget);
    let e1 = fr_persist::bench_lzf_compress_tier::<true>(&payload, budget);
    assert_eq!(e0, e1, "tier arms diverged; a speedup here would be meaningless");
    println!(
        "arm={} payload={} len={} budget={budget} encoded={:?} reps={reps}",
        args[1],
        args[2],
        payload.len(),
        a.as_ref().map(Vec::len)
    );

    let mut sink = 0usize;
    for _ in 0..reps {
        let out = if slice6 {
            if arm == "exact" {
                fr_persist::bench_lzf_compress_tier::<false>(black_box(&payload), black_box(budget))
            } else {
                fr_persist::bench_lzf_compress_tier::<true>(black_box(&payload), black_box(budget))
            }
        } else if slice5 {
            if arm == "tag" {
                fr_persist::bench_lzf_compress_xortag::<false>(black_box(&payload), black_box(budget))
            } else {
                fr_persist::bench_lzf_compress_xortag::<true>(black_box(&payload), black_box(budget))
            }
        } else if slice3 {
            if arm == "guard" {
                fr_persist::bench_lzf_compress_guard::<true>(black_box(&payload), black_box(budget))
            } else {
                fr_persist::bench_lzf_compress_guard::<false>(black_box(&payload), black_box(budget))
            }
        } else if batch {
            fr_persist::bench_lzf_compress_literals::<true>(black_box(&payload), black_box(budget))
        } else {
            fr_persist::bench_lzf_compress_literals::<false>(black_box(&payload), black_box(budget))
        };
        sink = sink.wrapping_add(black_box(out).map_or(0, |v| v.len()));
    }
    println!("sink={sink}");
}
