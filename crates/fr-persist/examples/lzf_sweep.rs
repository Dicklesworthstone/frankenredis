//! Price every `lzf_compress_dispatch` instantiation on ONE payload.
//!
//! The seven slices each landed against the baseline that existed when it was
//! written, and each bench hook pins the OTHER six to whatever those baselines
//! were -- so no measurement has ever compared the shipping tuple
//! `<true, true, false, false, true, true, true>` against its neighbours on the
//! payload shape that actually dominates today (stream macro-node listpacks).
//!
//!     lzf_sweep list                -> index, name for each combo
//!     lzf_sweep <idx> <bytes> <reps>
//!
//! One combo per process, so cost is the process Ir total and stays exact even
//! if LLVM folds two identical bodies to one symbol.

use std::hint::black_box;

type F = fn(&[u8], usize) -> Option<Vec<u8>>;

/// The shipping tuple and its six ONE-FLAG neighbours. A single flag at a time is
/// the comparison each slice was originally argued from, and the only one whose
/// result attributes to a specific slice.
static TABLE: [(&str, F); 7] = [
    (
        "SHIPPING  H1 B0 G0 X1 T1 W1",
        bench::<true, true, false, false, true, true, true>,
    ),
    (
        "HOIST=0   H0 B0 G0 X1 T1 W1",
        bench::<true, false, false, false, true, true, true>,
    ),
    (
        "BATCH=1   H1 B1 G0 X1 T1 W1",
        bench::<true, true, true, false, true, true, true>,
    ),
    (
        "GUARD=1   H1 B0 G1 X1 T1 W1",
        bench::<true, true, false, true, true, true, true>,
    ),
    (
        "XORTAG=0  H1 B0 G0 X0 T1 W1",
        bench::<true, true, false, false, false, true, true>,
    ),
    (
        "TIER=0    H1 B0 G0 X1 T0 W1",
        bench::<true, true, false, false, true, false, true>,
    ),
    (
        "WIDETAG=0 H1 B0 G0 X1 T1 W0",
        bench::<true, true, false, false, true, true, false>,
    ),
];

use fr_persist::bench_lzf_compress_tuple as bench;

/// Same generator as `lzf_hot`: a stream macro-node listpack, mostly small
/// integers with short field/value strings, bounded by stream-node-max-bytes.
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

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| "list".into());
    if first == "list" {
        for (i, (name, _)) in TABLE.iter().enumerate() {
            println!("{i}\t{name}");
        }
        return;
    }
    let idx: usize = first.parse().expect("index");
    let bytes: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(4096);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2000);

    let payload = stream_node_like(bytes);
    let budget = payload.len().saturating_sub(4);
    let (name, f) = TABLE[idx];

    // BYTE PARITY FIRST. All seven parameters change only HOW the match search
    // runs, never what is emitted; a combo whose output differs from the shipping
    // tuple is a bug, and its Ir would be measuring a different compressor.
    let shipping = TABLE[0].1;
    let want = shipping(&payload, budget);
    let got = f(&payload, budget);
    assert_eq!(
        want, got,
        "combo {idx} ({name}) is not byte-identical to shipping"
    );
    let out_len = got.as_ref().map_or(0, Vec::len);

    let mut acc = 0usize;
    for _ in 0..reps {
        let out = f(black_box(&payload), budget).expect("compresses");
        acc += black_box(out).len();
    }
    println!(
        "{idx}\t{name}\tin={} out={out_len} reps={reps} acc={acc}",
        payload.len()
    );
}
