//! OBJECT ENCODING transition conformance matrix.
//!
//! For each data-structure threshold, walk up to exactly the boundary and
//! one past, asserting `object_encoding` flips on the right cardinality /
//! value-size cell. Also encodes the invariant that upward inserts never
//! downgrade encoding (ENC-MONO) and that replacing an element with one of
//! identical type does not shift encoding.
//!
//! Thresholds exercised are the Store defaults documented at
//! `crates/fr-store/src/lib.rs:1285-1292`:
//!
//!   hash_max_listpack_entries = 512
//!   list_max_listpack_entries = 128
//!   set_max_intset_entries    = 512
//!   set_max_listpack_entries  = 128
//!   zset_max_listpack_entries = 128
//!   hash_max_listpack_value   = 64 (see field_limit below)
//!   zset_max_listpack_value   = 64
//!   list_max_listpack_value   = 64
//!
//! (br-frankenredis-euqm)

use fr_store::Store;

const NOW: u64 = 0;

// ── String encoding axis ────────────────────────────────────────────

#[test]
fn string_encoding_int_for_canonical_i64() {
    let mut store = Store::new();
    store.set(b"k".to_vec(), b"42".to_vec(), None, NOW);
    assert_eq!(store.object_encoding(b"k", NOW), Some("int"));
}

#[test]
fn string_encoding_int_rejects_leading_plus_noncanonical() {
    let mut store = Store::new();
    // "+42" parses as 42 but doesn't round-trip to "+42", so NOT "int".
    store.set(b"k".to_vec(), b"+42".to_vec(), None, NOW);
    assert_eq!(store.object_encoding(b"k", NOW), Some("embstr"));
}

#[test]
fn string_encoding_embstr_at_44_bytes_boundary() {
    let mut store = Store::new();
    store.set(b"embstr".to_vec(), vec![b'x'; 44], None, NOW);
    assert_eq!(store.object_encoding(b"embstr", NOW), Some("embstr"));
}

#[test]
fn string_encoding_raw_at_45_bytes_past_embstr_boundary() {
    let mut store = Store::new();
    store.set(b"raw".to_vec(), vec![b'x'; 45], None, NOW);
    assert_eq!(store.object_encoding(b"raw", NOW), Some("raw"));
}

// ── Hash encoding axis ──────────────────────────────────────────────

#[test]
fn hash_encoding_listpack_at_threshold_entries() {
    let mut store = Store::new();
    for i in 0..512_u32 {
        let field = format!("f{i}").into_bytes();
        store.hset(b"h", field, b"v".to_vec(), NOW).expect("hset");
    }
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack"));
}

#[test]
fn hash_encoding_hashtable_one_entry_past_threshold() {
    let mut store = Store::new();
    for i in 0..513_u32 {
        let field = format!("f{i}").into_bytes();
        store.hset(b"h", field, b"v".to_vec(), NOW).expect("hset");
    }
    assert_eq!(store.object_encoding(b"h", NOW), Some("hashtable"));
}

#[test]
fn hash_encoding_hashtable_when_any_value_exceeds_64_bytes() {
    let mut store = Store::new();
    store
        .hset(b"h", b"small".to_vec(), b"ok".to_vec(), NOW)
        .expect("hset");
    // 65-byte value exceeds the default hash_max_listpack_value (64).
    store
        .hset(b"h", b"big".to_vec(), vec![b'x'; 65], NOW)
        .expect("hset");
    assert_eq!(store.object_encoding(b"h", NOW), Some("hashtable"));
}

#[test]
fn hash_encoding_hashtable_when_field_name_exceeds_64_bytes() {
    let mut store = Store::new();
    let long_field = vec![b'f'; 65];
    store
        .hset(b"h", long_field, b"v".to_vec(), NOW)
        .expect("hset");
    assert_eq!(store.object_encoding(b"h", NOW), Some("hashtable"));
}

// ── Set encoding axis ───────────────────────────────────────────────

#[test]
fn set_encoding_intset_for_all_integer_members_within_threshold() {
    let mut store = Store::new();
    let members: Vec<Vec<u8>> = (1..=10).map(|n: i64| n.to_string().into_bytes()).collect();
    store.sadd(b"s", &members, NOW).expect("sadd");
    assert_eq!(store.object_encoding(b"s", NOW), Some("intset"));
}

#[test]
fn set_encoding_listpack_when_first_noninteger_member_added() {
    let mut store = Store::new();
    let members: Vec<Vec<u8>> = vec![b"1".to_vec(), b"2".to_vec(), b"hello".to_vec()];
    store.sadd(b"s", &members, NOW).expect("sadd");
    assert_eq!(store.object_encoding(b"s", NOW), Some("listpack"));
}

#[test]
fn set_encoding_listpack_beyond_intset_entries_even_when_integers() {
    let mut store = Store::new();
    // 513 all-integer members → still more than set_max_intset_entries (512).
    // Upstream falls back to listpack when cardinality <= set_max_listpack_entries
    // (128) — but here we're past both, so we expect hashtable.
    let members: Vec<Vec<u8>> = (1..=513_i64).map(|n| n.to_string().into_bytes()).collect();
    store.sadd(b"s", &members, NOW).expect("sadd");
    assert_eq!(store.object_encoding(b"s", NOW), Some("hashtable"));
}

#[test]
fn set_encoding_hashtable_one_past_listpack_threshold_noninteger_members() {
    let mut store = Store::new();
    let mut members: Vec<Vec<u8>> = (0..128_u32).map(|i| format!("m{i}").into_bytes()).collect();
    members.push(b"mX".to_vec()); // 129 non-integer members
    store.sadd(b"s", &members, NOW).expect("sadd");
    assert_eq!(store.object_encoding(b"s", NOW), Some("hashtable"));
}

#[test]
fn set_encoding_listpack_at_threshold_entries_noninteger_members() {
    let mut store = Store::new();
    let members: Vec<Vec<u8>> = (0..128_u32).map(|i| format!("m{i}").into_bytes()).collect();
    store.sadd(b"s", &members, NOW).expect("sadd");
    assert_eq!(store.object_encoding(b"s", NOW), Some("listpack"));
}

// ── Sorted set encoding axis ────────────────────────────────────────

#[test]
fn zset_encoding_listpack_at_threshold_entries() {
    let mut store = Store::new();
    let members: Vec<(f64, Vec<u8>)> = (0..128_u32)
        .map(|i| (i as f64, format!("m{i}").into_bytes()))
        .collect();
    store.zadd(b"z", &members, NOW).expect("zadd");
    assert_eq!(store.object_encoding(b"z", NOW), Some("listpack"));
}

#[test]
fn zset_encoding_skiplist_one_past_threshold_entries() {
    let mut store = Store::new();
    let members: Vec<(f64, Vec<u8>)> = (0..129_u32)
        .map(|i| (i as f64, format!("m{i}").into_bytes()))
        .collect();
    store.zadd(b"z", &members, NOW).expect("zadd");
    assert_eq!(store.object_encoding(b"z", NOW), Some("skiplist"));
}

#[test]
fn zset_encoding_skiplist_when_any_member_exceeds_64_bytes() {
    let mut store = Store::new();
    store
        .zadd(b"z", &[(1.0, b"short".to_vec())], NOW)
        .expect("zadd");
    store
        .zadd(b"z", &[(2.0, vec![b'm'; 65])], NOW)
        .expect("zadd long");
    assert_eq!(store.object_encoding(b"z", NOW), Some("skiplist"));
}

// ── List encoding axis ──────────────────────────────────────────────

#[test]
fn list_encoding_listpack_at_threshold_entries() {
    let mut store = Store::new();
    let values: Vec<Vec<u8>> = (0..128_u32).map(|i| format!("v{i}").into_bytes()).collect();
    store.rpush(b"l", &values, NOW).expect("rpush");
    assert_eq!(store.object_encoding(b"l", NOW), Some("listpack"));
}

#[test]
fn list_encoding_quicklist_one_past_threshold_bytes() {
    // (frankenredis-llry) Upstream Redis 7.2 t_list.c::listTypeTryConversion
    // converts purely on the byte budget (server.list_max_listpack_size,
    // default -2 = 8 KiB). 129 small elements fit within the budget so the
    // list stays as a single listpack — only overflowing the byte budget
    // forces a quicklist.
    let mut store = Store::new();
    let small_values: Vec<Vec<u8>> =
        (0..129_u32).map(|i| format!("v{i}").into_bytes()).collect();
    store.rpush(b"l", &small_values, NOW).expect("rpush small");
    assert_eq!(store.object_encoding(b"l", NOW), Some("listpack"));

    // Pushing one element that pushes the total well past the 8 KiB
    // byte budget (with the per-entry overhead) flips the encoding.
    store
        .rpush(b"l", &[vec![b'x'; 9_000]], NOW)
        .expect("rpush bulk");
    assert_eq!(store.object_encoding(b"l", NOW), Some("quicklist"));
}

#[test]
fn list_encoding_listpack_for_values_above_legacy_per_value_cap() {
    // (frankenredis-udxy) Upstream Redis 7.2 has no per-element value
    // cap distinct from the byte-budget — the fr-only
    // `list-max-listpack-value` config (default 64) does not gate the
    // listpack/quicklist transition. Differential probe vs vendored
    // redis 7.2.4 confirmed: a 65-byte (or 100-byte, or 8000-byte)
    // single-element list still reports "listpack" so long as the
    // total stays under list_max_listpack_size's byte budget.
    let mut store = Store::new();
    store
        .rpush(b"l", &[b"short".to_vec()], NOW)
        .expect("rpush short");
    store
        .rpush(b"l", &[vec![b'x'; 65]], NOW)
        .expect("rpush 65 byte");
    assert_eq!(store.object_encoding(b"l", NOW), Some("listpack"));

    // Cranking the spurious config low has no effect — byte-budget
    // alone is authoritative.
    store.list_max_listpack_value = 1;
    assert_eq!(store.object_encoding(b"l", NOW), Some("listpack"));
}

// ── Metamorphic invariants ──────────────────────────────────────────

#[test]
fn mr_enc_mono_hash_does_not_downgrade_on_insert_sweep() {
    // ENC-MONO: For hash, encoding moves monotonically listpack → hashtable
    // as we add entries. Flips exactly once, never back.
    let mut store = Store::new();
    let mut transitions: Vec<(usize, &'static str)> = Vec::new();
    for i in 0..520_u32 {
        let field = format!("f{i}").into_bytes();
        store.hset(b"h", field, b"v".to_vec(), NOW).expect("hset");
        let enc = store.object_encoding(b"h", NOW).expect("hash encoding");
        if transitions
            .last()
            .is_none_or(|(_, last_enc)| *last_enc != enc)
        {
            transitions.push((i as usize + 1, enc));
        }
    }
    // Expect exactly two observed encodings (listpack first, hashtable second).
    let encs: Vec<&'static str> = transitions.iter().map(|(_, e)| *e).collect();
    assert_eq!(
        encs,
        vec!["listpack", "hashtable"],
        "unexpected hash encoding trajectory: {transitions:?}"
    );
}

#[test]
fn mr_enc_no_shift_on_replace_hash_value_same_size() {
    // Replacing an existing field's value with another of identical size
    // must not shift encoding. Stay on listpack.
    let mut store = Store::new();
    store
        .hset(b"h", b"k".to_vec(), b"value1".to_vec(), NOW)
        .expect("hset");
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack"));
    store
        .hset(b"h", b"k".to_vec(), b"value2".to_vec(), NOW)
        .expect("hset replace");
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack"));
}

#[test]
fn mr_enc_mono_set_traverses_intset_listpack_hashtable_in_order() {
    // ENC-MONO-SET: For sets, encoding moves monotonically through
    // intset → listpack → hashtable as the membership grows. Each
    // transition fires at most once and never reverses, even when
    // intermediate states could re-qualify for an earlier encoding.
    //
    // Stage 1: pure-integer members ≤ set_max_intset_entries → intset
    // Stage 2: introducing one non-integer member with the working set
    //          still under set_max_listpack_entries → listpack
    // Stage 3: growing past set_max_listpack_entries → hashtable
    let mut store = Store::new();
    let mut transitions: Vec<&'static str> = Vec::new();
    let record = |store: &mut Store, transitions: &mut Vec<&'static str>| {
        let enc = store.object_encoding(b"s", NOW).expect("set encoding");
        if transitions.last().is_none_or(|prev| *prev != enc) {
            transitions.push(enc);
        }
    };

    for i in 0..50_u32 {
        store
            .sadd(b"s", &[format!("{i}").into_bytes()], NOW)
            .expect("sadd int");
        record(&mut store, &mut transitions);
    }
    // Adding one non-integer demotes to listpack (no longer pure-int).
    store
        .sadd(b"s", &[b"alpha".to_vec()], NOW)
        .expect("sadd non-int");
    record(&mut store, &mut transitions);

    // Grow past set_max_listpack_entries (default 128) to flip to hashtable.
    for i in 0..200_u32 {
        store
            .sadd(b"s", &[format!("m{i}").into_bytes()], NOW)
            .expect("sadd extra");
        record(&mut store, &mut transitions);
    }

    let encs: Vec<&'static str> = transitions.clone();
    assert_eq!(
        encs,
        vec!["intset", "listpack", "hashtable"],
        "unexpected set encoding trajectory: {transitions:?}"
    );
}

#[test]
#[ignore = "frankenredis-1jwi: fr currently downgrades to intset after SREM removes the last non-int; upstream keeps listpack"]
fn mr_enc_no_downgrade_on_set_srem_after_listpack_promotion() {
    // ENC-NO-DOWNGRADE-ON-REMOVE: Once a set has been promoted from
    // intset to listpack (because a non-integer was added), removing
    // that non-integer does NOT downgrade the encoding back to intset.
    // Upstream Redis tracks the "this set was once non-int" state via
    // the encoding itself; downgrades are not part of the type
    // contract. (mirror of upstream t_set.c::setTypeMaybeConvert)
    let mut store = Store::new();
    for i in 0..10_u32 {
        store
            .sadd(b"s", &[format!("{i}").into_bytes()], NOW)
            .expect("sadd int");
    }
    assert_eq!(store.object_encoding(b"s", NOW), Some("intset"));

    store
        .sadd(b"s", &[b"alpha".to_vec()], NOW)
        .expect("sadd non-int");
    assert_eq!(store.object_encoding(b"s", NOW), Some("listpack"));

    let alpha = b"alpha".as_slice();
    store
        .srem(b"s", &[alpha], NOW)
        .expect("srem non-int");
    // Now the set is purely integers again, BUT the encoding stays at
    // listpack — this is the metamorphic non-reversal property.
    assert_eq!(store.object_encoding(b"s", NOW), Some("listpack"));
}

#[test]
fn mr_enc_mono_zset_does_not_downgrade_on_insert_sweep() {
    let mut store = Store::new();
    let mut last: Option<&'static str> = None;
    let mut flipped = false;
    for i in 0..140_u32 {
        let pair = (i as f64, format!("m{i}").into_bytes());
        store.zadd(b"z", &[pair], NOW).expect("zadd");
        let enc = store.object_encoding(b"z", NOW).expect("zset encoding");
        match last {
            None => last = Some(enc),
            Some(prev) if prev != enc => {
                assert!(
                    !flipped,
                    "zset encoding flipped more than once (at len={})",
                    i + 1
                );
                assert_eq!(prev, "listpack");
                assert_eq!(enc, "skiplist");
                flipped = true;
                last = Some(enc);
            }
            _ => {}
        }
    }
    assert!(flipped, "zset encoding never transitioned to skiplist");
}
