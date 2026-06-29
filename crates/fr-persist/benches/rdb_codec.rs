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

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use fr_persist::{
    AofRecord, RdbEntry, RdbValue, decode_rdb, encode_aof_stream, encode_aof_stream_tail_bytes,
    encode_rdb,
};

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
        entries.push(RdbEntry {
            db: 0,
            key: k(),
            value: RdbValue::Hash(fields),
            expire_ms: None,
        });
    }
    // listpack zsets (40 members, mixed scores)
    for _ in 0..400 {
        let members = (0..40)
            .map(|j| (format!("m{j}").into_bytes(), j as f64 * 1.5))
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: k(),
            value: RdbValue::SortedSet(members),
            expire_ms: None,
        });
    }
    // listpack sets + an intset
    for _ in 0..400 {
        let m = (0..40).map(|j| format!("m{j}").into_bytes()).collect();
        entries.push(RdbEntry {
            db: 0,
            key: k(),
            value: RdbValue::Set(m),
            expire_ms: None,
        });
    }
    for _ in 0..400 {
        let m = (0..40).map(|j| j.to_string().into_bytes()).collect();
        entries.push(RdbEntry {
            db: 0,
            key: k(),
            value: RdbValue::Set(m),
            expire_ms: None,
        });
    }
    // quicklist lists (>8KiB => multi-node)
    for _ in 0..300 {
        let l = (0..240)
            .map(|j| format!("e{j:02}{}", "x".repeat(40)).into_bytes())
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: k(),
            value: RdbValue::List(l),
            expire_ms: None,
        });
    }
    // int-bearing strings (087qq itoa2 path)
    let mut integer_value = 0i64;
    for _ in 0..4000 {
        let key_bytes = k();
        integer_value += 1;
        let value = (integer_value * 7919).to_string().into_bytes();
        entries.push(RdbEntry {
            db: 0,
            key: key_bytes,
            value: RdbValue::String(value),
            expire_ms: None,
        });
    }
    entries
}

fn patterned_bytes(seed: usize, len: usize) -> Vec<u8> {
    let mut state = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        out.push((state >> 32) as u8);
    }
    out
}

fn build_quicklist_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::new();
    for key in 0..300 {
        let list = (0..180)
            .map(|member| patterned_bytes(key * 4099 + member, 96))
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("ql:{key:06}").into_bytes(),
            value: RdbValue::List(list),
            expire_ms: None,
        });
    }
    entries
}

fn build_mixed_zset_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::with_capacity(600);
    for key in 0..600 {
        let members = (0..96)
            .rev()
            .map(|member| {
                let score = if member % 3 == 0 {
                    (member as i64 - 48) as f64
                } else {
                    (member as f64 * 1.5) + ((key % 7) as f64 * 0.125)
                };
                (format!("m{member:03}:k{}", key % 17).into_bytes(), score)
            })
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("z:{key:06}").into_bytes(),
            value: RdbValue::SortedSet(members),
            expire_ms: None,
        });
    }
    entries
}

fn build_hash_listpack_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::with_capacity(600);
    for key in 0..600 {
        let fields = (0..96)
            .map(|field| {
                let field_bytes = if field % 4 == 0 {
                    field.to_string().into_bytes()
                } else {
                    format!("f{field:03}:k{}", key % 19).into_bytes()
                };
                let value_bytes = if field % 3 == 0 {
                    (field as i64 - 48).to_string().into_bytes()
                } else {
                    format!("value:{key:04}:{field:03}").into_bytes()
                };
                (field_bytes, value_bytes)
            })
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("h:{key:06}").into_bytes(),
            value: RdbValue::Hash(fields),
            expire_ms: None,
        });
    }
    entries
}

fn build_set_listpack_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::with_capacity(600);
    for key in 0..600 {
        let members = (0u8..96)
            .map(|member| match member % 5 {
                0 => member.to_string().into_bytes(),
                1 => (i64::from(member) - 48).to_string().into_bytes(),
                2 => format!("member:{key:04}:{member:03}").into_bytes(),
                3 => {
                    let mut bytes = format!("bin:{key:04}:").into_bytes();
                    bytes.push(member % 251);
                    bytes
                }
                _ => format!("s{member:03}:k{}", key % 23).into_bytes(),
            })
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("s:{key:06}").into_bytes(),
            value: RdbValue::Set(members),
            expire_ms: None,
        });
    }
    entries
}

fn build_set_intset_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::with_capacity(900);
    for key in 0..900 {
        let members = (0..96)
            .map(|member| {
                let member = i64::from(member);
                let value = match key % 3 {
                    0 => member - 48,
                    1 => (member * 257) - 12_345,
                    _ => (member * 1_048_573) - 2_147_483_000,
                };
                value.to_string().into_bytes()
            })
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("is:{key:06}").into_bytes(),
            value: RdbValue::Set(members),
            expire_ms: None,
        });
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

    let quicklist_entries = build_quicklist_entries();
    let quicklist_encoded = encode_rdb(&quicklist_entries, &[]);
    let mut quicklist = c.benchmark_group("rdb_codec_quicklist");
    quicklist.throughput(Throughput::Elements(quicklist_entries.len() as u64));
    quicklist.bench_function("encode_quicklist_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&quicklist_entries), &[]))
    });
    quicklist.bench_function("decode_quicklist_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&quicklist_encoded)).unwrap())
    });
    quicklist.finish();

    let mixed_zset_entries = build_mixed_zset_entries();
    let mixed_zset_encoded = encode_rdb(&mixed_zset_entries, &[]);
    let mut mixed_zset = c.benchmark_group("rdb_codec_mixed_zset");
    mixed_zset.throughput(Throughput::Elements(mixed_zset_entries.len() as u64));
    mixed_zset.bench_function("encode_mixed_zset_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&mixed_zset_entries), &[]))
    });
    mixed_zset.bench_function("decode_mixed_zset_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&mixed_zset_encoded)).unwrap())
    });
    mixed_zset.finish();

    let hash_listpack_entries = build_hash_listpack_entries();
    let hash_listpack_encoded = encode_rdb(&hash_listpack_entries, &[]);
    let mut hash_listpack = c.benchmark_group("rdb_codec_hash_listpack");
    hash_listpack.throughput(Throughput::Elements(hash_listpack_entries.len() as u64));
    hash_listpack.bench_function("encode_hash_listpack_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&hash_listpack_entries), &[]))
    });
    hash_listpack.bench_function("decode_hash_listpack_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&hash_listpack_encoded)).unwrap())
    });
    hash_listpack.finish();

    let set_listpack_entries = build_set_listpack_entries();
    let set_listpack_encoded = encode_rdb(&set_listpack_entries, &[]);
    let mut set_listpack = c.benchmark_group("rdb_codec_set_listpack");
    set_listpack.throughput(Throughput::Elements(set_listpack_entries.len() as u64));
    set_listpack.bench_function("encode_set_listpack_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&set_listpack_entries), &[]))
    });
    set_listpack.bench_function("decode_set_listpack_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&set_listpack_encoded)).unwrap())
    });
    set_listpack.finish();

    let set_intset_entries = build_set_intset_entries();
    let set_intset_encoded = encode_rdb(&set_intset_entries, &[]);
    let mut set_intset = c.benchmark_group("rdb_codec_set_intset");
    set_intset.throughput(Throughput::Elements(set_intset_entries.len() as u64));
    set_intset.bench_function("encode_set_intset_rdb", |b| {
        b.iter(|| encode_rdb(std::hint::black_box(&set_intset_entries), &[]))
    });
    set_intset.bench_function("decode_set_intset_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&set_intset_encoded)).unwrap())
    });
    set_intset.finish();

    // Large, highly-compressible string VALUES (repetitive blobs that LZF shrinks
    // and that decompress back to >8 KiB) — common in real RDBs (JSON/text blobs)
    // and the case the lzf_decompress initial-capacity cap exercises. 200 x 64 KiB.
    let big_string_entries = build_big_compressible_string_entries();
    let big_string_encoded = encode_rdb(&big_string_entries, &[]);
    let mut big_string = c.benchmark_group("rdb_codec_big_compressible_string");
    big_string.throughput(Throughput::Elements(big_string_entries.len() as u64));
    big_string.bench_function("decode_big_compressible_string_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&big_string_encoded)).unwrap())
    });
    big_string.finish();

    // Large HASHTABLE-encoded hashes (> 512 fields ⇒ RDB_TYPE_HASH) — the decode
    // pre-sizes the outer field Vec at RDB_COLLECTION_PRESIZE_CAP; below the cap it
    // grew log2(count/1024) times. 40 x 8000 fields.
    let big_hash_entries = build_big_hashtable_entries();
    let big_hash_encoded = encode_rdb(&big_hash_entries, &[]);
    let mut big_hash = c.benchmark_group("rdb_codec_big_hashtable");
    big_hash.throughput(Throughput::Elements(big_hash_entries.len() as u64));
    big_hash.bench_function("decode_big_hashtable_rdb", |b| {
        b.iter(|| decode_rdb(std::hint::black_box(&big_hash_encoded)).unwrap())
    });
    big_hash.finish();

    // AOF/replication propagation: the offset-accounting path needs only the RESP
    // wire LENGTH of each record. `encoded_resp_len` computes it alloc-free vs the
    // prior `to_resp_frame().to_bytes().len()` which clones every arg + encodes the
    // whole command just to count + drop it. Representative SET with a 64 KiB value
    // (the large-value case where the clone+encode dominates). (aofreclen)
    let aof_set = AofRecord {
        argv: vec![
            b"SET".to_vec(),
            b"some:key:000001".to_vec(),
            vec![b'x'; 64 * 1024],
        ],
    };
    let mut aoflen = c.benchmark_group("rdb_codec_aof_reclen");
    aoflen.bench_function("len_via_to_bytes_64k", |b| {
        b.iter(|| std::hint::black_box(&aof_set).to_resp_frame().to_bytes().len())
    });
    aoflen.bench_function("encoded_resp_len_64k", |b| {
        b.iter(|| std::hint::black_box(&aof_set).encoded_resp_len())
    });
    aoflen.finish();

    // AOF LOAD: decoding a record went parse->RespFrame (parser clones each arg)
    // then from_resp_frame (clones AGAIN into argv). from_resp_frame_owned MOVES the
    // args out of the owned frame instead. iter_batched clones a fresh frame UNTIMED
    // so the timed routine isolates the second clone vs a move. 4 KiB value.
    let frame_template = aof_set.to_resp_frame();
    let mut aofdec = c.benchmark_group("rdb_codec_aof_from_frame");
    aofdec.bench_function("from_resp_frame_clone_64k", |b| {
        b.iter_batched(
            || frame_template.clone(),
            |frame| AofRecord::from_resp_frame(std::hint::black_box(&frame)).unwrap(),
            criterion::BatchSize::SmallInput,
        )
    });
    aofdec.bench_function("from_resp_frame_owned_64k", |b| {
        b.iter_batched(
            || frame_template.clone(),
            |frame| AofRecord::from_resp_frame_owned(std::hint::black_box(frame)).unwrap(),
            criterion::BatchSize::SmallInput,
        )
    });
    aofdec.finish();

    // Replica feed: a caught-up replica is one write behind, so the tail to send is
    // the LAST record. Old path encoded the whole backlog then sliced; the tail
    // encoder skips the prefix. 5000-record backlog, send only the final record.
    let backlog: Vec<AofRecord> = (0..5000)
        .map(|i| AofRecord {
            argv: vec![
                b"SET".to_vec(),
                format!("key:{i:08}").into_bytes(),
                format!("val:{i:08}").into_bytes(),
            ],
        })
        .collect();
    let full_len = encode_aof_stream(&backlog).len();
    let last_record_len = backlog[4999].encoded_resp_len();
    let last_record_start = full_len - last_record_len;
    let mut feed = c.benchmark_group("rdb_codec_aof_feed_tail");
    feed.bench_function("full_encode_then_slice", |b| {
        b.iter(|| {
            let s = encode_aof_stream(std::hint::black_box(&backlog));
            s.get(std::hint::black_box(last_record_start)..)
                .unwrap_or(&[])
                .to_vec()
        })
    });
    feed.bench_function("encode_tail_bytes", |b| {
        b.iter(|| {
            encode_aof_stream_tail_bytes(
                std::hint::black_box(&backlog),
                std::hint::black_box(last_record_len),
            )
        })
    });
    feed.finish();
}

fn build_big_hashtable_entries() -> Vec<RdbEntry> {
    // Hashes ABOVE hash_max_listpack_entries (512) encode as the plain hashtable
    // RDB_TYPE_HASH, whose decode pre-sizes the outer field Vec at a cap — the
    // realloc-on-grow case for large collections. 40 hashes x 8000 short fields.
    let mut entries = Vec::with_capacity(40);
    for key in 0..40 {
        let fields = (0..8000)
            .map(|j| {
                (
                    format!("f{j:05}").into_bytes(),
                    format!("v{j:05}:{}", key % 9).into_bytes(),
                )
            })
            .collect();
        entries.push(RdbEntry {
            db: 0,
            key: format!("h:{key:06}").into_bytes(),
            value: RdbValue::Hash(fields),
            expire_ms: None,
        });
    }
    entries
}

fn build_big_compressible_string_entries() -> Vec<RdbEntry> {
    let mut entries = Vec::with_capacity(200);
    for key in 0..200 {
        let unit = format!("field-{}-value-{};", key % 13, key % 7).into_bytes();
        let mut value = Vec::with_capacity(64 * 1024);
        while value.len() < 64 * 1024 {
            value.extend_from_slice(&unit);
        }
        entries.push(RdbEntry {
            db: 0,
            key: format!("big:{key:06}").into_bytes(),
            value: RdbValue::String(value),
            expire_ms: None,
        });
    }
    entries
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
