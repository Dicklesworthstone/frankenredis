//! Metamorphic invariants for stream operations (XADD/XRANGE/XLEN/XDEL).
//!
//! Existence-style properties that don't require a live oracle but
//! catch refactor drift in the rax cursor walk, the high-watermark
//! counter (`stream_last_ids`), and the entries-added bookkeeping.
//!
//! (frankenredis-ipz4)

use fr_store::{Store, StreamField, StreamId};

const NOW: u64 = 0;
const FIELDS: &[StreamField] = &[];

fn fresh() -> Store {
    Store::new()
}

fn fields() -> Vec<StreamField> {
    vec![(b"f".to_vec(), b"v".to_vec())]
}

#[test]
fn mr_xlen_after_xadd_distinct_ids_equals_count() {
    // MR-XLEN-AFTER-XADD-DISTINCT: pushing N distinct IDs into an
    // empty stream yields xlen == N.
    let mut store = fresh();
    let f = fields();
    let ids: Vec<StreamId> = (1u64..=64).map(|i| (i, 0)).collect();
    for id in &ids {
        store.xadd(b"s", *id, &f, NOW).expect("xadd");
    }
    assert_eq!(store.xlen(b"s", NOW).unwrap(), ids.len());
}

#[test]
fn mr_xrange_returns_entries_in_nondecreasing_id_order() {
    // MR-XRANGE-MONOTONIC: regardless of insertion order, XRANGE walks
    // the rax in non-decreasing StreamId order.
    let mut store = fresh();
    let f = fields();
    // Insert in deliberately mixed order to exercise the rax sort.
    let inserted: Vec<StreamId> = vec![
        (10, 0),
        (5, 0),
        (10, 1),
        (3, 99),
        (7, 0),
        (10, 0), // duplicate; semantics: store.xadd returns Ok and
                 // overwrites the fields (entries.insert.is_none() is
                 // false → not counted as new).
    ];
    for id in &inserted {
        store.xadd(b"s", *id, &f, NOW).expect("xadd");
    }
    let records = store
        .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, NOW)
        .expect("xrange");
    let ids: Vec<StreamId> = records.iter().map(|(id, _)| *id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "xrange must yield non-decreasing IDs");

    // Also: xrange is monotonic across windowed slices.
    let head = store
        .xrange(b"s", (0, 0), (5, 0), None, NOW)
        .expect("xrange head");
    let head_ids: Vec<StreamId> = head.iter().map(|(id, _)| *id).collect();
    let mut head_sorted = head_ids.clone();
    head_sorted.sort();
    assert_eq!(head_ids, head_sorted);
}

#[test]
fn mr_xadd_overwrite_with_existing_id_does_not_grow_xlen() {
    // MR-XADD-OVERWRITE-IDEMPOTENT: re-inserting an existing ID
    // overwrites fields but leaves the entry count unchanged.
    let mut store = fresh();
    let f1 = vec![(b"f".to_vec(), b"v1".to_vec())];
    let f2 = vec![(b"f".to_vec(), b"v2".to_vec())];

    store.xadd(b"s", (1, 0), &f1, NOW).expect("first add");
    let len_after_first = store.xlen(b"s", NOW).unwrap();
    assert_eq!(len_after_first, 1);

    store.xadd(b"s", (1, 0), &f2, NOW).expect("overwrite add");
    let len_after_overwrite = store.xlen(b"s", NOW).unwrap();
    assert_eq!(len_after_overwrite, 1, "overwrite must not grow xlen");

    // The fields ARE rewritten — fetch via xrange and confirm.
    let records = store
        .xrange(b"s", (1, 0), (1, 0), None, NOW)
        .expect("xrange");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1, f2, "second add overwrote fields");
}

#[test]
fn mr_xlen_after_xdel_decreases_by_count_of_present_ids() {
    // MR-XLEN-AFTER-XDEL: xdel reduces xlen by exactly the number of
    // unique IDs that were present prior to deletion. IDs that don't
    // exist are counted as zero deletes.
    let mut store = fresh();
    let f = fields();
    let inserted: Vec<StreamId> = (1u64..=10).map(|i| (i, 0)).collect();
    for id in &inserted {
        store.xadd(b"s", *id, &f, NOW).expect("xadd");
    }
    assert_eq!(store.xlen(b"s", NOW).unwrap(), 10);

    let to_delete = vec![(2, 0), (5, 0), (99, 99), (5, 0)]; // 99-99 missing,
                                                             // 5-0 listed twice
    let removed = store.xdel(b"s", &to_delete, NOW).expect("xdel");
    // Upstream XDEL counts each ACTUALLY removed entry exactly once
    // even if the same ID is listed twice; missing IDs don't count.
    assert_eq!(removed, 2_usize, "removed = unique-present(2-0, 5-0)");
    assert_eq!(store.xlen(b"s", NOW).unwrap(), 8);
}

#[test]
fn mr_xdel_repeated_call_is_no_op_for_already_deleted_ids() {
    // MR-XDEL-MISSING-IDS-NOOP: calling xdel a second time with the
    // same ids yields zero actually-deleted entries and leaves xlen
    // unchanged.
    let mut store = fresh();
    let f = fields();
    let ids: Vec<StreamId> = (1u64..=5).map(|i| (i, 0)).collect();
    for id in &ids {
        store.xadd(b"s", *id, &f, NOW).expect("xadd");
    }

    let removed_first = store.xdel(b"s", &ids, NOW).expect("xdel first");
    assert_eq!(removed_first, ids.len());
    assert_eq!(store.xlen(b"s", NOW).unwrap(), 0);

    // Second xdel call on the same IDs: nothing left to delete.
    let removed_second = store.xdel(b"s", &ids, NOW).expect("xdel second");
    assert_eq!(removed_second, 0_usize, "no-op when IDs already gone");
    assert_eq!(store.xlen(b"s", NOW).unwrap(), 0);

    // After all entries deleted, the stream object remains; subsequent
    // XADD with a fresh higher ID succeeds and the stream rebuilds.
    store.xadd(b"s", (100, 0), &f, NOW).expect("post-clear xadd");
    assert_eq!(store.xlen(b"s", NOW).unwrap(), 1);
}

#[test]
fn mr_xrange_includes_xadd_immediately() {
    // MR-XADD-XRANGE-INCLUDES: an ID inserted by XADD is observable
    // via XRANGE on the next call (no eventual-consistency lag).
    let mut store = fresh();
    let f = fields();
    for i in 1u64..=20 {
        store.xadd(b"s", (i, 0), &f, NOW).expect("xadd");
        let records = store
            .xrange(b"s", (i, 0), (i, 0), None, NOW)
            .expect("xrange");
        assert_eq!(records.len(), 1, "newly added (id={i}-0) must be visible");
        assert_eq!(records[0].0, (i, 0));
    }
}

// Mark FIELDS used so cargo doesn't warn — this const is a placeholder
// for tests that need an empty fields slice in the future.
#[allow(dead_code)]
fn _silence_fields_unused() -> &'static [StreamField] {
    FIELDS
}
