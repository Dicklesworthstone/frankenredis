//! (frankenredis-cc get-ttl-lru-single-lookup) Per-crate A/B for the GET read path on
//! the common CACHE config: TTL-bearing keys present (so the no-TTL fast path is
//! skipped) but LRU policy (LFU off). The candidate collapses the slow path's
//! `record_keyspace_lookup` (drop_if_expired probe) + value `get_mut` double keyspace
//! lookup into ONE `entries.get_mut`. Toggle the `if !self.lfu_tracking_enabled()`
//! branch in `get_string_bytes` off (`if false`) to measure the baseline.

use criterion::{Criterion, criterion_group, criterion_main};
use fr_store::{MaxmemoryPolicy, Store};

fn bench_get(c: &mut Criterion) {
    let mut store = Store::new();
    // A TTL-bearing key makes `count_expiring_keys() > 0`, so GETs skip the no-TTL fast
    // path and exercise the TTL+LRU branch under test (default policy is LRU = LFU off).
    store.set(
        b"ttl:sentinel".to_vec(),
        b"x".to_vec(),
        Some(10_000_000),
        1_000,
    );
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
    // A hash with a few fields and NO per-field TTLs, for the HGET field-expiry-check
    // allocation-elision guard (hash_field_is_expired fast-exits on an empty TTL map).
    {
        let pairs: &[&[u8]] = &[b"f0", b"v0", b"f1", b"v1", b"f2", b"v2"];
        store.hset_borrowed_many(b"hh", pairs, 1_000).unwrap();
    }
    // Stream group with consumers, for the XINFO CONSUMERS single-lookup collapse.
    store
        .xadd(b"xic", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 1_000)
        .unwrap();
    store
        .xgroup_create(b"xic", b"g", (0, 0), false, 1_000)
        .unwrap();
    store
        .xgroup_createconsumer(b"xic", b"g", b"c1", 1_001)
        .unwrap();
    store
        .xgroup_createconsumer(b"xic", b"g", b"c2", 1_002)
        .unwrap();

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
        b.iter(|| {
            std::hint::black_box(store.value_type(std::hint::black_box(b"target:key"), 2_000))
        })
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
            std::hint::black_box(
                store.expiretime_value(std::hint::black_box(b"ttl:sentinel"), 2_000),
            )
        })
    });
    // TOUCH on a live no-TTL key (non-LFU): lazy-drop single `get_mut` access-touch.
    g.bench_function("touch_no_ttl", |b| {
        b.iter(|| std::hint::black_box(store.touch_key(std::hint::black_box(b"target:key"), 2_000)))
    });
    // GETSET returning a large old string: owned path clones the 4 KiB old value before
    // reply encoding; borrowed path streams the same old bytes through a callback and
    // only materializes the new value being stored.
    let getset_old_value = vec![b'x'; 4096];
    let getset_new_value = vec![b'y'; 4096];
    g.bench_function("getset_4096_old_clone", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.set(b"gs".to_vec(), getset_old_value.clone(), None, 1_000);
                s
            },
            |mut s| {
                let old = s
                    .getset(
                        std::hint::black_box(b"gs".to_vec()),
                        std::hint::black_box(getset_new_value.as_slice()),
                        2_000,
                    )
                    .unwrap();
                std::hint::black_box(old.as_ref().map_or(0, Vec::len))
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.bench_function("getset_4096_old_borrow", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.set(b"gs".to_vec(), getset_old_value.clone(), None, 1_000);
                s
            },
            |mut s| {
                let mut old_len = 0usize;
                s.getset_with(
                    std::hint::black_box(b"gs"),
                    std::hint::black_box(getset_new_value.as_slice()),
                    2_000,
                    |old| old_len = old.map_or(0, <[u8]>::len),
                )
                .unwrap();
                std::hint::black_box(old_len)
            },
            criterion::BatchSize::SmallInput,
        )
    });
    // HGET on a hash field with NO per-field TTLs (the common case): the candidate elides
    // the 2-Vec composite alloc + BTree probe in hash_field_is_expired.
    g.bench_function("hget_no_fieldttl", |b| {
        b.iter(|| {
            std::hint::black_box(store.hget(
                std::hint::black_box(b"hh"),
                std::hint::black_box(b"f0"),
                2_000,
            ))
        })
    });
    g.bench_function("xinfo_consumers_no_ttl", |b| {
        b.iter(|| {
            std::hint::black_box(store.xinfo_consumers(
                std::hint::black_box(b"xic"),
                std::hint::black_box(b"g"),
                2_000,
            ))
        })
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

    // (cc_fr) LPOP count popping ALL of a 120-element PACKED list: the batch `pop_front_n` does one
    // `buf.drain` instead of 120 per-element shifts (the old `pop_front` loop was O(count*len)).
    // iter_batched rebuilds the list each iter (untimed setup), so only the LPOP is measured.
    let lpop_members: Vec<Vec<u8>> = (0..120u32).map(|i| format!("elem:{i:05}").into_bytes()).collect();
    g.bench_function("lpop_count_all_120_packed", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.rpush(b"l", &lpop_members, 1_000).unwrap();
                s
            },
            |mut s| std::hint::black_box(s.lpop_count(std::hint::black_box(b"l"), 120, 2_000)),
            criterion::BatchSize::SmallInput,
        )
    });

    // (cc_fr) RPOP count popping ALL of a 120-element PACKED list: the batch `pop_back_n` scans+
    // truncates ONCE instead of 120 `pop_back`s that each re-scan from the front (O(count*len)).
    g.bench_function("rpop_count_all_120_packed", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.rpush(b"l", &lpop_members, 1_000).unwrap();
                s
            },
            |mut s| std::hint::black_box(s.rpop_count(std::hint::black_box(b"l"), 120, 2_000)),
            criterion::BatchSize::SmallInput,
        )
    });

    // (cc_fr) LTRIM a 120-element PACKED list to the middle window [40, 79]: removes 40 off the
    // front + 40 off the back. The batch does one drain + one scan+truncate instead of 40 front
    // shifts + 40 back front-scans (the old two loops were O((s+back)*len)).
    g.bench_function("ltrim_middle_120_packed", |b| {
        b.iter_batched(
            || {
                let mut s = Store::new();
                s.rpush(b"l", &lpop_members, 1_000).unwrap();
                s
            },
            |mut s| std::hint::black_box(s.ltrim(std::hint::black_box(b"l"), 40, 79, 2_000)),
            criterion::BatchSize::SmallInput,
        )
    });

    // Existing-key variadic HSET into a packed hash: ORIG is the old
    // per-field packed-map insert loop (K repeated listpack scans). The
    // candidate builds a transient borrowed overlay and rebuilds the packed map
    // once, preserving existing-field order, new-field append order, and
    // duplicate-field last-wins semantics.
    let existing_hset_fields: Vec<Vec<u8>> = (0..96u32)
        .map(|i| format!("hf{i:03}").into_bytes())
        .collect();
    let existing_hset_values: Vec<Vec<u8>> = (0..96u32)
        .map(|i| format!("hv{i:03}").into_bytes())
        .collect();
    let mut existing_hset_seed = Vec::with_capacity(existing_hset_fields.len() * 2);
    for (field, value) in existing_hset_fields.iter().zip(&existing_hset_values) {
        existing_hset_seed.push(field.as_slice());
        existing_hset_seed.push(value.as_slice());
    }
    let update_hset_fields: Vec<Vec<u8>> = (0..48u32)
        .map(|i| match i % 4 {
            0 => existing_hset_fields[((i * 7) as usize) % existing_hset_fields.len()].clone(),
            1 => format!("hn{:03}", i % 18).into_bytes(),
            2 => existing_hset_fields[((i * 13) as usize) % existing_hset_fields.len()].clone(),
            _ => format!("hn{:03}", i % 18).into_bytes(),
        })
        .collect();
    let update_hset_values: Vec<Vec<u8>> = (0..48u32)
        .map(|i| format!("hu{i:03}").into_bytes())
        .collect();
    let mut existing_hset_update = Vec::with_capacity(update_hset_fields.len() * 2);
    for (field, value) in update_hset_fields.iter().zip(&update_hset_values) {
        existing_hset_update.push(field.as_slice());
        existing_hset_update.push(value.as_slice());
    }
    let make_existing_hset_store = || {
        let mut s = Store::new();
        s.hset_borrowed_many(b"hhb", &existing_hset_seed, 1_000)
            .unwrap();
        s
    };
    g.bench_function("hset_existing_packed_orig_loop_96x48", |b| {
        b.iter_batched(
            make_existing_hset_store,
            |mut s| {
                std::hint::black_box(
                    s.hset_borrowed_many_existing_loop_for_bench(
                        std::hint::black_box(b"hhb"),
                        std::hint::black_box(&existing_hset_update),
                        2_000,
                    )
                    .unwrap(),
                )
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.bench_function("hset_existing_packed_overlay_96x48", |b| {
        b.iter_batched(
            make_existing_hset_store,
            |mut s| {
                std::hint::black_box(
                    s.hset_borrowed_many(
                        std::hint::black_box(b"hhb"),
                        std::hint::black_box(&existing_hset_update),
                        2_000,
                    )
                    .unwrap(),
                )
            },
            criterion::BatchSize::SmallInput,
        )
    });
    let make_existing_hset_lfu_store = || {
        let mut s = Store::new();
        s.hset_borrowed_many(b"hhb", &existing_hset_seed, 1_000)
            .unwrap();
        s.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
        s.lfu_decay_time = 0;
        s
    };
    g.bench_function("hset_lfu_existing_packed_orig_loop_96x48", |b| {
        b.iter_batched(
            make_existing_hset_lfu_store,
            |mut s| {
                std::hint::black_box(
                    s.hset_borrowed_many_existing_loop_for_bench(
                        std::hint::black_box(b"hhb"),
                        std::hint::black_box(&existing_hset_update),
                        2_000,
                    )
                    .unwrap(),
                )
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.bench_function("hset_lfu_existing_packed_batch_96x48", |b| {
        b.iter_batched(
            make_existing_hset_lfu_store,
            |mut s| {
                std::hint::black_box(
                    s.hset_borrowed_many(
                        std::hint::black_box(b"hhb"),
                        std::hint::black_box(&existing_hset_update),
                        2_000,
                    )
                    .unwrap(),
                )
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();
}

// (frankenredis-zrange-into) Per-crate A/B for the ZRANGE ... WITHSCORES reply
// build: the owned `zrange_withscores` clones every member into
// `Vec<(Vec<u8>, f64)>`, while `zrange_withscores_borrow_scan` streams each
// (member, score) with the member borrowed (zero per-member alloc). Both sinks do
// identical work (sum member lengths + scores) so the delta is purely the clone.
fn bench_zrange_withscores(c: &mut Criterion) {
    use fr_store::{SmembersScanEvent, ZRangeWithScoresScanEvent};
    const N: usize = 1_000;
    const MEMBER_LEN: usize = 32;
    let mut store = Store::new();
    for i in 0..N {
        let mut m = vec![b'm'; MEMBER_LEN];
        m[0..8].copy_from_slice(&(i as u64).to_be_bytes());
        store.zadd(b"z", &[(i as f64, m)], 1_000).unwrap();
    }

    let mut g = c.benchmark_group("zrange_withscores");
    // OWNED clone path (what the reply built before this lever).
    g.bench_function("clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrange_withscores(std::hint::black_box(b"z"), 0, -1, 2_000)
                .unwrap();
            let mut acc = 0usize;
            for (m, s) in &pairs {
                acc += m.len() + (*s as usize & 1);
            }
            std::hint::black_box(acc)
        })
    });
    // BORROW scan path (streamed, no per-member alloc).
    g.bench_function("borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrange_withscores_borrow_scan(std::hint::black_box(b"z"), 0, -1, 2_000, |ev| {
                    if let ZRangeWithScoresScanEvent::Pair(m, s) = ev {
                        acc += m.len() + (s as usize & 1);
                    }
                })
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // Descending twin: ZREVRANGE ... WITHSCORES clone vs borrow.
    g.bench_function("rev_clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrevrange_withscores(std::hint::black_box(b"z"), 0, -1, 2_000)
                .unwrap();
            let mut acc = 0usize;
            for (m, s) in &pairs {
                acc += m.len() + (*s as usize & 1);
            }
            std::hint::black_box(acc)
        })
    });
    g.bench_function("rev_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrevrange_withscores_borrow_scan(std::hint::black_box(b"z"), 0, -1, 2_000, |ev| {
                    if let ZRangeWithScoresScanEvent::Pair(m, s) = ev {
                        acc += m.len() + (s as usize & 1);
                    }
                })
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // ZRANGEBYSCORE -inf +inf WITHSCORES (whole set by score): clone vs borrow.
    let smin = fr_store::ScoreBound::Inclusive(f64::NEG_INFINITY);
    let smax = fr_store::ScoreBound::Inclusive(f64::INFINITY);
    g.bench_function("byscore_clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrangebyscore_withscores_limited(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    false,
                    0,
                    None,
                    2_000,
                )
                .unwrap();
            let mut acc = 0usize;
            for (m, s) in &pairs {
                acc += m.len() + (*s as usize & 1);
            }
            std::hint::black_box(acc)
        })
    });
    g.bench_function("byscore_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrangebyscore_withscores_borrow_scan(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    2_000,
                    |ev| {
                        if let ZRangeWithScoresScanEvent::Pair(m, s) = ev {
                            acc += m.len() + (s as usize & 1);
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // ZREVRANGEBYSCORE +inf -inf WITHSCORES (descending score walk): clone vs borrow.
    g.bench_function("revbyscore_clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrangebyscore_withscores_limited(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    true,
                    0,
                    None,
                    2_000,
                )
                .unwrap();
            let mut acc = 0usize;
            for (m, s) in &pairs {
                acc += m.len() + (*s as usize & 1);
            }
            std::hint::black_box(acc)
        })
    });
    g.bench_function("revbyscore_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrevrangebyscore_withscores_borrow_scan(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    2_000,
                    |ev| {
                        if let ZRangeWithScoresScanEvent::Pair(m, s) = ev {
                            acc += m.len() + (s as usize & 1);
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // Plain (member-only) ZRANGEBYSCORE: what the reply builds today (clone members,
    // drop scores) vs the member-only borrow scan.
    g.bench_function("byscore_members_clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrangebyscore_withscores_limited(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    false,
                    0,
                    None,
                    2_000,
                )
                .unwrap();
            let members: Vec<Vec<u8>> = pairs.into_iter().map(|(m, _)| m).collect();
            std::hint::black_box(members.iter().map(Vec::len).sum::<usize>())
        })
    });
    g.bench_function("byscore_members_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrangebyscore_members_borrow_scan(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    false,
                    2_000,
                    |ev| {
                        if let SmembersScanEvent::Member(m) = ev {
                            acc += m.len();
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // ZRANGEBYSCORE -inf +inf LIMIT 0 200 (paginated member-only): clone vs borrow.
    g.bench_function("byscore_limit_clone_full_range", |b| {
        b.iter(|| {
            let pairs = store
                .zrangebyscore_withscores_limited(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    false,
                    0,
                    Some(200),
                    2_000,
                )
                .unwrap();
            let members: Vec<Vec<u8>> = pairs.into_iter().map(|(m, _)| m).collect();
            std::hint::black_box(members.iter().map(Vec::len).sum::<usize>())
        })
    });
    g.bench_function("byscore_limit_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrangebyscore_members_limit_borrow_scan(
                    std::hint::black_box(b"z"),
                    smin,
                    smax,
                    false,
                    0,
                    200,
                    2_000,
                    |ev| {
                        if let SmembersScanEvent::Member(m) = ev {
                            acc += m.len();
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // ZRANGEBYLEX - + LIMIT 0 200 (paginated lex): clone vs member-only borrow.
    g.bench_function("bylex_limit_clone_full_range", |b| {
        b.iter(|| {
            let members = store
                .zrangebylex_limited(
                    std::hint::black_box(b"z"),
                    b"-",
                    b"+",
                    false,
                    0,
                    Some(200),
                    2_000,
                )
                .unwrap();
            std::hint::black_box(members.iter().map(Vec::len).sum::<usize>())
        })
    });
    g.bench_function("bylex_limit_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrangebylex_members_limit_borrow_scan(
                    std::hint::black_box(b"z"),
                    b"-",
                    b"+",
                    false,
                    0,
                    200,
                    2_000,
                    |ev| {
                        if let SmembersScanEvent::Member(m) = ev {
                            acc += m.len();
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    // ZRANGEBYLEX - + (whole lex range): clone vs member-only borrow.
    g.bench_function("bylex_members_clone_full_range", |b| {
        b.iter(|| {
            let members = store
                .zrangebylex(std::hint::black_box(b"z"), b"-", b"+", 2_000)
                .unwrap();
            std::hint::black_box(members.iter().map(Vec::len).sum::<usize>())
        })
    });
    g.bench_function("bylex_members_borrow_full_range", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zrangebylex_members_borrow_scan(
                    std::hint::black_box(b"z"),
                    b"-",
                    b"+",
                    false,
                    2_000,
                    |ev| {
                        if let SmembersScanEvent::Member(m) = ev {
                            acc += m.len();
                        }
                    },
                )
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    g.finish();
}

fn bench_restore_zset_listpack(c: &mut Criterion) {
    const N: usize = 96;
    let mut source = Store::new();
    for i in 0..N {
        let member = format!("member:{i:03}:restore-zset-lp").into_bytes();
        let score = if i % 3 == 0 {
            i as f64
        } else {
            (i as f64 * 1.5) + 0.25
        };
        source.zadd(b"z", &[(score, member)], 1_000).unwrap();
    }
    let payload = source.dump_key(b"z", 2_000).expect("zset dump payload");
    assert_eq!(
        payload.first().copied(),
        Some(17),
        "bench payload must stay RDB_TYPE_ZSET_LISTPACK"
    );

    let mut g = c.benchmark_group("restore_zset_listpack");
    g.bench_function("restore_96_members", |b| {
        b.iter_batched(
            Store::new,
            |mut store| {
                store
                    .restore_key(
                        std::hint::black_box(b"z"),
                        0,
                        std::hint::black_box(&payload),
                        false,
                        3_000,
                    )
                    .unwrap();
                std::hint::black_box(store.zcard(b"z", 3_000).unwrap())
            },
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();
}

fn bench_hrandfield_withvalues(c: &mut Criterion) {
    use fr_store::HrandfieldWithValuesScanEvent;

    const N: usize = 2_000;
    const VALUE_LEN: usize = 32;
    let mut store = Store::new();
    for i in 0..N {
        let mut value = vec![b'v'; VALUE_LEN];
        value[0..8].copy_from_slice(&(i as u64).to_be_bytes());
        store
            .hset(b"h", format!("f{i:04}").into_bytes(), value, 1_000)
            .unwrap();
    }

    let mut g = c.benchmark_group("hrandfield_withvalues");
    g.bench_function("count50_clone_pairs", |b| {
        b.iter(|| {
            let pairs = store
                .hrandfield_count(std::hint::black_box(b"h"), 50, 2_000)
                .unwrap();
            std::hint::black_box(
                pairs
                    .iter()
                    .map(|(field, value)| field.len() + value.len())
                    .sum::<usize>(),
            )
        })
    });
    g.bench_function("count50_borrow_pairs", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .hrandfield_count_pair_borrow_scan(std::hint::black_box(b"h"), 50, 2_000, |ev| {
                    if let HrandfieldWithValuesScanEvent::Pair(field, value) = ev {
                        acc += field.len() + value.len();
                    }
                })
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    g.finish();
}

fn bench_zscan0_borrow(c: &mut Criterion) {
    use fr_store::ZscanReplyEvent;

    const N: usize = 2_000;
    let mut store = Store::new();
    for i in 0..N {
        store
            .zadd(b"z", &[(i as f64, format!("m{i:04}").into_bytes())], 1_000)
            .unwrap();
    }

    let mut g = c.benchmark_group("zscan0");
    g.bench_function("count10_clone_pairs", |b| {
        b.iter(|| {
            let pairs = store
                .zscan(std::hint::black_box(b"z"), 0, None, 10, 2_000)
                .unwrap();
            std::hint::black_box(
                pairs
                    .1
                    .iter()
                    .map(|(member, score)| member.len() + (*score as usize & 1))
                    .sum::<usize>(),
            )
        })
    });
    g.bench_function("count10_borrow_pairs", |b| {
        b.iter(|| {
            let mut acc = 0usize;
            store
                .zscan0_borrow_scan(std::hint::black_box(b"z"), 0, None, 10, 2_000, |ev| {
                    if let ZscanReplyEvent::Pair(member, score) = ev {
                        acc += member.len() + (score as usize & 1);
                    }
                })
                .unwrap();
            std::hint::black_box(acc)
        })
    });
    g.finish();
}

fn bench_zlexcount_borrowed_bounds(c: &mut Criterion) {
    const N: usize = 3_000;
    let mut store = Store::new();
    for i in 0..N {
        store
            .zadd(b"z", &[(0.0, format!("m{i:04}").into_bytes())], 1_000)
            .unwrap();
    }
    let _ = store.zrank(b"z", b"m1500", 2_000).unwrap();

    let mut g = c.benchmark_group("zlexcount_borrowed_bounds");
    g.bench_function("warm_rank_tree", |b| {
        b.iter(|| {
            std::hint::black_box(
                store
                    .zlexcount(
                        std::hint::black_box(b"z"),
                        std::hint::black_box(b"[m0700"),
                        std::hint::black_box(b"[m2300"),
                        2_000,
                    )
                    .unwrap(),
            )
        })
    });
    g.finish();
}

fn ttl_clone_each_best(keys: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut best_key: Option<Vec<u8>> = None;
    let mut best_ttl = u64::MAX;
    for key in keys {
        let expires_at_ms = 10_000;
        if expires_at_ms < best_ttl
            || (expires_at_ms == best_ttl && best_key.as_ref().is_none_or(|b| key < b))
        {
            best_ttl = expires_at_ms;
            best_key = Some(key.clone());
        }
    }
    best_key
}

fn ttl_defer_winner_clone(keys: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut best_idx: Option<usize> = None;
    let mut best_ttl = u64::MAX;
    for (i, key) in keys.iter().enumerate() {
        let expires_at_ms = 10_000;
        if expires_at_ms < best_ttl
            || (expires_at_ms == best_ttl
                && best_idx.is_none_or(|b| key.as_slice() < keys[b].as_slice()))
        {
            best_ttl = expires_at_ms;
            best_idx = Some(i);
        }
    }
    best_idx.map(|i| keys[i].clone())
}

fn bench_ttl_eviction_candidate_clone(c: &mut Criterion) {
    const N: usize = 100;
    let keys: Vec<Vec<u8>> = (0..N).map(|i| format!("t{i:03}").into_bytes()).collect();
    let sample: Vec<Vec<u8>> = keys.iter().rev().cloned().collect();
    assert_eq!(
        ttl_clone_each_best(&sample),
        ttl_defer_winner_clone(&sample)
    );

    let mut g = c.benchmark_group("ttl_eviction_candidate_clone");
    g.bench_function("orig_clone_each_best", |b| {
        b.iter(|| std::hint::black_box(ttl_clone_each_best(std::hint::black_box(&sample))))
    });
    g.bench_function("defer_winner_clone", |b| {
        b.iter(|| std::hint::black_box(ttl_defer_winner_clone(std::hint::black_box(&sample))))
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_get,
    bench_zrange_withscores,
    bench_restore_zset_listpack,
    bench_hrandfield_withvalues,
    bench_zscan0_borrow,
    bench_zlexcount_borrowed_bounds,
    bench_ttl_eviction_candidate_clone
);
criterion_main!(benches);
