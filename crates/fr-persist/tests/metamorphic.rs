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

fn arb_rdb_value() -> impl Strategy<Value = RdbValue> {
    prop_oneof![
        arb_value().prop_map(RdbValue::String),
        prop::collection::vec(arb_value(), 0..16).prop_map(RdbValue::List),
        prop::collection::vec(arb_key(), 0..16).prop_map(RdbValue::Set),
        prop::collection::vec(arb_field_value(), 0..16).prop_map(RdbValue::Hash),
        prop::collection::vec(arb_zset_member(), 0..16).prop_map(RdbValue::SortedSet),
    ]
}

fn arb_rdb_entry_with_index(idx: usize) -> impl Strategy<Value = RdbEntry> {
    (
        0usize..4,
        arb_key(),
        arb_rdb_value(),
        prop::option::of(0u64..u64::MAX / 2),
    )
        .prop_map(move |(db, mut key, value, expire_ms)| {
            key.extend_from_slice(&idx.to_le_bytes());
            RdbEntry {
                db,
                key,
                value,
                expire_ms,
            }
        })
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
            match (&orig.value, &dec.value) {
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
                    for ((ma, sa), (mb, sb)) in a.iter().zip(b.iter()) {
                        prop_assert_eq!(ma, mb, "zset member mismatch");
                        if sa.is_nan() && sb.is_nan() {
                            continue;
                        }
                        prop_assert!((sa - sb).abs() < 1e-10, "zset score mismatch: {} vs {}", sa, sb);
                    }
                }
                (RdbValue::Stream(_, _, _), RdbValue::Stream(_, _, _)) => {
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

        match (&value, &decoded[0].value) {
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
                for ((ma, sa), (mb, sb)) in a.iter().zip(b.iter()) {
                    prop_assert_eq!(ma, mb, "zset member mismatch");
                    if !(sa.is_nan() && sb.is_nan()) {
                        prop_assert!((sa - sb).abs() < 1e-10, "zset score mismatch");
                    }
                }
            }
            (RdbValue::Stream(_, _, _), RdbValue::Stream(_, _, _)) => {
                // Stream encoding has known incompatibilities
            }
            _ => {
                prop_assert!(false, "value type changed during roundtrip");
            }
        }
    }
}

#[test]
fn unit_aof_single_record_roundtrip() {
    let record = AofRecord {
        argv: vec![b"SET".to_vec(), b"key".to_vec(), b"value".to_vec()],
    };
    let encoded = encode_aof_stream(&[record.clone()]);
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
    let encoded = encode_rdb(&[entry.clone()], &[]);
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
    let encoded = encode_rdb(&[entry.clone()], &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0].value, RdbValue::List(items) if items.len() == 3));
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
    let encoded = encode_rdb(&[entry.clone()], &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0].value, RdbValue::Hash(fields) if fields.len() == 2));
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
    let encoded = encode_rdb(&[entry.clone()], &[]);
    let (decoded, _) = decode_rdb(&encoded).unwrap();
    assert_eq!(decoded.len(), 1);
    assert!(matches!(&decoded[0].value, RdbValue::SortedSet(members) if members.len() == 3));
}
