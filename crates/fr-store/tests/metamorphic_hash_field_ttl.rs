//! Metamorphic invariants for the Redis 7.4 hash-field TTL family
//! (HEXPIRE / HPEXPIRE / HEXPIREAT / HPEXPIREAT / HTTL / HPTTL /
//! HEXPIRETIME / HPEXPIRETIME / HPERSIST). The unit-test surface in
//! `tests/hash_field_ttl.rs` covers primitives; this file pins
//! existence-style invariants that catch refactor drift on the
//! NX/XX/GT/LT flag matrix and on HDEL → field_expires cleanup.
//!
//! (frankenredis-k7zm8)

use fr_store::{
    HashFieldPersistResult, HashFieldTtl, HashFieldTtlCondition, HashFieldTtlSet,
    HashFieldTtlUnit, Store,
};

const NOW: u64 = 1_000_000;

fn fresh_with_field() -> Store {
    let mut store = Store::new();
    store
        .hset(b"h", b"f".to_vec(), b"v".to_vec(), NOW)
        .expect("hset");
    store
}

#[test]
fn mr_hexpire_httl_identity_on_fresh_field() {
    // After setting a TTL of N ms on a field with no prior TTL,
    // HTTL must report ≈ N/1000 (rounded up).
    let mut store = fresh_with_field();
    let target = NOW + 60_000; // +60s from NOW
    let outcome = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        target,
        HashFieldTtlCondition::None,
        NOW,
    );
    assert!(matches!(outcome, HashFieldTtlSet::Applied));

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Seconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(60));

    let ttl_ms = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl_ms, HashFieldTtl::Remaining(60_000));
}

#[test]
fn mr_hpersist_returns_field_to_no_ttl() {
    // After HPERSIST, the field's TTL must read as NoTtl (-1 in the
    // command surface) — never as Remaining or Expired.
    let mut store = fresh_with_field();
    let _ = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );

    let persisted = store.hash_field_persist(b"h", b"f");
    assert!(matches!(persisted, HashFieldPersistResult::Persisted));

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Seconds, false);
    assert_eq!(ttl, HashFieldTtl::NoTtl);
    assert!(
        !store
            .hash_field_expires
            .contains_key(&(b"h".to_vec(), b"f".to_vec())),
        "hash_field_expires row must be gone after HPERSIST"
    );
}

#[test]
fn mr_hexpire_nx_idempotent_after_initial_apply() {
    // HEXPIRE NX on a field that already has a TTL is a no-op —
    // ConditionNotMet — and the prior TTL stands.
    let mut store = fresh_with_field();
    let first = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 10_000,
        HashFieldTtlCondition::Nx,
        NOW,
    );
    assert!(matches!(first, HashFieldTtlSet::Applied));

    let again = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 999_999,
        HashFieldTtlCondition::Nx,
        NOW,
    );
    assert!(matches!(again, HashFieldTtlSet::ConditionNotMet));

    // Original TTL stands.
    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(10_000));
}

#[test]
fn mr_hexpire_xx_no_op_on_field_without_ttl() {
    // HEXPIRE XX on a field that has NO existing TTL is a no-op.
    // The field stays at NoTtl regardless of how aggressive the
    // proposed deadline is.
    let mut store = fresh_with_field();
    let outcome = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 60_000,
        HashFieldTtlCondition::Xx,
        NOW,
    );
    assert!(matches!(outcome, HashFieldTtlSet::ConditionNotMet));

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Seconds, false);
    assert_eq!(ttl, HashFieldTtl::NoTtl);
}

#[test]
fn mr_hexpire_lt_walks_down_to_minimum_proposed_deadline() {
    // Sequential HEXPIRE LT calls with strictly decreasing proposed
    // deadlines must each apply, ending at the minimum.
    let mut store = fresh_with_field();
    // Seed at a "high" deadline.
    let _ = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 1_000_000,
        HashFieldTtlCondition::None,
        NOW,
    );

    let mut last_seen = u64::MAX;
    for proposed_offset in [500_000_u64, 100_000, 50_000, 1_000] {
        let outcome = store.hash_field_set_abs_expiry(
            b"h",
            b"f",
            NOW + proposed_offset,
            HashFieldTtlCondition::Lt,
            NOW,
        );
        assert!(
            matches!(outcome, HashFieldTtlSet::Applied),
            "LT with smaller proposed offset {proposed_offset} should apply, got {outcome:?}"
        );
        last_seen = proposed_offset;
    }
    assert_eq!(last_seen, 1_000);

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(1_000));

    // A *higher* proposed deadline must NOT apply (LT only allows
    // strictly-lower).
    let no_op = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 5_000,
        HashFieldTtlCondition::Lt,
        NOW,
    );
    assert!(matches!(no_op, HashFieldTtlSet::ConditionNotMet));

    let ttl_after = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl_after, HashFieldTtl::Remaining(1_000));
}

#[test]
fn mr_hexpire_gt_walks_up_to_maximum_proposed_deadline() {
    // Sequential HEXPIRE GT calls with strictly increasing proposed
    // deadlines must each apply, ending at the maximum.
    let mut store = fresh_with_field();
    // Seed at a "low" deadline.
    let _ = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 1_000,
        HashFieldTtlCondition::None,
        NOW,
    );

    let mut last_seen = 0_u64;
    for proposed_offset in [5_000_u64, 50_000, 500_000, 999_999] {
        let outcome = store.hash_field_set_abs_expiry(
            b"h",
            b"f",
            NOW + proposed_offset,
            HashFieldTtlCondition::Gt,
            NOW,
        );
        assert!(
            matches!(outcome, HashFieldTtlSet::Applied),
            "GT with larger proposed offset {proposed_offset} should apply, got {outcome:?}"
        );
        last_seen = proposed_offset;
    }
    assert_eq!(last_seen, 999_999);

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(999_999));

    // A *lower* proposed deadline must NOT apply (GT only allows
    // strictly-higher).
    let no_op = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 100,
        HashFieldTtlCondition::Gt,
        NOW,
    );
    assert!(matches!(no_op, HashFieldTtlSet::ConditionNotMet));
}

#[test]
fn mr_hdel_clears_per_field_ttl_row() {
    // HDEL on a field with a TTL must drop the (key, field) row from
    // hash_field_expires so subsequent HSET of the same field starts
    // fresh.
    let mut store = fresh_with_field();
    let _ = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );
    assert!(
        store
            .hash_field_expires
            .contains_key(&(b"h".to_vec(), b"f".to_vec())),
        "ttl row should exist before HDEL"
    );

    let removed = store.hdel(b"h", &[b"f".as_slice()], NOW).expect("hdel");
    assert_eq!(removed, 1);

    assert!(
        !store
            .hash_field_expires
            .contains_key(&(b"h".to_vec(), b"f".to_vec())),
        "hash_field_expires row must be gone after HDEL"
    );

    // Re-creating the field starts at NoTtl, not the prior 60_000.
    store
        .hset(b"h", b"f".to_vec(), b"v2".to_vec(), NOW + 1)
        .expect("hset re-create");
    let ttl = store.hash_field_ttl(b"h", b"f", NOW + 1, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::NoTtl);
}
