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

    // Several live string keys for MGET (no TTL; hits the cache-config collapse).
    for i in 0..8u32 {
        store.set(format!("mk:{i}").into_bytes(), vec![b'v'; 32], None, 1_000);
    }
    let mget_keys: Vec<Vec<u8>> = (0..8u32).map(|i| format!("mk:{i}").into_bytes()).collect();

    // An integer key (no TTL) for the INCR write-path single-lookup collapse.
    store.set(b"counter:key".to_vec(), b"0".to_vec(), None, 1_000);
    // A key for the EXPIRE write-path lazy-drop (TTL refreshed each iter).
    store.set(b"expire:key".to_vec(), b"v".to_vec(), None, 1_000);

    let mut g = c.benchmark_group("store_read");
    g.bench_function("get_string_bytes_ttl_lru_hit", |b| {
        b.iter(|| {
            let got = store
                .get_string_bytes(std::hint::black_box(b"target:key"), 2_000)
                .unwrap();
            std::hint::black_box(got.map(|v| v.len()))
        })
    });
    g.bench_function("mget_8_ttl_lru_hit", |b| {
        let refs: Vec<&[u8]> = mget_keys.iter().map(Vec::as_slice).collect();
        b.iter(|| {
            let got = store.mget(std::hint::black_box(&refs), 2_000);
            std::hint::black_box(got.len())
        })
    });
    g.bench_function("exists_ttl_lru_hit", |b| {
        b.iter(|| std::hint::black_box(store.exists(std::hint::black_box(b"target:key"), 2_000)))
    });
    g.bench_function("strlen_ttl_lru_hit", |b| {
        b.iter(|| std::hint::black_box(store.strlen(std::hint::black_box(b"target:key"), 2_000)))
    });
    g.bench_function("value_type_ttl_lru_hit", |b| {
        b.iter(|| std::hint::black_box(store.value_type(std::hint::black_box(b"target:key"), 2_000)))
    });
    g.bench_function("pttl_ttl_lru_hit", |b| {
        b.iter(|| std::hint::black_box(store.pttl(std::hint::black_box(b"target:key"), 2_000)))
    });
    g.bench_function("getrange_ttl_lru_hit", |b| {
        b.iter(|| {
            std::hint::black_box(store.getrange(std::hint::black_box(b"target:key"), 0, 31, 2_000))
        })
    });
    g.bench_function("getbit_ttl_lru_hit", |b| {
        b.iter(|| {
            std::hint::black_box(store.getbit(std::hint::black_box(b"target:key"), 100, 2_000))
        })
    });
    g.bench_function("get_sort_weight_ttl_lru_hit", |b| {
        b.iter(|| {
            std::hint::black_box(store.get_sort_weight(std::hint::black_box(b"target:key"), 2_000))
        })
    });
    g.bench_function("bitfield_get_ttl_lru_hit", |b| {
        b.iter(|| {
            std::hint::black_box(store.bitfield_get(
                std::hint::black_box(b"target:key"),
                0,
                8,
                false,
                2_000,
            ))
        })
    });
    g.bench_function("incr_no_ttl", |b| {
        b.iter(|| std::hint::black_box(store.incr(std::hint::black_box(b"counter:key"), 2_000)))
    });
    // SETNX on an EXISTING key (contended-lock case): returns false, no mutation —
    // lookup-dominated, exercises the lazy drop_if_expired collapse.
    g.bench_function("setnx_existing_no_ttl", |b| {
        b.iter(|| {
            std::hint::black_box(store.setnx(
                std::hint::black_box(b"target:key"),
                std::hint::black_box(b"x"),
                2_000,
            ))
        })
    });
    g.bench_function("expire_existing", |b| {
        b.iter(|| {
            std::hint::black_box(store.expire_milliseconds(
                std::hint::black_box(b"expire:key"),
                100_000,
                2_000,
            ))
        })
    });
    // EXPIREAT/PEXPIREAT on an existing key: absolute-time sibling, lazy-drop collapse.
    g.bench_function("expireat_existing", |b| {
        b.iter(|| {
            std::hint::black_box(store.expire_at_milliseconds(
                std::hint::black_box(b"expire:key"),
                10_000_000,
                2_000,
            ))
        })
    });
    // PERSIST on a no-TTL key (returns false, no mutation): stable, exercises the
    // lazy-drop + deadline-reuse lookup elision.
    g.bench_function("persist_no_ttl", |b| {
        b.iter(|| std::hint::black_box(store.persist(std::hint::black_box(b"target:key"), 2_000)))
    });
    // EXPIRETIME on a TTL-bearing key: the live-deadline case answers ExpiresAt from ONE
    // `expiry_ms` probe (a key with a future expiry is provably present), collapsing the
    // record_keyspace_lookup probe + the redundant second `expiry_ms`.
    g.bench_function("expiretime_ttl", |b| {
        b.iter(|| {
            std::hint::black_box(store.expiretime_value(std::hint::black_box(b"ttl:sentinel"), 2_000))
        })
    });
    // TOUCH on a live no-TTL key (non-LFU): lazy-drop single `get_mut` access-touch.
    g.bench_function("touch_no_ttl", |b| {
        b.iter(|| std::hint::black_box(store.touch_key(std::hint::black_box(b"target:key"), 2_000)))
    });
    // HDEL of 50 fields on a hashtable with NO field TTLs (the common case): the candidate
    // skips the per-field `hash_field_ttl_clear_for_field` loop (2 Vec allocs + BTree probe
    // each) behind an `is_empty` guard. iter_batched rebuilds the hash each iter (untimed
    // setup), so only the HDEL is measured.
    let hdel_fields: Vec<Vec<u8>> = (0..50u32).map(|i| format!("f{i}").into_bytes()).collect();
    let hdel_field_refs: Vec<&[u8]> = hdel_fields.iter().map(Vec::as_slice).collect();
    let hdel_val: &[u8] = b"v";
    let mut hdel_flat: Vec<&[u8]> = Vec::with_capacity(100);
    for f in &hdel_field_refs {
        hdel_flat.push(f);
        hdel_flat.push(hdel_val);
    }
    g.bench_function("hdel_50_no_fieldttl", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.hset_borrowed_many(b"h", &hdel_flat, 1_000).unwrap();
                s
            },
            |mut s| {
                std::hint::black_box(s.hdel(std::hint::black_box(b"h"), &hdel_field_refs, 2_000))
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();
}

criterion_group!(benches, bench_get);
criterion_main!(benches);
