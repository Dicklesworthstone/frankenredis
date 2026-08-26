use fr_persist::{
    AofRecord, RdbEntry, RdbValue, decode_aof_stream, decode_rdb, encode_aof_stream, encode_rdb,
};
use proptest::prelude::*;

fn arb_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..64)
}

fn arb_value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..256)
}

fn arb_field_value() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (arb_key(), arb_value())
}

fn arb_finite_f64() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("must be finite", |f| f.is_finite())
}

fn arb_zset_member() -> impl Strategy<Value = (Vec<u8>, f64)> {
    (arb_key(), arb_finite_f64())
}

fn sort_zset_for_redis_order(members: &[(Vec<u8>, f64)]) -> Vec<(Vec<u8>, f64)> {
    let mut sorted = members.to_vec();
    sorted.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    sorted
}

/// Render a decoded set back to the `Set` spelling its encoded input used; every
/// other value is returned unchanged. (frankenredis-wuxai)
///
/// A set survives `encode_rdb` -> `decode_rdb` with its MEMBERS intact but not
/// necessarily its variant, and this test compares variants:
///
/// - a plain `RDB_TYPE_SET` decodes to `SetHashtable`, deliberately, so the load
///   does not re-derive a smaller encoding from content (frankenredis-39is8) —
///   and `encode_rdb` without compact options writes exactly that type, so EVERY
///   generated `Set` came back as `SetHashtable`;
/// - an all-integer set written as `RDB_TYPE_SET_INTSET` decodes to the typed
///   `IntSet` (f4781193c).
///
/// Both left this test's `(Set, Set)` arm unreachable for sets, so it fell
/// through to the catch-all "value type changed during roundtrip". The lib-side
/// round-trip tests already carry the 39is8 accommodation; this file never got
/// it, and the lib failure masked that because cargo stops at the first failing
/// test target.
///
/// `decimal_i64_bytes` is crate-private, but `to_string` is byte-identical to it
/// for every `i64` (canonical decimal, leading `-` for negatives) — the same
/// rendering the decoder produced before the typed variant landed.
fn canonicalise_decoded_set(value: &RdbValue) -> RdbValue {
    match value {
        RdbValue::SetHashtable(members) => RdbValue::Set(members.clone()),
        RdbValue::IntSet(values) => RdbValue::Set(
            values
                .iter()
                .map(|member| member.to_string().into_bytes())
                .collect(),
        ),
        // (frankenredis-aqkvk) A listpack-encoded hash now decodes as the blob
        // it was stored as, so put it back into the `Hash` spelling the
        // generator produced by DECODING it. Exactly the IntSet precedent above:
        // only the variant stops being load-bearing — every field and value is
        // still compared by the arms below, so a blob whose contents drifted
        // still fails, and a non-hash decoding as a hash still falls through to
        // the catch-all.
        RdbValue::HashListpack(blob) => {
            let spans = fr_persist::listpack::decode_value_spans(blob)
                .expect("a blob we just encoded must decode");
            assert!(
                spans.len().is_multiple_of(2),
                "hash listpack must hold field/value pairs"
            );
            let (pairs, _) = spans.as_chunks::<2>();
            RdbValue::Hash(
                pairs
                    .iter()
                    .map(|p| (p[0].as_bytes(blob).to_vec(), p[1].as_bytes(blob).to_vec()))
                    .collect(),
            )
        }
        // (frankenredis-qj6jn) A list written as QUICKLIST_2 now decodes as its VERBATIM
        // listpack node blobs -- upstream's own shape, which the store installs directly
        // instead of rebuilding element by element. Same precedent as the two above: decode
        // the blobs back to the `List` spelling the generator produced, so every element is
        // still compared by the arms below and only the variant stops being load-bearing.
        RdbValue::ListQuicklist2Packed(nodes) => {
            let mut items = Vec::new();
            for node in nodes {
                let spans = fr_persist::listpack::decode_value_spans(node)
                    .expect("a blob we just encoded must decode");
                for span in spans {
                    items.push(span.as_bytes(node).to_vec());
                }
            }
            RdbValue::List(items)
        }
        // (BlackThrush) A set handed to the encoder as its listpack blob: decode
        // back to the `Set` spelling the generator produced, so every member is
        // still compared and only the VARIANT stops being load-bearing.
        RdbValue::SetListpack(blob) => RdbValue::Set(
            fr_persist::listpack::decode_value_spans(blob)
                .expect("a blob we just encoded must decode")
                .iter()
                .map(|s| s.as_bytes(blob).to_vec())
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The variant name of an `RdbValue`, for failure messages that have to
/// distinguish a spelling change from a content change. (frankenredis-wuxai)
fn rdb_value_kind(value: &RdbValue) -> &'static str {
    match value {
        RdbValue::String(_) => "String",
        RdbValue::List(_) => "List",
        RdbValue::ListQuicklist2Packed(_) => "ListQuicklist2Packed",
        RdbValue::Set(_) => "Set",
        RdbValue::SetListpack(_) => "SetListpack",
        RdbValue::IntSet(_) => "IntSet",
        RdbValue::SetHashtable(_) => "SetHashtable",
        RdbValue::Hash(_) => "Hash",
        RdbValue::HashListpack(_) => "HashListpack",
        RdbValue::HashWithTtls(_) => "HashWithTtls",
        RdbValue::SortedSet(_) => "SortedSet",
        RdbValue::Stream(..) => "Stream",
    }
}

fn arb_rdb_value() -> impl Strategy<Value = RdbValue> {
    prop_oneof![
        arb_value().prop_map(RdbValue::String),
        prop::collection::vec(arb_value(), 0..16).prop_map(RdbValue::List),
        prop::collection::vec(arb_key(), 0..16).prop_map(RdbValue::Set),
        prop::collection::vec(arb_field_value(), 0..16).prop_map(RdbValue::Hash),
        prop::collection::vec(arb_zset_member(), 0..16).prop_map(RdbValue::SortedSet),
    ]
}

fn arb_aof_argv() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(arb_value(), 1..8)
}

fn arb_aof_record() -> impl Strategy<Value = AofRecord> {
    arb_aof_argv().prop_map(|argv| AofRecord { argv })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn mr_aof_roundtrip(records in prop::collection::vec(arb_aof_record(), 0..20)) {
        let encoded = encode_aof_stream(&records);
        let decoded = decode_aof_stream(&encoded).expect("decode should succeed for valid encoded AOF");
        prop_assert_eq!(records, decoded, "AOF roundtrip must preserve records");
    }

    #[test]
    fn mr_rdb_roundtrip(count in 0usize..10) {
        let entries: Vec<RdbEntry> = (0..count).map(|i| {
            let mut key = vec![b'k'];
            key.extend_from_slice(&i.to_le_bytes());
            RdbEntry {
                db: 0,
                key,
                value: RdbValue::String(format!("value{i}").into_bytes()),
                expire_ms: None,
            }
        }).collect();

        let aux = [("redis-ver", "7.4.0"), ("redis-bits", "64")];
        let encoded = encode_rdb(&entries, &aux);
        let (decoded_entries, decoded_aux) = decode_rdb(&encoded)
            .expect("decode should succeed for valid encoded RDB");

        prop_assert_eq!(entries.len(), decoded_entries.len(), "entry count mismatch");

        for (orig, dec) in entries.iter().zip(decoded_entries.iter()) {
            prop_assert_eq!(orig.db, dec.db, "db mismatch");
            prop_assert_eq!(&orig.key, &dec.key, "key mismatch");
            prop_assert_eq!(orig.expire_ms, dec.expire_ms, "expire_ms mismatch");
            // (frankenredis-wuxai) An all-integer set is written as
            // RDB_TYPE_SET_INTSET and decodes back as the typed
            // `RdbValue::IntSet`, so put it back into the `Set` spelling the
            // generator produced. Only the variant stops being load-bearing —
            // the arms below still compare every member, and a non-set decoding
            // as a set (or vice versa) still falls through to the catch-all.
            let dec_value = canonicalise_decoded_set(&dec.value);
            match (&orig.value, &dec_value) {
                (RdbValue::String(a), RdbValue::String(b)) => {
                    prop_assert_eq!(a, b, "string value mismatch");
                }
                (RdbValue::List(a), RdbValue::List(b)) => {
                    prop_assert_eq!(a, b, "list value mismatch");
                }
                (RdbValue::Set(a), RdbValue::Set(b)) => {
                    let mut a_sorted = a.clone();
                    let mut b_sorted = b.clone();
                    a_sorted.sort();
                    b_sorted.sort();
                    prop_assert_eq!(a_sorted, b_sorted, "set value mismatch (order-independent)");
                }
                (RdbValue::Hash(a), RdbValue::Hash(b)) => {
                    let mut a_sorted = a.clone();
                    let mut b_sorted = b.clone();
                    a_sorted.sort();
                    b_sorted.sort();
                    prop_assert_eq!(a_sorted, b_sorted, "hash value mismatch (order-independent)");
                }
                (RdbValue::SortedSet(a), RdbValue::SortedSet(b)) => {
                    prop_assert_eq!(a.len(), b.len(), "zset length mismatch");
                    let a_sorted = sort_zset_for_redis_order(a);
                    let b_sorted = sort_zset_for_redis_order(b);
                    for ((ma, sa), (mb, sb)) in a_sorted.iter().zip(b_sorted.iter()) {
                        prop_assert_eq!(ma, mb, "zset member mismatch");
                        if sa.is_nan() && sb.is_nan() {
                            continue;
                        }
                        prop_assert!((sa - sb).abs() < 1e-10, "zset score mismatch: {} vs {}", sa, sb);
                    }
                }
                (RdbValue::Stream(_, _, _, _, _, _), RdbValue::Stream(_, _, _, _, _, _)) => {
                    // Stream encoding has known incompatibilities, skip detailed comparison
                }
                (a, b) => {
                    prop_assert!(false, "value type mismatch: {:?} vs {:?}", a, b);
                }
            }
        }

        prop_assert_eq!(decoded_aux.get("redis-ver").map(String::as_str), Some("7.4.0"));
        prop_assert_eq!(decoded_aux.get("redis-bits").map(String::as_str), Some("64"));
    }

    #[test]
    fn mr_aof_record_resp_roundtrip(record in arb_aof_record()) {
        let frame = record.to_resp_frame();
        let recovered = AofRecord::from_resp_frame(&frame)
            .expect("from_resp_frame should succeed for valid frame");
        prop_assert_eq!(record, recovered, "AofRecord <-> RespFrame roundtrip must preserve data");
    }

    #[test]
    fn mr_rdb_idempotent_encoding(count in 0usize..5) {
        let entries: Vec<RdbEntry> = (0..count).map(|i| {
            let mut key = vec![b'k'];
            key.extend_from_slice(&i.to_le_bytes());
            RdbEntry {
                db: 0,
                key,
                value: RdbValue::String(format!("value{i}").into_bytes()),
                expire_ms: None,
            }
        }).collect();

        let aux = [("redis-ver", "7.4.0")];

        let encoded1 = encode_rdb(&entries, &aux);
        let (decoded, _) = decode_rdb(&encoded1).expect("first decode");
        let encoded2 = encode_rdb(&decoded, &aux);
        let (decoded2, _) = decode_rdb(&encoded2).expect("second decode");

        prop_assert_eq!(decoded.len(), decoded2.len(), "idempotent roundtrip count mismatch");
    }

    #[test]
    fn mr_aof_empty_preserves_empty(records in Just(Vec::<AofRecord>::new())) {
        let encoded = encode_aof_stream(&records);
        prop_assert!(encoded.is_empty(), "empty AOF should encode to empty bytes");
        let decoded = decode_aof_stream(&encoded).expect("decode empty");
        prop_assert!(decoded.is_empty(), "empty bytes should decode to empty records");
    }

    #[test]
    fn mr_rdb_empty_decodes(entries in Just(Vec::<RdbEntry>::new())) {
        let aux: &[(&str, &str)] = &[];
        let encoded = encode_rdb(&entries, aux);
        let (decoded, _) = decode_rdb(&encoded).expect("decode empty RDB");
        prop_assert!(decoded.is_empty(), "empty RDB should decode to empty entries");
    }

    #[test]
    fn mr_rdb_diverse_values_roundtrip(
        value in arb_rdb_value(),
        db in 0usize..4,
        expire_ms in prop::option::of(1_000_000u64..2_000_000_000_000u64)
    ) {
        let entry = RdbEntry {
            db,
            key: b"unique_test_key".to_vec(),
            value: value.clone(),
            expire_ms,
        };
        let encoded = encode_rdb(&[entry], &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode should succeed");

        prop_assert_eq!(decoded.len(), 1, "should decode exactly one entry");
        prop_assert_eq!(decoded[0].db, db, "db should match");
        prop_assert_eq!(&decoded[0].key, b"unique_test_key", "key should match");
        prop_assert_eq!(decoded[0].expire_ms, expire_ms, "expire_ms should match");

        // Sets round-trip their MEMBERS, not their variant — see
        // `canonicalise_decoded_set`. (frankenredis-wuxai)
        let decoded_value = canonicalise_decoded_set(&decoded[0].value);
        match (&value, &decoded_value) {
            (RdbValue::String(a), RdbValue::String(b)) => {
                prop_assert_eq!(a, b, "string roundtrip failed");
            }
            (RdbValue::List(a), RdbValue::List(b)) => {
                prop_assert_eq!(a, b, "list roundtrip failed");
            }
            (RdbValue::Set(a), RdbValue::Set(b)) => {
                let mut a_sorted = a.clone();
                let mut b_sorted = b.clone();
                a_sorted.sort();
                b_sorted.sort();
                prop_assert_eq!(a_sorted, b_sorted, "set roundtrip failed");
            }
            (RdbValue::Hash(a), RdbValue::Hash(b)) => {
                let mut a_sorted = a.clone();
                let mut b_sorted = b.clone();
                a_sorted.sort();
                b_sorted.sort();
                prop_assert_eq!(a_sorted, b_sorted, "hash roundtrip failed");
            }
            (RdbValue::SortedSet(a), RdbValue::SortedSet(b)) => {
                prop_assert_eq!(a.len(), b.len(), "zset length mismatch");
                let a_sorted = sort_zset_for_redis_order(a);
                let b_sorted = sort_zset_for_redis_order(b);
                for ((ma, sa), (mb, sb)) in a_sorted.iter().zip(b_sorted.iter()) {
                    prop_assert_eq!(ma, mb, "zset member mismatch");
                    if !(sa.is_nan() && sb.is_nan()) {
                        prop_assert!((sa - sb).abs() < 1e-10, "zset score mismatch");
                    }
                }
            }
            (RdbValue::Stream(_, _, _, _, _, _), RdbValue::Stream(_, _, _, _, _, _)) => {
                // Stream encoding has known incompatibilities
            }
            (encoded_value, round_tripped) => {
                // Name BOTH variants: "value type changed" alone cannot tell a
                // real encoder bug from a spelling the canonicaliser above does
                // not yet cover, and that ambiguity is what let this test sit
                // red. (frankenredis-wuxai)
                prop_assert!(
                    false,
                    "value type changed during roundtrip: encoded {} -> decoded {} \
                     (canonicalised from {})",
                    rdb_value_kind(encoded_value),
                    rdb_value_kind(round_tripped),
                    rdb_value_kind(&decoded[0].value),
                );
            }
        }
    }
}

#[test]
fn unit_aof_single_record_roundtrip() {
    let record = AofRecord {
        argv: vec![b"SET".to_vec(), b"key".to_vec(), b"value".to_vec()],
    };
    let encoded = encode_aof_stream(std::slice::from_ref(&record));
    let decoded = decode_aof_stream(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], record);
}

#[test]
fn unit_rdb_string_roundtrip() {
    let entry = RdbEntry {
        db: 0,
        key: b"mykey".to_vec(),
        value: RdbValue::String(b"myvalue".to_vec()),
        expire_ms: None,
    };
    let encoded = encode_rdb(std::slice::from_ref(&entry), &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].key, entry.key);
    assert!(matches!(&decoded[0].value, RdbValue::String(v) if v == b"myvalue"));
}

#[test]
fn unit_rdb_list_roundtrip() {
    let entry = RdbEntry {
        db: 0,
        key: b"mylist".to_vec(),
        value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]),
        expire_ms: Some(1700000000000),
    };
    let encoded = encode_rdb(std::slice::from_ref(&entry), &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    // (frankenredis-qj6jn) QUICKLIST_2 decodes to its verbatim node blobs; canonicalise to
    // the element spelling before asking about cardinality.
    assert!(
        matches!(canonicalise_decoded_set(&decoded[0].value), RdbValue::List(items) if items.len() == 3)
    );
    assert_eq!(decoded[0].expire_ms, Some(1700000000000));
}

#[test]
fn unit_rdb_hash_roundtrip() {
    let entry = RdbEntry {
        db: 0,
        key: b"myhash".to_vec(),
        value: RdbValue::Hash(vec![
            (b"field1".to_vec(), b"value1".to_vec()),
            (b"field2".to_vec(), b"value2".to_vec()),
        ]),
        expire_ms: None,
    };
    let encoded = encode_rdb(std::slice::from_ref(&entry), &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    // (frankenredis-aqkvk) A listpack-encoded hash decodes as its blob so the
    // load path can build from borrowed spans; canonicalise before the shape
    // check so this still asserts two fields rather than the spelling.
    assert!(matches!(
        canonicalise_decoded_set(&decoded[0].value),
        RdbValue::Hash(ref fields) if fields.len() == 2
    ));
}

#[test]
fn unit_rdb_zset_roundtrip() {
    let entry = RdbEntry {
        db: 0,
        key: b"myzset".to_vec(),
        value: RdbValue::SortedSet(vec![
            (b"one".to_vec(), 1.0),
            (b"two".to_vec(), 2.0),
            (b"three".to_vec(), 3.5),
        ]),
        expire_ms: None,
    };
    let encoded = encode_rdb(std::slice::from_ref(&entry), &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0].value, RdbValue::SortedSet(members) if members.len() == 3));
}
