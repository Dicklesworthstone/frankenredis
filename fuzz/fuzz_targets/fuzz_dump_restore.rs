#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

use fr_store::{Store, StoreError};

const NOW_MS: u64 = 1_000;
const MAX_KEY_LEN: usize = 32;
const MAX_BLOB_LEN: usize = 64;
const MAX_COLLECTION_LEN: usize = 8;
const MAX_STREAM_ENTRIES: usize = 6;
const MAX_STREAM_FIELDS: usize = 6;
const MAX_RAW_PAYLOAD_LEN: usize = 2_048;

#[derive(Arbitrary, Debug)]
enum DumpRestoreInput {
    Valid(ValidDumpRestoreCase),
    Raw(RawRestoreCase),
}

#[derive(Arbitrary, Debug)]
struct ValidDumpRestoreCase {
    key: Vec<u8>,
    ttl_ms: Option<u16>,
    value: DumpValue,
}

#[derive(Arbitrary, Debug)]
enum DumpValue {
    String(Vec<u8>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    SortedSet(Vec<(i16, Vec<u8>)>),
    Stream(Vec<Vec<(Vec<u8>, Vec<u8>)>>),
}

#[derive(Arbitrary, Debug)]
struct RawRestoreCase {
    key: Vec<u8>,
    ttl_ms: u16,
    payload: Vec<u8>,
    sentinel: Vec<u8>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 4_096 {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = DumpRestoreInput::arbitrary(&mut unstructured) else {
        return;
    };

    match input {
        DumpRestoreInput::Valid(case) => fuzz_valid_roundtrip(case),
        DumpRestoreInput::Raw(case) => fuzz_raw_restore(case),
    }
});

fn fuzz_valid_roundtrip(case: ValidDumpRestoreCase) {
    let key = normalized_key(case.key);
    // TTL=0 causes immediate expiry, so use at least 1ms.
    // Add buffer to ensure key doesn't expire before dump_key is called.
    let ttl_ms = case.ttl_ms.map(|v| u64::from(v).max(1000));

    let mut original = Store::new();
    install_value(&mut original, &key, ttl_ms, case.value);
    let payload = original
        .dump_key(&key, NOW_MS)
        .expect("installed value must be dumpable");

    let mut restored = Store::new();
    restored
        .restore_key(&key, ttl_ms.unwrap_or(0), &payload, false, NOW_MS)
        .expect("self-generated DUMP payload must restore");

    let reencoded = restored
        .dump_key(&key, NOW_MS)
        .expect("restored value must remain dumpable");
    assert_eq!(
        payload, reencoded,
        "dump/restore round-trip must preserve the serialized payload"
    );
}

fn fuzz_raw_restore(case: RawRestoreCase) {
    let key = normalized_key(case.key);
    let payload = truncate_bytes(case.payload, MAX_RAW_PAYLOAD_LEN);
    let ttl_ms = u64::from(case.ttl_ms);
    let sentinel = nonempty_blob(case.sentinel, b"sentinel");

    let mut busy_store = Store::new();
    busy_store.set(key.clone(), sentinel.clone(), None, NOW_MS);
    let busy_result = busy_store.restore_key(&key, ttl_ms, &payload, false, NOW_MS);
    assert_eq!(
        busy_result,
        Err(StoreError::BusyKey),
        "REPLACE=no must preserve BusyKey precedence even for hostile payloads"
    );

    let mut replace_store = Store::new();
    replace_store.set(key.clone(), sentinel.clone(), None, NOW_MS);
    let before = replace_store
        .dump_key(&key, NOW_MS)
        .expect("sentinel key must be dumpable");
    let replace_result = replace_store.restore_key(&key, ttl_ms, &payload, true, NOW_MS);
    if replace_result.is_err() {
        let after = replace_store
            .dump_key(&key, NOW_MS)
            .expect("failed restore must not remove the original key");
        assert_eq!(
            before, after,
            "failed restore with REPLACE must not mutate the existing value"
        );
    }
}

fn install_value(store: &mut Store, key: &[u8], ttl_ms: Option<u64>, value: DumpValue) {
    match value {
        DumpValue::String(bytes) => {
            store.set(key.to_vec(), nonempty_blob(bytes, b"value"), ttl_ms, NOW_MS);
        }
        DumpValue::List(values) => {
            let values = nonempty_vec_vec(values, b"item");
            store
                .rpush(key, &values, NOW_MS)
                .expect("valid fuzz list setup must succeed");
            apply_optional_ttl(store, key, ttl_ms);
        }
        DumpValue::Set(values) => {
            let values = nonempty_vec_vec(values, b"member");
            store
                .sadd(key, &values, NOW_MS)
                .expect("valid fuzz set setup must succeed");
            apply_optional_ttl(store, key, ttl_ms);
        }
        DumpValue::Hash(fields) => {
            let fields = nonempty_pairs(fields, b"field", b"value");
            for (field, value) in fields {
                store
                    .hset(key, field, value, NOW_MS)
                    .expect("valid fuzz hash setup must succeed");
            }
            apply_optional_ttl(store, key, ttl_ms);
        }
        DumpValue::SortedSet(members) => {
            let members = nonempty_sorted_set(members);
            store
                .zadd(key, &members, NOW_MS)
                .expect("valid fuzz zset setup must succeed");
            apply_optional_ttl(store, key, ttl_ms);
        }
        DumpValue::Stream(entries) => {
            let entries = nonempty_stream_entries(entries);
            for (index, fields) in entries.into_iter().enumerate() {
                store
                    .xadd(key, (index as u64 + 1, 0), &fields, NOW_MS)
                    .expect("valid fuzz stream setup must succeed");
            }
            apply_optional_ttl(store, key, ttl_ms);
        }
    }
}

fn apply_optional_ttl(store: &mut Store, key: &[u8], ttl_ms: Option<u64>) {
    if let Some(ttl_ms) = ttl_ms {
        let ttl_ms = ttl_ms.max(1);
        assert!(
            store.expire_milliseconds(key, ttl_ms as i64, NOW_MS),
            "just-installed key must accept a fuzz TTL"
        );
    }
}

fn normalized_key(key: Vec<u8>) -> Vec<u8> {
    let mut key = truncate_bytes(key, MAX_KEY_LEN);
    if key.is_empty() {
        key.push(b'k');
    }
    key
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}

fn nonempty_blob(bytes: Vec<u8>, fallback: &[u8]) -> Vec<u8> {
    let bytes = truncate_bytes(bytes, MAX_BLOB_LEN);
    if bytes.is_empty() {
        fallback.to_vec()
    } else {
        bytes
    }
}

fn nonempty_vec_vec(mut values: Vec<Vec<u8>>, fallback: &[u8]) -> Vec<Vec<u8>> {
    values.truncate(MAX_COLLECTION_LEN);
    let mut values: Vec<Vec<u8>> = values
        .into_iter()
        .map(|value| nonempty_blob(value, fallback))
        .collect();
    if values.is_empty() {
        values.push(fallback.to_vec());
    }
    values
}

fn nonempty_pairs(
    mut pairs: Vec<(Vec<u8>, Vec<u8>)>,
    key_fallback: &[u8],
    value_fallback: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    pairs.truncate(MAX_COLLECTION_LEN);
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = pairs
        .into_iter()
        .map(|(key, value)| {
            (
                nonempty_blob(key, key_fallback),
                nonempty_blob(value, value_fallback),
            )
        })
        .collect();
    if pairs.is_empty() {
        pairs.push((key_fallback.to_vec(), value_fallback.to_vec()));
    }
    pairs
}

fn nonempty_sorted_set(mut members: Vec<(i16, Vec<u8>)>) -> Vec<(f64, Vec<u8>)> {
    members.truncate(MAX_COLLECTION_LEN);
    let mut members: Vec<(f64, Vec<u8>)> = members
        .into_iter()
        .map(|(score, member)| (f64::from(score), nonempty_blob(member, b"member")))
        .collect();
    if members.is_empty() {
        members.push((0.0, b"member".to_vec()));
    }
    members
}

fn nonempty_stream_entries(
    mut entries: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
) -> Vec<Vec<(Vec<u8>, Vec<u8>)>> {
    entries.truncate(MAX_STREAM_ENTRIES);
    let mut entries: Vec<Vec<(Vec<u8>, Vec<u8>)>> = entries
        .into_iter()
        .map(|mut fields| {
            fields.truncate(MAX_STREAM_FIELDS);
            let mut fields: Vec<(Vec<u8>, Vec<u8>)> = fields
                .into_iter()
                .map(|(field, value)| {
                    (
                        nonempty_blob(field, b"field"),
                        nonempty_blob(value, b"value"),
                    )
                })
                .collect();
            if fields.is_empty() {
                fields.push((b"field".to_vec(), b"value".to_vec()));
            }
            fields
        })
        .collect();
    if entries.is_empty() {
        entries.push(vec![(b"field".to_vec(), b"value".to_vec())]);
    }
    entries
}
