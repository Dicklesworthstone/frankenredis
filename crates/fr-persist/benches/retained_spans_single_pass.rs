//! Same-binary A/B for `decode_retained_listpack_spans`: TWO-PASS vs SINGLE-PASS.
//!
//! (frankenredis-gvm6z) WHY THIS IS INSTRUCTION-COUNTED AND NOT TIMED. The other
//! same-binary A/Bs in this crate (`value_spans_presize`, `listpack_backlen`)
//! interleave arms and take a median of wall-clock ratios. That substrate needs a
//! host whose noise is smaller than the effect; this one was written on a host at
//! loadavg 656, where it is not. Retired instructions are load-immune -- the
//! repo's instrument audit bounded a 34 pct MHz swing at 0.64 pct on instr/op --
//! so this binary does no timing at all. It runs ONE arm for a caller-given
//! iteration count and prints a checksum; the driver runs it under Callgrind at N
//! and 2N and differences the totals, so process startup, the fixture build and
//! teardown cancel exactly and what is left is per-decode work.
//!
//! Both arms are in THIS binary, per docs/BENCH_METHODOLOGY section 3: `rch exec`
//! picks workers non-deterministically and an ORIG/CAND ratio taken across two
//! builds is not worker-invariant.
//!
//!   ORIG = `bench_decode_retained_spans_two_pass`  (decode generic spans, convert)
//!   CAND = `decode_retained_listpack_spans`        (one walk into the retained form)
//!
//! Usage: retained_spans_single_pass <two_pass|single_pass|verify> <entries> <iters> <shape>
//!   shape = strings | integers | mixed
//!
//! `verify` asserts the two arms agree on all three shapes and exits; run it once
//! per build so a "faster" arm that decodes something else cannot be reported.

use std::hint::black_box;

use fr_persist::encode_listpack_strings_blob;
use fr_persist::listpack::{
    RetainedListpackSpans, bench_decode_retained_spans_two_pass, decode_retained_listpack_spans,
};

/// LETTER-LEADING on purpose for the string shape: a digit-leading payload is
/// integer-encoded by the listpack encoder and would measure the other arm of the
/// decoder (the same trap `edcbf8b66` documents for the LIST RESTORE workload).
fn blob(entries: usize, shape: &str) -> Vec<u8> {
    let owned: Vec<Vec<u8>> = (0..entries)
        .map(|i| match shape {
            "strings" => format!("v{i:06}").into_bytes(),
            "integers" => format!("{}", (i as i64) - (entries as i64 / 2)).into_bytes(),
            // Alternating, so the arena offsets advance non-trivially rather than
            // staying empty or staying dense.
            _ => {
                if i % 2 == 0 {
                    format!("v{i:06}").into_bytes()
                } else {
                    format!("{}", (i as i64) - (entries as i64 / 2)).into_bytes()
                }
            }
        })
        .collect();
    let refs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
    encode_listpack_strings_blob(&refs).expect("fixture encodes")
}

/// Consume enough of the result that neither arm can be optimised away, and do it
/// identically for both so the fold is not itself an arm difference.
fn consume(spans: &RetainedListpackSpans, payload: &[u8]) -> usize {
    let mut acc = spans.entries().len();
    for span in spans.entries() {
        acc = acc.wrapping_add(span.as_bytes(payload, spans.integer_bytes()).len());
    }
    acc
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "usage: {} <two_pass|single_pass|verify> <entries> <iters> <strings|integers|mixed>",
            args[0]
        );
        std::process::exit(2);
    }
    let arm = args[1].as_str();
    let entries: usize = args[2].parse().expect("entries");
    let iters: usize = args[3].parse().expect("iters");
    let shape = args[4].as_str();

    if arm == "verify" {
        for shape in ["strings", "integers", "mixed"] {
            for n in [1usize, 2, 3, 16, 200, 512] {
                let payload = blob(n, shape);
                let a = bench_decode_retained_spans_two_pass(&payload).expect("two-pass decodes");
                let b = decode_retained_listpack_spans(&payload).expect("single-pass decodes");
                assert_eq!(
                    a.entries(),
                    b.entries(),
                    "{shape}/{n}: retained spans diverged between arms"
                );
                assert_eq!(
                    a.integer_bytes(),
                    b.integer_bytes(),
                    "{shape}/{n}: integer arena diverged between arms"
                );
                let (va, vb) = (consume(&a, &payload), consume(&b, &payload));
                assert_eq!(va, vb, "{shape}/{n}: decoded values diverged between arms");
            }
        }
        println!("VERIFY_OK both arms agree on strings/integers/mixed at n=1,2,3,16,200,512");
        return;
    }

    let payload = blob(entries, shape);
    let mut acc = 0usize;
    match arm {
        "two_pass" => {
            for _ in 0..iters {
                let spans = bench_decode_retained_spans_two_pass(black_box(payload.as_slice()))
                    .expect("two-pass decodes");
                acc = acc.wrapping_add(consume(black_box(&spans), &payload));
            }
        }
        "single_pass" => {
            for _ in 0..iters {
                let spans = decode_retained_listpack_spans(black_box(payload.as_slice()))
                    .expect("single-pass decodes");
                acc = acc.wrapping_add(consume(black_box(&spans), &payload));
            }
        }
        other => {
            eprintln!("unknown arm {other}");
            std::process::exit(2);
        }
    }
    println!(
        "{arm} {shape} n={entries} iters={iters} checksum={}",
        black_box(acc)
    );
}
