//! Metamorphic invariant for SWAPDB:
//!
//!   `swap_prefixes("db0:", "db1:")` applied twice must be the
//!   identity on every observable surface.
//!
//! Catches the class of bug fixed in ss2k0 + sdmwz — internal shadow
//! tables (ordered_keys, hash_field_expires, stream_*) being silently
//! dropped or orphaned across the swap. Failing this test means some
//! table didn't migrate (or migrated asymmetrically) on one of the
//! two swaps, which would surface as a value drop, TTL disappearance,
//! or stream-cursor regression for clients that snapshot DB state
//! across a SWAPDB cycle.
//!
//! (frankenredis-lxewj)

use fr_store::{Store, StreamField, StreamId};

const NOW: u64 = 0;

type FieldTtl = ((Vec<u8>, Vec<u8>), u64);
type HashState = (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);
type StreamIds = (Vec<u8>, Vec<StreamId>);

fn seed_db0(store: &mut Store) {
    // String
    store.set(
        b"db0:str".to_vec(),
        b"value-string".to_vec(),
        Some(60_000),
        NOW,
    );

    // Hash with per-field TTL (the sdmwz target)
    store
        .hset(b"db0:hash", b"f1".to_vec(), b"v1".to_vec(), NOW)
        .expect("hset f1");
    store
        .hset(b"db0:hash", b"f2".to_vec(), b"v2".to_vec(), NOW)
        .expect("hset f2");
    store
        .hash_field_expires
        .insert((b"db0:hash".to_vec(), b"f1".to_vec()), 99_000);

    // Sorted set
    store
        .zadd(
            b"db0:zset",
            &[(1.0, b"a".to_vec()), (2.5, b"b".to_vec())],
            NOW,
        )
        .expect("zadd");

    // Stream with two entries
    let f: Vec<StreamField> = vec![(b"k".to_vec(), b"v".to_vec())];
    store.xadd(b"db0:stream", (1, 0), &f, NOW).expect("xadd 1");
    store.xadd(b"db0:stream", (2, 0), &f, NOW).expect("xadd 2");
}

fn seed_db1(store: &mut Store) {
    // String already in destination prefix to make sure both directions
    // of the swap exercise the migration path.
    store.set(b"db1:other".to_vec(), b"untouched".to_vec(), None, NOW);
    store
        .hset(b"db1:hash2", b"a".to_vec(), b"b".to_vec(), NOW)
        .expect("hset right hash");
}

fn snapshot_observable(store: &mut Store) -> Snapshot {
    let mut keys = store.keys_matching(b"*", NOW);
    keys.sort();

    let mut hash_states: Vec<HashState> = Vec::new();
    let mut field_ttls: Vec<FieldTtl> = store
        .hash_field_expires
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    field_ttls.sort();

    for key in &keys {
        if let Ok(pairs) = store.hgetall(key, NOW)
            && !pairs.is_empty()
        {
            let mut sorted = pairs;
            sorted.sort();
            hash_states.push((key.clone(), sorted));
        }
    }

    let mut stream_lengths: Vec<(Vec<u8>, usize)> = Vec::new();
    let mut stream_ids: Vec<StreamIds> = Vec::new();
    for key in &keys {
        if let Ok(len) = store.xlen(key, NOW)
            && len > 0
        {
            stream_lengths.push((key.clone(), len));
            let records = store
                .xrange(key, (0, 0), (u64::MAX, u64::MAX), None, NOW)
                .expect("xrange");
            stream_ids.push((key.clone(), records.iter().map(|(id, _)| *id).collect()));
        }
    }

    Snapshot {
        keys,
        field_ttls,
        hash_states,
        stream_lengths,
        stream_ids,
    }
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    keys: Vec<Vec<u8>>,
    field_ttls: Vec<FieldTtl>,
    hash_states: Vec<HashState>,
    stream_lengths: Vec<(Vec<u8>, usize)>,
    stream_ids: Vec<StreamIds>,
}

#[test]
fn mr_swapdb_twice_is_identity_across_string_hash_zset_stream() {
    let mut store = Store::new();
    seed_db0(&mut store);
    seed_db1(&mut store);

    let before = snapshot_observable(&mut store);

    // First swap: db0 ↔ db1
    let touched_first = store.swap_prefixes(b"db0:", b"db1:");
    assert!(
        touched_first > 0,
        "first swap_prefixes must touch >= 1 key; got {touched_first}"
    );

    // Sanity: things actually moved.
    assert!(
        store.exists(b"db1:str", NOW),
        "db1:str must exist after first swap"
    );
    assert!(
        !store.exists(b"db0:str", NOW),
        "db0:str must be empty after first swap"
    );

    // Second swap restores the original namespace.
    let _ = store.swap_prefixes(b"db0:", b"db1:");

    let after = snapshot_observable(&mut store);

    assert_eq!(
        before, after,
        "SWAPDB applied twice must be the identity on every observable surface"
    );
}
