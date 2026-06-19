//! RDB encode+decode criterion bench (frankenredis VERIFY phase, cc).
//!
//! Server-free, degradation-immune measurement of the full RDB codec via the pub
//! `encode_rdb` / `decode_rdb` entry points. This is the stable A/B harness for the
//! "code-first batch-test pending" encode levers (collection-blob + quicklist-node presizes,
//! zset listpack direct-emit, intset in-place sort) and the decode-string-move levers
//! (knzdi listpack, ta8s1 quicklist2). Unlike the end-to-end DEBUG-RELOAD head-to-head,
//! criterion handles noise statistically and needs no server, so it does not suffer the
//! long-session sandbox-server degradation. To A/B a lever: toggle it, `cargo bench -p
//! fr-persist`, compare criterion's reported change.
//!
//! NOTE: this measures fr's ABSOLUTE encode/decode speed (for lever A/B), not a head-to-head
//! ratio vs redis (criterion is in-process, fr-only) — the vs-redis ratios live in
//! docs/RELEASE_READINESS_SCORECARD.md (DEBUG RELOAD).

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fr_persist::{decode_rdb, encode_rdb, RdbEntry, RdbValue};

fn build_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::new();
    let mut key = 0usize;
    let mut k = || {
        key += 1;
        format!("k{key:06}").into_bytes()
    };
    // listpack hashes (40 int+str fields)
    for _ in 0..400 {
        let fields = (0..40)
            .map(|j| (format!("f{j}").into_bytes(), format!("v{j}").into_bytes()))
            .collect();
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::Hash(fields), expire_ms: None });
    }
    // listpack zsets (40 members, mixed scores)
    for _ in 0..400 {
        let members = (0..40)
            .map(|j| (format!("m{j}").into_bytes(), j as f64 * 1.5))
            .collect();
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::SortedSet(members), expire_ms: None });
    }
    // listpack sets + an intset
    for _ in 0..400 {
        let m = (0..40).map(|j| format!("m{j}").into_bytes()).collect();
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::Set(m), expire_ms: None });
    }
    for _ in 0..400 {
        let m = (0..40).map(|j| j.to_string().into_bytes()).collect();
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::Set(m), expire_ms: None });
    }
    // quicklist lists (>8KiB => multi-node)
    for _ in 0..300 {
        let l = (0..60).map(|j| format!("e{j:02}{}", "x".repeat(40)).into_bytes()).collect();
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::List(l), expire_ms: None });
    }
    // int-bearing strings (087qq itoa2 path)
    for _ in 0..4000 {
        entries.push(RdbEntry { db: 0, key: k(), value: RdbValue::String((key as i64 * 7919).to_string().into_bytes()), expire_ms: None });
    }
    entries
}

fn bench_codec(c: &mut Criterion) {
    let entries = build_entries();
    let encoded = encode_rdb(&entries, &[]);
    let n = entries.len() as u64;

    let mut g = c.benchmark_group("rdb_codec");
    g.throughput(Throughput::Elements(n));
    g.bench_function("encode_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&entries), &[]))
    });
    g.bench_function("decode_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&encoded)).unwrap())
    });
    g.finish();
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
