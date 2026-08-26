//! Instruction-level probe for `listpack::decode_raw_values`.
//!
//! It is the largest fr-only frame on the worst arm (stream RESTORE): 10,810 Ir
//! per op, 17.9 pct, decoding ~246 elements of one stream macro-node listpack at
//! **44 instructions per element**. Redis walks the same listpack with
//! `lpGet`/`lpNext` for roughly 20. That gap is on work BOTH engines do, so unlike
//! the decode-vs-store-verbatim asymmetry it is a real lever if the instructions
//! are actually recoverable.
//!
//!     lp_decode_hot <bytes> <reps>
//!
//! Deterministic and load-immune: one payload, no clock, no allocator churn beyond
//! the decoder's own. Run under `callgrind --dump-instr=yes` at two rep counts and
//! difference the per-address costs, the way `lzf_hot` did for the compressor.

use std::hint::black_box;

/// The same stream macro-node shape the RESTORE arm decodes: mostly small
/// integers (flags, ms/seq deltas, field counts, lp_count) with short strings for
/// field names and values. The integer/string MIX is what decides which arms of
/// the decoder's dispatch chain run.
fn stream_node_like(target_bytes: usize) -> Vec<u8> {
    let mut p: Vec<u8> = vec![0u8; 6];
    let mut i: u32 = 0;
    while p.len() < target_bytes {
        for v in [2u8, (i % 100) as u8, 1, 2] {
            p.push(v & 0x7F);
            p.push(1);
        }
        for s in [format!("f{}", i % 8), format!("v{i:04}")] {
            p.push(0x80 | u8::try_from(s.len()).expect("short"));
            p.extend_from_slice(s.as_bytes());
            p.push(u8::try_from(s.len() + 1).expect("short"));
        }
        p.push(6);
        p.push(1);
        i += 1;
    }
    p.push(0xFF);
    p
}

/// A listpack header the decoder will accept: total bytes then element count.
fn finish_header(p: &mut [u8], elements: usize) {
    let total = u32::try_from(p.len()).expect("fits");
    p[0..4].copy_from_slice(&total.to_le_bytes());
    let n = u16::try_from(elements).unwrap_or(u16::MAX);
    p[4..6].copy_from_slice(&n.to_le_bytes());
}

fn main() {
    let mut args = std::env::args().skip(1);
    let bytes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(4096);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2000);

    let mut payload = stream_node_like(bytes);
    // Count elements by decoding once with an UNKNOWN count, then stamp the exact
    // count so the decoder takes its pre-sized path -- which is what the real
    // RESTORE payload has.
    finish_header(&mut payload, usize::from(u16::MAX));
    let probe = fr_persist::listpack::decode_raw_values(&payload)
        .expect("generated listpack must decode");
    let elements = probe.len();
    finish_header(&mut payload, elements);
    let probe = fr_persist::listpack::decode_raw_values(&payload)
        .expect("listpack with an exact count must decode");
    assert_eq!(probe.len(), elements, "element count must survive the stamp");

    let mut acc = 0usize;
    for _ in 0..reps {
        let values = fr_persist::listpack::decode_raw_values(black_box(&payload))
            .expect("decodes");
        acc += black_box(values).len();
    }
    println!(
        "bytes={} elements={} bytes_per_element={:.1} reps={reps} acc={acc}",
        payload.len(),
        elements,
        payload.len() as f64 / elements as f64,
    );
}
