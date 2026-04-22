#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::{Store, StoreError};
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_OPS: usize = 64;
const MAX_BATCH_ITEMS: usize = 12;
const MAX_BLOB_LEN: usize = 32;
const HLL_SLOT_COUNT: usize = 4;

const HLL_KEYS: [&[u8]; HLL_SLOT_COUNT] =
    [b"fuzz:hll:0", b"fuzz:hll:1", b"fuzz:hll:2", b"fuzz:hll:3"];
const WRONG_TYPE_KEY: &[u8] = b"fuzz:hll:wrong";
const INVALID_HLL_KEY: &[u8] = b"fuzz:hll:invalid";
const ROUNDTRIP_KEY: &[u8] = b"fuzz:hll:roundtrip";
const TEMP_ALL_SOURCES_KEY: &[u8] = b"fuzz:hll:merge:all";
const TEMP_UNIQUE_SOURCES_KEY: &[u8] = b"fuzz:hll:merge:unique";
const MISSING_KEY: &[u8] = b"fuzz:hll:missing";

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    ops: Vec<HllOp>,
}

#[derive(Debug, Arbitrary)]
enum HllOp {
    Add {
        key: u8,
        elements: Vec<Blob>,
    },
    Count {
        source_count: u8,
        first: SourceRef,
        second: SourceRef,
        third: SourceRef,
    },
    Merge {
        dest: u8,
        source_count: u8,
        first: SourceRef,
        second: SourceRef,
        third: SourceRef,
    },
    PoisonInvalid {
        bytes: Blob,
    },
    WrongType {
        op: ErrorOp,
        dest: u8,
        source_count: u8,
        first: SourceRef,
        second: SourceRef,
        third: SourceRef,
        elements: Vec<Blob>,
    },
    InvalidHll {
        op: ErrorOp,
        dest: u8,
        source_count: u8,
        first: SourceRef,
        second: SourceRef,
        third: SourceRef,
        elements: Vec<Blob>,
    },
    RoundTrip {
        key: u8,
        ttl_ms: u16,
    },
}

#[derive(Debug, Clone, Arbitrary)]
struct Blob(Vec<u8>);

#[derive(Debug, Clone, Copy, Arbitrary)]
enum SourceRef {
    Slot(u8),
    Missing,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum ErrorOp {
    Add,
    Count,
    MergeSource,
    MergeDest,
}

#[derive(Debug)]
struct ErrorCase {
    op: ErrorOp,
    dest: &'static [u8],
    source_count: u8,
    sources: [SourceRef; 3],
    elements: Vec<Blob>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = FuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_store_hll(input);
});

fn fuzz_store_hll(input: FuzzInput) {
    let mut store = Store::new();
    store
        .hset(WRONG_TYPE_KEY, b"field".to_vec(), b"value".to_vec(), 0)
        .expect("wrong-type sentinel must initialize");
    poison_invalid_hll_key(&mut store, b"", 0);

    let mut now_ms = 1_u64;
    for (step_index, op) in input.ops.into_iter().take(MAX_OPS).enumerate() {
        apply_op(&mut store, op, now_ms);
        now_ms = now_ms.saturating_add(1 + (step_index % 7) as u64);
    }

    let _ = store.to_aof_commands(now_ms);
}

fn apply_op(store: &mut Store, op: HllOp, now_ms: u64) {
    match op {
        HllOp::Add { key, elements } => {
            let key = slot_key(slot_index(key));
            let elements = normalize_elements(elements);
            let before_count = store
                .pfcount(&[key], now_ms)
                .expect("slot count should work");
            let _ = store
                .pfadd(key, &elements, now_ms)
                .expect("slot add should work");
            let after_count = store
                .pfcount(&[key], now_ms)
                .expect("slot count should work");
            assert!(
                after_count >= before_count,
                "PFADD should never shrink the approximate cardinality"
            );

            let digest_after_first = store.state_digest();
            let payload_after_first = dump_payload(store, key, now_ms);
            assert_eq!(
                store.pfadd(key, &elements, now_ms),
                Ok(false),
                "re-adding the same batch should not mutate the HLL"
            );
            assert_eq!(
                store
                    .pfcount(&[key], now_ms)
                    .expect("slot count should work"),
                after_count,
                "re-adding the same batch should keep the estimate stable"
            );
            assert_eq!(
                dump_payload(store, key, now_ms),
                payload_after_first,
                "re-adding the same batch should preserve the serialized HLL"
            );
            assert_eq!(
                store.state_digest(),
                digest_after_first,
                "re-adding the same batch should not mutate the store digest"
            );
        }
        HllOp::Count {
            source_count,
            first,
            second,
            third,
        } => {
            let source_keys = build_source_keys(source_count, [first, second, third]);
            let unique_source_keys = dedupe_source_keys(&source_keys);
            let count_all = store
                .pfcount(&source_keys, now_ms)
                .expect("counting HLL sources should work");
            let count_unique = store
                .pfcount(&unique_source_keys, now_ms)
                .expect("counting unique HLL sources should work");
            assert_eq!(
                count_all, count_unique,
                "duplicate PFCOUNT sources should not change the estimate"
            );

            for key in unique_source_keys {
                if key == MISSING_KEY {
                    continue;
                }
                let single = store.pfcount(&[key], now_ms).expect("single slot count");
                assert!(
                    count_all >= single,
                    "PFCOUNT unions should not undercount an individual source"
                );
            }
        }
        HllOp::Merge {
            dest,
            source_count,
            first,
            second,
            third,
        } => {
            let dest = slot_key(slot_index(dest));
            let source_keys = build_source_keys(source_count, [first, second, third]);
            let unique_source_keys = dedupe_source_keys(&source_keys);
            let expected_union = store
                .pfcount(&source_keys, now_ms)
                .expect("counting merge sources should work");
            let expected_unique_union = store
                .pfcount(&unique_source_keys, now_ms)
                .expect("counting unique merge sources should work");
            assert_eq!(
                expected_union, expected_unique_union,
                "duplicate PFMERGE sources should not change the union estimate"
            );

            store
                .pfmerge(dest, &source_keys, now_ms)
                .expect("merge into slot should work");
            let after_merge = store.pfcount(&[dest], now_ms).expect("dest count");
            assert_eq!(
                after_merge, expected_union,
                "PFMERGE destination should match PFCOUNT over the source union"
            );

            let payload_after_first = dump_payload(store, dest, now_ms);
            store
                .pfmerge(dest, &source_keys, now_ms)
                .expect("repeating the same merge should work");
            assert_eq!(
                store.pfcount(&[dest], now_ms).expect("dest count"),
                after_merge,
                "repeating the same merge should keep the estimate stable"
            );
            assert_eq!(
                dump_payload(store, dest, now_ms),
                payload_after_first,
                "repeating the same merge should preserve the serialized HLL"
            );

            clear_keys(
                store,
                &[TEMP_ALL_SOURCES_KEY, TEMP_UNIQUE_SOURCES_KEY],
                now_ms,
            );
            store
                .pfmerge(TEMP_ALL_SOURCES_KEY, &source_keys, now_ms)
                .expect("temp merge with duplicates should work");
            store
                .pfmerge(TEMP_UNIQUE_SOURCES_KEY, &unique_source_keys, now_ms)
                .expect("temp merge with unique sources should work");
            assert_eq!(
                dump_payload(store, TEMP_ALL_SOURCES_KEY, now_ms),
                dump_payload(store, TEMP_UNIQUE_SOURCES_KEY, now_ms),
                "merging duplicate sources should be byte-for-byte stable"
            );
        }
        HllOp::PoisonInvalid { bytes } => {
            poison_invalid_hll_key(store, &bytes.0, now_ms);
        }
        HllOp::WrongType {
            op,
            dest,
            source_count,
            first,
            second,
            third,
            elements,
        } => {
            assert_error_preserves_hll_state(
                store,
                WRONG_TYPE_KEY,
                StoreError::WrongType,
                ErrorCase {
                    op,
                    dest: slot_key(slot_index(dest)),
                    source_count,
                    sources: [first, second, third],
                    elements,
                },
                now_ms,
            );
        }
        HllOp::InvalidHll {
            op,
            dest,
            source_count,
            first,
            second,
            third,
            elements,
        } => {
            assert_error_preserves_hll_state(
                store,
                INVALID_HLL_KEY,
                StoreError::InvalidHllValue,
                ErrorCase {
                    op,
                    dest: slot_key(slot_index(dest)),
                    source_count,
                    sources: [first, second, third],
                    elements,
                },
                now_ms,
            );
        }
        HllOp::RoundTrip { key, ttl_ms } => {
            let key = slot_key(slot_index(key));
            let Some(payload) = store.dump_key(key, now_ms) else {
                return;
            };
            let expected_count = store.pfcount(&[key], now_ms).expect("slot count");
            store
                .restore_key(ROUNDTRIP_KEY, u64::from(ttl_ms), &payload, true, now_ms)
                .expect("self-generated HLL dump should restore");
            assert_eq!(
                store
                    .pfcount(&[ROUNDTRIP_KEY], now_ms)
                    .expect("round-trip count"),
                expected_count,
                "DUMP/RESTORE should preserve the approximate HLL count"
            );
            assert_eq!(
                store.dump_key(ROUNDTRIP_KEY, now_ms),
                Some(payload),
                "DUMP/RESTORE should preserve the serialized HLL payload"
            );
        }
    }
}

fn assert_error_preserves_hll_state(
    store: &mut Store,
    error_key: &[u8],
    expected_error: StoreError,
    case: ErrorCase,
    now_ms: u64,
) {
    let before_digest = store.state_digest();
    let elements = normalize_elements(case.elements);
    let source_keys = build_source_keys(case.source_count, case.sources);
    let result = match case.op {
        ErrorOp::Add => store.pfadd(error_key, &elements, now_ms).map(|_| ()),
        ErrorOp::Count => {
            let mut keys = Vec::with_capacity(source_keys.len() + 1);
            keys.push(error_key);
            keys.extend(source_keys.iter().copied());
            store.pfcount(&keys, now_ms).map(|_| ())
        }
        ErrorOp::MergeSource => {
            let mut keys = Vec::with_capacity(source_keys.len() + 1);
            keys.push(error_key);
            keys.extend(source_keys.iter().copied());
            store.pfmerge(case.dest, &keys, now_ms)
        }
        ErrorOp::MergeDest => store.pfmerge(error_key, &source_keys, now_ms),
    };

    assert_eq!(
        result,
        Err(expected_error),
        "hostile PF* error paths should keep their documented error precedence"
    );
    assert_eq!(
        store.state_digest(),
        before_digest,
        "hostile PF* error paths should not mutate existing HLL state"
    );
}

fn poison_invalid_hll_key(store: &mut Store, bytes: &[u8], now_ms: u64) {
    let mut payload = b"bad:hll:".to_vec();
    payload.extend(truncate_bytes(bytes.to_vec(), MAX_BLOB_LEN));
    store.set(INVALID_HLL_KEY.to_vec(), payload, None, now_ms);
}

fn normalize_elements(elements: Vec<Blob>) -> Vec<Vec<u8>> {
    elements
        .into_iter()
        .take(MAX_BATCH_ITEMS)
        .map(|Blob(bytes)| truncate_bytes(bytes, MAX_BLOB_LEN))
        .collect()
}

fn build_source_keys(source_count: u8, refs: [SourceRef; 3]) -> Vec<&'static [u8]> {
    refs.into_iter()
        .take(normalize_source_count(source_count))
        .map(resolve_source_key)
        .collect()
}

fn dedupe_source_keys<'a>(keys: &[&'a [u8]]) -> Vec<&'a [u8]> {
    let mut unique = Vec::new();
    for key in keys {
        if !unique.contains(key) {
            unique.push(*key);
        }
    }
    unique
}

fn clear_keys(store: &mut Store, keys: &[&[u8]], now_ms: u64) {
    let owned = keys.iter().map(|key| (*key).to_vec()).collect::<Vec<_>>();
    let _ = store.del(&owned, now_ms);
}

fn dump_payload(store: &mut Store, key: &[u8], now_ms: u64) -> Option<Vec<u8>> {
    store.dump_key(key, now_ms)
}

fn resolve_source_key(source: SourceRef) -> &'static [u8] {
    match source {
        SourceRef::Slot(index) => slot_key(slot_index(index)),
        SourceRef::Missing => MISSING_KEY,
    }
}

fn slot_index(index: u8) -> usize {
    usize::from(index) % HLL_SLOT_COUNT
}

fn slot_key(index: usize) -> &'static [u8] {
    HLL_KEYS[index]
}

fn normalize_source_count(source_count: u8) -> usize {
    usize::from(source_count % 3) + 1
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}
