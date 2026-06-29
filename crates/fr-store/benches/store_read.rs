//! (frankenredis-cc get-ttl-lru-single-lookup) Per-crate A/B for the GET read path on
//! the common CACHE config: TTL-bearing keys present (so the no-TTL fast path is
//! skipped) but LRU policy (LFU off). The candidate collapses the slow path's
//! `record_keyspace_lookup` (drop_if_expired probe) + value `get_mut` double keyspace
//! lookup into ONE `entries.get_mut`. Toggle the `if !self.lfu_tracking_enabled()`
//! branch in `get_string_bytes` off (`if false`) to measure the baseline.

use criterion::{Criterion, criterion_group, criterion_main};
use fr_store::Store;

fn bench_get(c: &mut Criterion) {
    let mut store = Store::new();
    // A TTL-bearing key makes `count_expiring_keys() > 0`, so GETs skip the no-TTL fast
    // path and exercise the TTL+LRU branch under test (default policy is LRU = LFU off).
    store.set(b"ttl:sentinel".to_vec(), b"x".to_vec(), Some(10_000_000), 1_000);
    // The live target (no TTL): a hit on the read path.
    store.set(b"target:key".to_vec(), vec![b'v'; 64], None, 1_000);

    let mut g = c.benchmark_group("store_read");
    g.bench_function("get_string_bytes_ttl_lru_hit", |b| {
        b.iter(|| {
            let got = store
                .get_string_bytes(std::hint::black_box(b"target:key"), 2_000)
                .unwrap();
            std::hint::black_box(got.map(|v| v.len()))
        })
    });
    g.finish();
}

criterion_group!(benches, bench_get);
criterion_main!(benches);
