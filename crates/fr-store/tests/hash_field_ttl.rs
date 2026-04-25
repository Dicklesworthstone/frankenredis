//! Hash field TTL storage primitives (Redis 7.4 HEXPIRE family).
//!
//! Part 1 of br-frankenredis-rv89 — tests the Store-level primitives
//! without any command-layer or lazy-expiry integration.
//! (br-frankenredis-wwz3)

use fr_store::{
    HashFieldPersistResult, HashFieldTtl, HashFieldTtlCondition, HashFieldTtlSet, HashFieldTtlUnit,
    Store,
};

const NOW: u64 = 1_700_000_000_000; // 2023-11-14

fn seed_hash(store: &mut Store, key: &[u8], fields: &[(&[u8], &[u8])]) {
    for (f, v) in fields {
        store.hset(key, f.to_vec(), v.to_vec(), NOW).expect("hset");
    }
}

// ── Set + get roundtrip ────────────────────────────────────────────

#[test]
fn hash_field_ttl_set_unconditional_then_read_ms() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    let set =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(set, HashFieldTtlSet::Applied);

    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(60_000));

    let abs = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, true);
    assert_eq!(abs, HashFieldTtl::Remaining(NOW + 60_000));
}

#[test]
fn hash_field_ttl_read_seconds_rounds_remaining_up() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 600, // 600 ms
        HashFieldTtlCondition::None,
        NOW,
    );
    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Seconds, false);
    assert_eq!(
        ttl,
        HashFieldTtl::Remaining(1),
        "600ms should round up to 1s"
    );

    let abs = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Seconds, true);
    // Absolute seconds truncates: floor((NOW+600) / 1000).
    assert_eq!(abs, HashFieldTtl::Remaining((NOW + 600) / 1000));
}

// ── Missing / WRONGTYPE paths ──────────────────────────────────────

#[test]
fn hash_field_ttl_missing_key_returns_key_missing() {
    let mut store = Store::new();
    let set = store.hash_field_set_abs_expiry(
        b"missing",
        b"f",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );
    assert_eq!(set, HashFieldTtlSet::KeyMissing);

    let ttl = store.hash_field_ttl(b"missing", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::KeyMissing);

    let persist = store.hash_field_persist(b"missing", b"f");
    assert_eq!(persist, HashFieldPersistResult::KeyMissing);
}

#[test]
fn hash_field_ttl_missing_field_returns_field_missing() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"only", b"v")]);
    let set = store.hash_field_set_abs_expiry(
        b"h",
        b"nofield",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );
    assert_eq!(set, HashFieldTtlSet::FieldMissing);

    let ttl = store.hash_field_ttl(b"h", b"nofield", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::FieldMissing);

    let persist = store.hash_field_persist(b"h", b"nofield");
    assert_eq!(persist, HashFieldPersistResult::FieldMissing);
}

#[test]
fn hash_field_ttl_wrong_type_is_surfaced_distinctly() {
    let mut store = Store::new();
    store.set(b"s".to_vec(), b"a string".to_vec(), None, NOW);

    let set =
        store.hash_field_set_abs_expiry(b"s", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(set, HashFieldTtlSet::WrongType);

    let ttl = store.hash_field_ttl(b"s", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::WrongType);

    let persist = store.hash_field_persist(b"s", b"f");
    assert_eq!(persist, HashFieldPersistResult::WrongType);
}

// ── NX / XX / GT / LT flag matrix ──────────────────────────────────

#[test]
fn hash_field_ttl_nx_applies_only_when_no_ttl() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    let first =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::Nx, NOW);
    assert_eq!(first, HashFieldTtlSet::Applied);

    let second =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 120_000, HashFieldTtlCondition::Nx, NOW);
    assert_eq!(second, HashFieldTtlSet::ConditionNotMet);

    // The original TTL stands.
    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::Remaining(60_000));
}

#[test]
fn hash_field_ttl_xx_applies_only_when_ttl_exists() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    let blocked =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::Xx, NOW);
    assert_eq!(blocked, HashFieldTtlSet::ConditionNotMet);

    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 30_000, HashFieldTtlCondition::None, NOW);
    let allowed =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::Xx, NOW);
    assert_eq!(allowed, HashFieldTtlSet::Applied);
}

#[test]
fn hash_field_ttl_gt_requires_existing_and_stricter_deadline() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    // No existing TTL → GT is a no-op.
    let no_prior =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::Gt, NOW);
    assert_eq!(no_prior, HashFieldTtlSet::ConditionNotMet);

    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);

    let shorter = store.hash_field_set_abs_expiry(
        b"h",
        b"f",
        NOW + 30_000, // earlier deadline
        HashFieldTtlCondition::Gt,
        NOW,
    );
    assert_eq!(shorter, HashFieldTtlSet::ConditionNotMet);

    let longer =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 120_000, HashFieldTtlCondition::Gt, NOW);
    assert_eq!(longer, HashFieldTtlSet::Applied);
}

#[test]
fn hash_field_ttl_lt_applies_when_no_existing_or_stricter_deadline() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    // No existing TTL → LT applies (anything is less than "infinity").
    let no_prior =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::Lt, NOW);
    assert_eq!(no_prior, HashFieldTtlSet::Applied);

    let longer =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 120_000, HashFieldTtlCondition::Lt, NOW);
    assert_eq!(longer, HashFieldTtlSet::ConditionNotMet);

    let shorter =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW + 30_000, HashFieldTtlCondition::Lt, NOW);
    assert_eq!(shorter, HashFieldTtlSet::Applied);
}

// ── Past-deadline + persist ─────────────────────────────────────────

#[test]
fn hash_field_ttl_set_to_past_deadline_reports_already_expired() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);

    let set =
        store.hash_field_set_abs_expiry(b"h", b"f", NOW - 1, HashFieldTtlCondition::None, NOW);
    assert_eq!(set, HashFieldTtlSet::AppliedAlreadyExpired);
    assert!(store.hash_field_is_expired(b"h", b"f", NOW));
}

#[test]
fn hash_field_persist_reports_no_ttl_when_never_set() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    let persist = store.hash_field_persist(b"h", b"f");
    assert_eq!(persist, HashFieldPersistResult::NoTtl);
}

#[test]
fn hash_field_persist_clears_existing_ttl_and_is_idempotent() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);

    assert_eq!(
        store.hash_field_persist(b"h", b"f"),
        HashFieldPersistResult::Persisted
    );
    // Second call → NoTtl (the field still exists).
    assert_eq!(
        store.hash_field_persist(b"h", b"f"),
        HashFieldPersistResult::NoTtl
    );
    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::NoTtl);
}

// ── Read for field with no TTL returns NoTtl ───────────────────────

#[test]
fn hash_field_ttl_read_without_prior_set_returns_no_ttl() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    let ttl = store.hash_field_ttl(b"h", b"f", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(ttl, HashFieldTtl::NoTtl);
}

// ── Cleanup when the whole key or field is deleted ──────────────────

#[test]
fn hash_field_ttl_clear_for_key_removes_every_field_entry() {
    let mut store = Store::new();
    seed_hash(
        &mut store,
        b"h",
        &[(b"f1", b"v1"), (b"f2", b"v2"), (b"f3", b"v3")],
    );
    for f in [b"f1".as_slice(), b"f2".as_slice(), b"f3".as_slice()] {
        store.hash_field_set_abs_expiry(b"h", f, NOW + 60_000, HashFieldTtlCondition::None, NOW);
    }
    assert_eq!(store.hash_field_ttl_carrier_count(), 1);

    store.hash_field_ttl_clear_for_key(b"h");

    for f in [b"f1".as_slice(), b"f2".as_slice(), b"f3".as_slice()] {
        let ttl = store.hash_field_ttl(b"h", f, NOW, HashFieldTtlUnit::Milliseconds, false);
        assert_eq!(ttl, HashFieldTtl::NoTtl);
    }
    assert_eq!(store.hash_field_ttl_carrier_count(), 0);
}

#[test]
fn hash_field_ttl_clear_for_field_is_targeted() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"keep", b"v"), (b"drop", b"v")]);
    store.hash_field_set_abs_expiry(
        b"h",
        b"keep",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );
    store.hash_field_set_abs_expiry(
        b"h",
        b"drop",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );

    store.hash_field_ttl_clear_for_field(b"h", b"drop");

    let keep_ttl = store.hash_field_ttl(b"h", b"keep", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(keep_ttl, HashFieldTtl::Remaining(60_000));
    let drop_ttl = store.hash_field_ttl(b"h", b"drop", NOW, HashFieldTtlUnit::Milliseconds, false);
    assert_eq!(drop_ttl, HashFieldTtl::NoTtl);
}

// ── Carrier count ───────────────────────────────────────────────────

// ── Keyspace events (part 4b: br-frankenredis-7jhg) ────────────────

fn enable_all_keyspace_events(store: &mut Store) {
    // K + E + g + h + x bits → covers hset / hexpire / hexpired streams.
    store.notify_keyspace_events = 0xFFFF;
    let _ = store.drain_keyspace_notifications();
}

fn collect_event_pairs(store: &mut Store) -> Vec<(String, String)> {
    store
        .drain_keyspace_notifications()
        .into_iter()
        .map(|(chan, key)| {
            (
                String::from_utf8_lossy(&chan).into_owned(),
                String::from_utf8_lossy(&key).into_owned(),
            )
        })
        .collect()
}

#[test]
fn hash_field_set_with_event_emits_hexpire_on_applied() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    enable_all_keyspace_events(&mut store);

    let outcome = store.hash_field_set_abs_expiry_with_event(
        b"h",
        b"f",
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
        "hexpire",
    );
    assert_eq!(outcome, HashFieldTtlSet::Applied);

    let events = collect_event_pairs(&mut store);
    // Expect both __keyspace@0__:h → hexpire and __keyevent@0__:hexpire → h.
    assert!(
        events
            .iter()
            .any(|(c, k)| c == "__keyspace@0__:h" && k == "hexpire")
    );
    assert!(
        events
            .iter()
            .any(|(c, k)| c == "__keyevent@0__:hexpire" && k == "h")
    );
}

#[test]
fn hash_field_set_with_event_emits_on_already_expired() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    enable_all_keyspace_events(&mut store);

    let outcome = store.hash_field_set_abs_expiry_with_event(
        b"h",
        b"f",
        NOW - 1,
        HashFieldTtlCondition::None,
        NOW,
        "hpexpireat",
    );
    assert_eq!(outcome, HashFieldTtlSet::AppliedAlreadyExpired);
    let events = collect_event_pairs(&mut store);
    assert!(events.iter().any(|(_, k)| k == "hpexpireat"));
}

#[test]
fn hash_field_set_with_event_suppresses_event_on_condition_miss() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    // Set initial TTL so an NX attempt blocks.
    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    enable_all_keyspace_events(&mut store);
    let outcome = store.hash_field_set_abs_expiry_with_event(
        b"h",
        b"f",
        NOW + 120_000,
        HashFieldTtlCondition::Nx,
        NOW,
        "hexpire",
    );
    assert_eq!(outcome, HashFieldTtlSet::ConditionNotMet);
    let events = collect_event_pairs(&mut store);
    assert!(
        events.is_empty(),
        "no event on blocked condition: {events:?}"
    );
}

#[test]
fn hash_field_persist_with_event_emits_hpersist_on_clear() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    enable_all_keyspace_events(&mut store);
    let outcome = store.hash_field_persist_with_event(b"h", b"f");
    assert_eq!(outcome, HashFieldPersistResult::Persisted);
    let events = collect_event_pairs(&mut store);
    assert!(events.iter().any(|(_, k)| k == "hpersist"));
}

#[test]
fn hash_field_persist_with_event_suppresses_event_when_no_ttl() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    enable_all_keyspace_events(&mut store);
    let outcome = store.hash_field_persist_with_event(b"h", b"f");
    assert_eq!(outcome, HashFieldPersistResult::NoTtl);
    let events = collect_event_pairs(&mut store);
    assert!(events.is_empty(), "no event on no-op persist: {events:?}");
}

// ── OBJECT ENCODING / listpack_ex / hashtable_ex (part 4a) ─────────

#[test]
fn object_encoding_reports_listpack_ex_for_hash_with_any_field_ttl() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    // No TTL yet → plain listpack.
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack"));

    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(store.object_encoding(b"h", NOW), Some("listpack_ex"));
}

#[test]
fn object_encoding_reports_hashtable_ex_when_big_hash_has_field_ttl() {
    let mut store = Store::new();
    // Seed >128 entries to push over the listpack threshold.
    let fields: Vec<Vec<u8>> = (0..140_u32).map(|i| format!("f{i}").into_bytes()).collect();
    for f in &fields {
        store
            .hset(b"h", f.clone(), b"v".to_vec(), NOW)
            .expect("hset");
    }
    assert_eq!(store.object_encoding(b"h", NOW), Some("hashtable"));

    store.hash_field_set_abs_expiry(
        b"h",
        &fields[0],
        NOW + 60_000,
        HashFieldTtlCondition::None,
        NOW,
    );
    assert_eq!(store.object_encoding(b"h", NOW), Some("hashtable_ex"));
}

// ── Lazy-expiry hook (part 3: br-frankenredis-b8ut) ────────────────
//
// Once a per-field TTL lapses, every hash-read surface must act as if
// the field is gone, and the entries map + hash_field_expires must both
// be cleaned up. A hash with no remaining fields after reaping is
// removed entirely.

#[test]
fn hget_reaps_expired_field_returning_none() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"alive", b"v"), (b"doomed", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"doomed", NOW - 1, HashFieldTtlCondition::None, NOW);
    // Past-deadline write already marked the field as reaped in the
    // field_expires map; the first read (via hget on the doomed field)
    // must drop it from the hash too.
    let v = store.hget(b"h", b"doomed", NOW).expect("hget");
    assert_eq!(v, None);
    // Subsequent hget on a non-expired field still returns its value.
    let alive = store.hget(b"h", b"alive", NOW).expect("hget alive");
    assert_eq!(alive, Some(b"v".to_vec()));
}

#[test]
fn hgetall_hkeys_hvals_hide_expired_fields() {
    let mut store = Store::new();
    seed_hash(
        &mut store,
        b"h",
        &[(b"keep", b"k"), (b"drop1", b"d"), (b"drop2", b"d")],
    );
    store.hash_field_set_abs_expiry(b"h", b"drop1", NOW - 1, HashFieldTtlCondition::None, NOW);
    store.hash_field_set_abs_expiry(b"h", b"drop2", NOW - 5, HashFieldTtlCondition::None, NOW);

    let all = store.hgetall(b"h", NOW).expect("hgetall");
    assert_eq!(all, vec![(b"keep".to_vec(), b"k".to_vec())]);

    let keys = store.hkeys(b"h", NOW).expect("hkeys");
    assert_eq!(keys, vec![b"keep".to_vec()]);

    let vals = store.hvals(b"h", NOW).expect("hvals");
    assert_eq!(vals, vec![b"k".to_vec()]);

    assert_eq!(store.hlen(b"h", NOW).expect("hlen"), 1);
}

#[test]
fn hmget_returns_none_for_fields_that_have_just_expired() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"alive", b"v"), (b"dead", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"dead", NOW - 1, HashFieldTtlCondition::None, NOW);
    let got = store
        .hmget(b"h", &[b"alive", b"dead", b"nope"], NOW)
        .expect("hmget");
    assert_eq!(got, vec![Some(b"v".to_vec()), None, None]);
}

#[test]
fn hexists_treats_expired_field_as_missing() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"dead", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"dead", NOW - 1, HashFieldTtlCondition::None, NOW);
    assert!(!store.hexists(b"h", b"dead", NOW).expect("hexists"));
}

#[test]
fn reaping_last_field_deletes_the_whole_hash_key() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"only", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"only", NOW - 1, HashFieldTtlCondition::None, NOW);
    // HLEN triggers a sweep that reaps the only field and should also
    // drop the hash key itself.
    let len = store.hlen(b"h", NOW).expect("hlen");
    assert_eq!(len, 0);
    // Directly asserting the key vanished — no raw getter for this so
    // use key_type which returns None for missing keys.
    let ty = store.key_type(b"h", NOW);
    assert_eq!(ty, None);
}

#[test]
fn hdel_clears_the_per_field_ttl_entry() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(store.hash_field_ttl_carrier_count(), 1);
    // HDEL drops the field AND its TTL entry.
    let removed = store.hdel(b"h", &[&b"f"[..]], NOW).expect("hdel");
    assert_eq!(removed, 1);
    assert_eq!(store.hash_field_ttl_carrier_count(), 0);
}

#[test]
fn whole_key_removal_clears_all_field_ttls() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h", &[(b"f1", b"v"), (b"f2", b"v")]);
    store.hash_field_set_abs_expiry(b"h", b"f1", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    store.hash_field_set_abs_expiry(b"h", b"f2", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(store.hash_field_ttl_carrier_count(), 1);
    assert_eq!(store.del(&[b"h".to_vec()], NOW), 1);
    assert_eq!(store.hash_field_ttl_carrier_count(), 0);
}

#[test]
fn hash_field_ttl_carrier_count_tracks_distinct_keys() {
    let mut store = Store::new();
    seed_hash(&mut store, b"h1", &[(b"f", b"v")]);
    seed_hash(&mut store, b"h2", &[(b"f", b"v")]);
    seed_hash(&mut store, b"h3", &[(b"f", b"v")]);

    assert_eq!(store.hash_field_ttl_carrier_count(), 0);

    store.hash_field_set_abs_expiry(b"h1", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    store.hash_field_set_abs_expiry(b"h2", b"f", NOW + 60_000, HashFieldTtlCondition::None, NOW);
    assert_eq!(store.hash_field_ttl_carrier_count(), 2);

    store.hash_field_persist(b"h1", b"f");
    // h1 no longer carries a TTL; h2 still does.
    assert_eq!(store.hash_field_ttl_carrier_count(), 1);
}
