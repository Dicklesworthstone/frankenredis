#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::{Store, StoreError, glob_match};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

const MAX_INPUT_LEN: usize = 4_096;
const MAX_CASES: usize = 64;
const MAX_EXTRA_STRINGS: usize = 8;
const MAX_HASH_FIELDS: usize = 8;
const MAX_SET_MEMBERS: usize = 8;
const MAX_ZSET_MEMBERS: usize = 8;
const MAX_BLOB_LEN: usize = 24;

const STRING_KEY: &[u8] = b"scan:string";
const LIST_KEY: &[u8] = b"scan:list";
const HASH_KEY: &[u8] = b"scan:hash";
const SET_KEY: &[u8] = b"scan:set";
const ZSET_KEY: &[u8] = b"scan:zset";
const STREAM_KEY: &[u8] = b"scan:stream";
const WRONG_TYPE_KEY: &[u8] = STRING_KEY;
const MISSING_KEY: &[u8] = b"scan:missing";

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    extra_strings: Vec<Blob>,
    hash_fields: Vec<(Blob, Blob)>,
    set_members: Vec<Blob>,
    zset_members: Vec<(i16, Blob)>,
    queries: Vec<Query>,
}

#[derive(Debug, Clone, Arbitrary)]
struct Blob(Vec<u8>);

#[derive(Debug, Arbitrary)]
enum Query {
    Scan {
        cursor: u16,
        count: u8,
        pattern: PatternSpec,
    },
    Hscan {
        target: CollectionTarget,
        cursor: u16,
        count: u8,
        pattern: PatternSpec,
    },
    Sscan {
        target: CollectionTarget,
        cursor: u16,
        count: u8,
        pattern: PatternSpec,
    },
    Zscan {
        target: CollectionTarget,
        cursor: u16,
        count: u8,
        pattern: PatternSpec,
    },
}

#[derive(Debug, Clone, Arbitrary)]
enum PatternSpec {
    None,
    Raw(Blob),
    Prefix(Blob),
    Suffix(Blob),
    Contains(Blob),
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum CollectionTarget {
    Real,
    WrongType,
    Missing,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = FuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_scan_family(input);
});

fn fuzz_scan_family(input: FuzzInput) {
    let now_ms = 1_u64;
    let FuzzInput {
        extra_strings,
        hash_fields,
        set_members,
        zset_members,
        queries,
    } = input;
    let mut store = seeded_store(
        extra_strings,
        hash_fields,
        set_members,
        zset_members,
        now_ms,
    );

    for query in queries.into_iter().take(MAX_CASES) {
        match query {
            Query::Scan {
                cursor,
                count,
                pattern,
            } => {
                let pattern = materialize_pattern(&pattern);
                assert_scan_step(&mut store, cursor, count, pattern.as_deref(), now_ms);
                assert_scan_full_walk(&mut store, count, pattern.as_deref(), now_ms);
            }
            Query::Hscan {
                target,
                cursor,
                count,
                pattern,
            } => {
                let pattern = materialize_pattern(&pattern);
                assert_hscan_query(
                    &mut store,
                    target,
                    cursor,
                    count,
                    pattern.as_deref(),
                    now_ms,
                );
            }
            Query::Sscan {
                target,
                cursor,
                count,
                pattern,
            } => {
                let pattern = materialize_pattern(&pattern);
                assert_sscan_query(
                    &mut store,
                    target,
                    cursor,
                    count,
                    pattern.as_deref(),
                    now_ms,
                );
            }
            Query::Zscan {
                target,
                cursor,
                count,
                pattern,
            } => {
                let pattern = materialize_pattern(&pattern);
                assert_zscan_query(
                    &mut store,
                    target,
                    cursor,
                    count,
                    pattern.as_deref(),
                    now_ms,
                );
            }
        }
    }
}

fn seeded_store(
    extra_strings: Vec<Blob>,
    hash_fields: Vec<(Blob, Blob)>,
    set_members: Vec<Blob>,
    zset_members: Vec<(i16, Blob)>,
    now_ms: u64,
) -> Store {
    let mut store = Store::new();
    store.set(STRING_KEY.to_vec(), b"value".to_vec(), None, now_ms);
    let _ = store.lpush(LIST_KEY, &[b"item".to_vec()], now_ms);
    let _ = store.hset(HASH_KEY, b"field:a".to_vec(), b"value:a".to_vec(), now_ms);
    let _ = store.hset(HASH_KEY, b"field:b".to_vec(), b"value:b".to_vec(), now_ms);
    let _ = store.sadd(
        SET_KEY,
        &[
            b"member:a".to_vec(),
            b"member:b".to_vec(),
            b"member:c".to_vec(),
        ],
        now_ms,
    );
    let _ = store.zadd(
        ZSET_KEY,
        &[
            (1.0, b"z:a".to_vec()),
            (2.0, b"z:b".to_vec()),
            (3.0, b"z:c".to_vec()),
        ],
        now_ms,
    );
    let _ = store.xadd(
        STREAM_KEY,
        (1, 0),
        &[(b"f".to_vec(), b"1".to_vec())],
        now_ms,
    );

    for (index, blob) in extra_strings
        .into_iter()
        .take(MAX_EXTRA_STRINGS)
        .enumerate()
    {
        let mut key = format!("scan:extra:{index}:").into_bytes();
        key.extend(normalize_blob(blob));
        if key.len() > MAX_BLOB_LEN * 2 {
            key.truncate(MAX_BLOB_LEN * 2);
        }
        if key.is_empty() {
            key.extend_from_slice(b"scan:extra:fallback");
        }
        store.set(key, b"x".to_vec(), None, now_ms);
    }

    for (field, value) in hash_fields.into_iter().take(MAX_HASH_FIELDS) {
        let _ = store.hset(
            HASH_KEY,
            nonempty_blob(field, b"field"),
            nonempty_blob(value, b"value"),
            now_ms,
        );
    }

    let set_members: Vec<Vec<u8>> = set_members
        .into_iter()
        .take(MAX_SET_MEMBERS)
        .map(|blob| nonempty_blob(blob, b"member"))
        .collect();
    if !set_members.is_empty() {
        let _ = store.sadd(SET_KEY, &set_members, now_ms);
    }

    let zset_members: Vec<(f64, Vec<u8>)> = zset_members
        .into_iter()
        .take(MAX_ZSET_MEMBERS)
        .map(|(score, member)| (f64::from(score), nonempty_blob(member, b"member")))
        .collect();
    if !zset_members.is_empty() {
        let _ = store.zadd(ZSET_KEY, &zset_members, now_ms);
    }

    store
}

fn assert_scan_step(
    store: &mut Store,
    cursor: u16,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
) {
    let ordered = ordered_scan_keys(store, now_ms);
    let expected = expected_scan_batch(&ordered, cursor as usize, count as usize, pattern);
    let actual = store.scan(u64::from(cursor), pattern, usize::from(count), now_ms);
    assert_eq!(actual, expected);
    for key in &actual.1 {
        assert!(ordered.contains(key));
        if let Some(pattern) = pattern {
            assert!(glob_match(pattern, key));
        }
        assert!(store.exists(key, now_ms));
    }
}

fn assert_scan_full_walk(store: &mut Store, count: u8, pattern: Option<&[u8]>, now_ms: u64) {
    let ordered = ordered_scan_keys(store, now_ms);
    let expected: Vec<Vec<u8>> = ordered
        .iter()
        .filter(|key| pattern.is_none_or(|pat| glob_match(pat, key)))
        .cloned()
        .collect();

    let mut cursor = 0_u64;
    let mut seen = Vec::new();
    let mut steps = 0_usize;
    let max_steps = ordered.len().saturating_add(1);

    loop {
        let (next, batch) = store.scan(cursor, pattern, usize::from(count), now_ms);
        seen.extend(batch);
        steps += 1;
        assert!(steps <= max_steps);
        if next == 0 {
            break;
        }
        assert!(next > cursor);
        cursor = next;
    }

    assert_eq!(seen, expected);
    let dedup: BTreeSet<Vec<u8>> = seen.iter().cloned().collect();
    assert_eq!(dedup.len(), seen.len());
}

fn assert_hscan_query(
    store: &mut Store,
    target: CollectionTarget,
    cursor: u16,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
) {
    match target {
        CollectionTarget::Real => {
            let ordered = store.hgetall(HASH_KEY, now_ms).expect("seeded hash");
            let expected = expected_assoc_batch(&ordered, cursor as usize, count as usize, pattern);
            let actual = store.hscan(
                HASH_KEY,
                u64::from(cursor),
                pattern,
                usize::from(count),
                now_ms,
            );
            assert_eq!(actual, Ok(expected.clone()));
            assert_hash_full_walk(store, count, pattern, now_ms, ordered);
        }
        CollectionTarget::WrongType => {
            assert_eq!(
                store.hscan(
                    WRONG_TYPE_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Err(StoreError::WrongType)
            );
        }
        CollectionTarget::Missing => {
            assert_eq!(
                store.hscan(
                    MISSING_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Ok((0, Vec::new()))
            );
        }
    }
}

fn assert_sscan_query(
    store: &mut Store,
    target: CollectionTarget,
    cursor: u16,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
) {
    match target {
        CollectionTarget::Real => {
            let ordered = store.smembers(SET_KEY, now_ms).expect("seeded set");
            let expected = expected_scan_batch(&ordered, cursor as usize, count as usize, pattern);
            let actual = store.sscan(
                SET_KEY,
                u64::from(cursor),
                pattern,
                usize::from(count),
                now_ms,
            );
            assert_eq!(actual, Ok(expected.clone()));
            assert_member_full_walk(store, SET_KEY, count, pattern, now_ms, ordered);
        }
        CollectionTarget::WrongType => {
            assert_eq!(
                store.sscan(
                    WRONG_TYPE_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Err(StoreError::WrongType)
            );
        }
        CollectionTarget::Missing => {
            assert_eq!(
                store.sscan(
                    MISSING_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Ok((0, Vec::new()))
            );
        }
    }
}

fn assert_zscan_query(
    store: &mut Store,
    target: CollectionTarget,
    cursor: u16,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
) {
    match target {
        CollectionTarget::Real => {
            let ordered = store
                .zrange_withscores(ZSET_KEY, 0, -1, now_ms)
                .expect("seeded zset");
            let expected = expected_assoc_batch(&ordered, cursor as usize, count as usize, pattern);
            let actual = store.zscan(
                ZSET_KEY,
                u64::from(cursor),
                pattern,
                usize::from(count),
                now_ms,
            );
            assert_eq!(actual, Ok(expected.clone()));
            assert_zscan_full_walk(store, count, pattern, now_ms, ordered);
        }
        CollectionTarget::WrongType => {
            assert_eq!(
                store.zscan(
                    WRONG_TYPE_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Err(StoreError::WrongType)
            );
        }
        CollectionTarget::Missing => {
            assert_eq!(
                store.zscan(
                    MISSING_KEY,
                    u64::from(cursor),
                    pattern,
                    usize::from(count),
                    now_ms
                ),
                Ok((0, Vec::new()))
            );
        }
    }
}

fn assert_member_full_walk(
    store: &mut Store,
    key: &[u8],
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
    ordered: Vec<Vec<u8>>,
) {
    let expected: Vec<Vec<u8>> = ordered
        .iter()
        .filter(|member| pattern.is_none_or(|pat| glob_match(pat, member)))
        .cloned()
        .collect();
    let mut cursor = 0_u64;
    let mut seen = Vec::new();
    let mut steps = 0_usize;
    let max_steps = ordered.len().saturating_add(1);

    loop {
        let (next, batch) = store
            .sscan(key, cursor, pattern, usize::from(count), now_ms)
            .expect("seeded set scan");
        seen.extend(batch);
        steps += 1;
        assert!(steps <= max_steps);
        if next == 0 {
            break;
        }
        assert!(next > cursor);
        cursor = next;
    }

    assert_eq!(seen, expected);
    let dedup: BTreeSet<Vec<u8>> = seen.iter().cloned().collect();
    assert_eq!(dedup.len(), seen.len());
}

fn assert_hash_full_walk(
    store: &mut Store,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
    ordered: Vec<(Vec<u8>, Vec<u8>)>,
) {
    let expected: Vec<(Vec<u8>, Vec<u8>)> = ordered
        .iter()
        .filter(|(field, _)| pattern.is_none_or(|pat| glob_match(pat, field)))
        .cloned()
        .collect();

    let mut cursor = 0_u64;
    let mut seen = Vec::new();
    let mut steps = 0_usize;
    let max_steps = ordered.len().saturating_add(1);

    loop {
        let next_batch = store
            .hscan(HASH_KEY, cursor, pattern, usize::from(count), now_ms)
            .expect("seeded hash scan");

        seen.extend(next_batch.1);
        steps += 1;
        assert!(steps <= max_steps);
        if next_batch.0 == 0 {
            break;
        }
        assert!(next_batch.0 > cursor);
        cursor = next_batch.0;
    }

    assert_eq!(seen, expected);
    let dedup: BTreeSet<Vec<u8>> = seen.iter().map(|(field, _)| field.clone()).collect();
    assert_eq!(dedup.len(), seen.len());
}

fn assert_zscan_full_walk(
    store: &mut Store,
    count: u8,
    pattern: Option<&[u8]>,
    now_ms: u64,
    ordered: Vec<(Vec<u8>, f64)>,
) {
    let expected: Vec<(Vec<u8>, f64)> = ordered
        .iter()
        .filter(|(field, _)| pattern.is_none_or(|pat| glob_match(pat, field)))
        .cloned()
        .collect();

    let mut cursor = 0_u64;
    let mut seen = Vec::new();
    let mut steps = 0_usize;
    let max_steps = ordered.len().saturating_add(1);

    loop {
        let next_batch = store
            .zscan(ZSET_KEY, cursor, pattern, usize::from(count), now_ms)
            .expect("seeded zset scan");

        seen.extend(next_batch.1);
        steps += 1;
        assert!(steps <= max_steps);
        if next_batch.0 == 0 {
            break;
        }
        assert!(next_batch.0 > cursor);
        cursor = next_batch.0;
    }

    assert_eq!(seen, expected);
    let dedup: BTreeSet<Vec<u8>> = seen.iter().map(|(field, _)| field.clone()).collect();
    assert_eq!(dedup.len(), seen.len());
}

fn ordered_scan_keys(store: &mut Store, now_ms: u64) -> Vec<Vec<u8>> {
    let (_, keys) = store.scan(0, None, usize::MAX, now_ms);
    keys
}

fn expected_scan_batch(
    ordered: &[Vec<u8>],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
) -> (u64, Vec<Vec<u8>>) {
    if cursor >= ordered.len() {
        return (0, Vec::new());
    }

    let batch = count.max(1);
    let end = cursor.saturating_add(batch).min(ordered.len());
    let result = ordered[cursor..end]
        .iter()
        .filter(|item| pattern.is_none_or(|pat| glob_match(pat, item)))
        .cloned()
        .collect();
    let next = if end >= ordered.len() { 0 } else { end as u64 };
    (next, result)
}

fn expected_assoc_batch<T: Clone>(
    ordered: &[(Vec<u8>, T)],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
) -> (u64, Vec<(Vec<u8>, T)>) {
    if cursor >= ordered.len() {
        return (0, Vec::new());
    }

    let batch = count.max(1);
    let end = cursor.saturating_add(batch).min(ordered.len());
    let result = ordered[cursor..end]
        .iter()
        .filter(|(field, _)| pattern.is_none_or(|pat| glob_match(pat, field)))
        .cloned()
        .collect();
    let next = if end >= ordered.len() { 0 } else { end as u64 };
    (next, result)
}

fn materialize_pattern(pattern: &PatternSpec) -> Option<Vec<u8>> {
    match pattern {
        PatternSpec::None => None,
        PatternSpec::Raw(blob) => Some(normalize_blob(blob.clone())),
        PatternSpec::Prefix(blob) => {
            let mut pattern = nonempty_blob(blob.clone(), b"*");
            pattern.push(b'*');
            Some(pattern)
        }
        PatternSpec::Suffix(blob) => {
            let mut pattern = vec![b'*'];
            pattern.extend(nonempty_blob(blob.clone(), b"*"));
            Some(pattern)
        }
        PatternSpec::Contains(blob) => {
            let mut pattern = vec![b'*'];
            pattern.extend(nonempty_blob(blob.clone(), b"*"));
            pattern.push(b'*');
            Some(pattern)
        }
    }
}

fn normalize_blob(blob: Blob) -> Vec<u8> {
    truncate_bytes(blob.0, MAX_BLOB_LEN)
}

fn nonempty_blob(blob: Blob, fallback: &[u8]) -> Vec<u8> {
    let bytes = normalize_blob(blob);
    if bytes.is_empty() {
        fallback.to_vec()
    } else {
        bytes
    }
}

fn truncate_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    bytes.truncate(max_len);
    bytes
}
