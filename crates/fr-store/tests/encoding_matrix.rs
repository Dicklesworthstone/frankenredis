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
//!   hash_max_listpack_entries = 128
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
    for i in 0..128_u32 {
        let field = format!("f{i}").into_bytes();
        store.hset(b"h", field, b"v".to_vec(), NOW).expect("hset");
    }
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack"));
}

#[test]
fn hash_encoding_hashtable_one_entry_past_threshold() {
    let mut store = Store::new();
    for i in 0..129_u32 {
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
fn list_encoding_quicklist_one_past_threshold_entries() {
    let mut store = Store::new();
    let values: Vec<Vec<u8>> = (0..129_u32).map(|i| format!("v{i}").into_bytes()).collect();
    store.rpush(b"l", &values, NOW).expect("rpush");
    assert_eq!(store.object_encoding(b"l", NOW), Some("quicklist"));
}

#[test]
fn list_encoding_quicklist_when_any_value_exceeds_64_bytes() {
    let mut store = Store::new();
    store
        .rpush(b"l", &[b"short".to_vec()], NOW)
        .expect("rpush short");
    store
        .rpush(b"l", &[vec![b'x'; 65]], NOW)
        .expect("rpush long");
    assert_eq!(store.object_encoding(b"l", NOW), Some("quicklist"));
}

// ── Metamorphic invariants ──────────────────────────────────────────

#[test]
fn mr_enc_mono_hash_does_not_downgrade_on_insert_sweep() {
    // ENC-MONO: For hash, encoding moves monotonically listpack → hashtable
    // as we add entries. Flips exactly once, never back.
    let mut store = Store::new();
    let mut transitions: Vec<(usize, &'static str)> = Vec::new();
    for i in 0..140_u32 {
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
