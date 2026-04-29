use fr_store::{Store, crc16_slot, encode_db_key};
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

fn tagged_key(prefix: &[u8], tag: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(prefix.len() + tag.len() + suffix.len() + 2);
    key.extend_from_slice(prefix);
    key.push(b'{');
    key.extend_from_slice(tag);
    key.push(b'}');
    key.extend_from_slice(suffix);
    key
}

fn whole_key_slot(key: &[u8]) -> u16 {
    crc16_xmodem(key) & 0x3FFF
}

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[test]
fn cluster_slot_empty_hashtag_falls_back_to_whole_key() {
    let key = b"foo{}{bar}";
    assert_eq!(crc16_slot(key), whole_key_slot(key));
}

#[test]
fn cluster_slot_missing_closing_brace_falls_back_to_whole_key() {
    let key = b"foo{bar";
    assert_eq!(crc16_slot(key), whole_key_slot(key));
}

#[test]
fn cluster_slot_double_open_brace_matches_redis_reference() {
    assert_eq!(crc16_slot(b"foo{{bar}}zap"), crc16_slot(b"{bar"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    // MR1: SET/GET roundtrip - SET k v then GET k must return v
    #[test]
    fn mr_set_get_roundtrip(key in prop::collection::vec(any::<u8>(), 1..64),
                            value in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);
        let got = store.get(&key, 0).unwrap();
        prop_assert_eq!(got, Some(value));
    }

    // MR2: INCR/DECR identity - INCRBY n then DECRBY n leaves value unchanged
    #[test]
    fn mr_incr_decr_identity(key in prop::collection::vec(any::<u8>(), 1..32),
                             initial in -1_000_000i64..1_000_000i64,
                             delta in 1i64..100_000i64) {
        let mut store = fresh_store();
        store.set(key.clone(), initial.to_string().into_bytes(), None, 0);

        let after_incr = store.incrby(&key, delta, 0).unwrap();
        let after_decr = store.incrby(&key, -delta, 0).unwrap();

        prop_assert_eq!(after_decr, initial, "INCR then DECR should restore original");
        prop_assert_eq!(after_incr, initial + delta);
    }

    // MR3: List LPUSH/RPOP queue behavior - first pushed is first popped from right
    #[test]
    fn mr_list_lpush_rpop_queue(key in prop::collection::vec(any::<u8>(), 1..32),
                                values in prop::collection::vec(
                                    prop::collection::vec(any::<u8>(), 1..64),
                                    1..10
                                )) {
        let mut store = fresh_store();

        // LPUSH all values (they go to head)
        for v in &values {
            store.lpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        // RPOP should return them in original order (FIFO from right)
        for expected in &values {
            let got = store.rpop(&key, 0).unwrap();
            prop_assert_eq!(got.as_ref(), Some(expected));
        }

        // List should now be empty
        prop_assert_eq!(store.llen(&key, 0).unwrap(), 0);
    }

    // MR4: Set membership - SADD then SISMEMBER must return true
    #[test]
    fn mr_set_membership(key in prop::collection::vec(any::<u8>(), 1..32),
                         members in prop::collection::vec(
                             prop::collection::vec(any::<u8>(), 1..64),
                             1..20
                         )) {
        let mut store = fresh_store();

        store.sadd(&key, &members, 0).unwrap();

        for member in &members {
            let is_member = store.sismember(&key, member, 0).unwrap();
            prop_assert!(is_member, "SADD member then SISMEMBER must be true");
        }

        let unique_members = members.iter().collect::<std::collections::BTreeSet<_>>().len();
        prop_assert_eq!(store.scard(&key, 0).unwrap(), unique_members);
    }

    // MR5: Hash field roundtrip - HSET then HGET must return same value
    #[test]
    fn mr_hash_field_roundtrip(key in prop::collection::vec(any::<u8>(), 1..32),
                               field in prop::collection::vec(any::<u8>(), 1..32),
                               value in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut store = fresh_store();

        store.hset(&key, field.clone(), value.clone(), 0).unwrap();
        let got = store.hget(&key, &field, 0).unwrap();

        prop_assert_eq!(got, Some(value));
    }

    // MR6: ZADD/ZSCORE roundtrip - score is preserved
    #[test]
    fn mr_zset_score_roundtrip(key in prop::collection::vec(any::<u8>(), 1..32),
                               member in prop::collection::vec(any::<u8>(), 1..32),
                               score in -1e10f64..1e10f64) {
        prop_assume!(score.is_finite());

        let mut store = fresh_store();
        store.zadd(&key, &[(score, member.clone())], 0).unwrap();

        let got_score = store.zscore(&key, &member, 0).unwrap();
        prop_assert_eq!(got_score, Some(score));
    }

    // MR7: DEL removes key - SET then DEL then EXISTS must be false
    #[test]
    fn mr_del_removes_key(key in prop::collection::vec(any::<u8>(), 1..32),
                          value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut store = fresh_store();

        store.set(key.clone(), value, None, 0);
        prop_assert!(store.exists(&key, 0));

        let deleted = store.del(std::slice::from_ref(&key), 0);
        prop_assert_eq!(deleted, 1);

        prop_assert!(!store.exists(&key, 0), "DEL should remove key");
    }

    // MR8: RENAME preserves value - SET k v; RENAME k k2; GET k2 == v
    #[test]
    fn mr_rename_preserves_value(src in prop::collection::vec(any::<u8>(), 1..32),
                                  dst in prop::collection::vec(any::<u8>(), 1..32),
                                  value in prop::collection::vec(any::<u8>(), 1..64)) {
        prop_assume!(src != dst);

        let mut store = fresh_store();
        store.set(src.clone(), value.clone(), None, 0);

        store.rename(&src, &dst, 0).unwrap();

        prop_assert!(!store.exists(&src, 0), "source key should be gone after RENAME");
        let got = store.get(&dst, 0).unwrap();
        prop_assert_eq!(got, Some(value), "destination should have original value");
    }

    // MR9: APPEND concatenation - SET "a"; APPEND "b"; GET == "ab"
    #[test]
    fn mr_append_concatenates(key in prop::collection::vec(any::<u8>(), 1..32),
                              prefix in prop::collection::vec(any::<u8>(), 0..64),
                              suffix in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut store = fresh_store();

        store.set(key.clone(), prefix.clone(), None, 0);
        let new_len = store.append(&key, &suffix, 0).unwrap();

        let mut expected = prefix;
        expected.extend_from_slice(&suffix);

        prop_assert_eq!(new_len, expected.len());
        let got = store.get(&key, 0).unwrap().unwrap();
        prop_assert_eq!(got, expected);
    }

    // MR10: GETSET returns old value and sets new
    #[test]
    fn mr_getset_returns_old(key in prop::collection::vec(any::<u8>(), 1..32),
                             old_value in prop::collection::vec(any::<u8>(), 1..64),
                             new_value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut store = fresh_store();

        store.set(key.clone(), old_value.clone(), None, 0);
        let got_old = store.getset(key.clone(), new_value.clone(), 0).unwrap();

        prop_assert_eq!(got_old, Some(old_value));
        let got_new = store.get(&key, 0).unwrap();
        prop_assert_eq!(got_new, Some(new_value));
    }

    // MR11: SETNX only sets if not exists
    #[test]
    fn mr_setnx_respects_existing(key in prop::collection::vec(any::<u8>(), 1..32),
                                   first in prop::collection::vec(any::<u8>(), 1..64),
                                   second in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut store = fresh_store();

        let set1 = store.setnx(key.clone(), first.clone(), 0);
        prop_assert!(set1, "first SETNX should succeed");

        let set2 = store.setnx(key.clone(), second, 0);
        prop_assert!(!set2, "second SETNX should fail");

        let got = store.get(&key, 0).unwrap();
        prop_assert_eq!(got, Some(first), "original value should be preserved");
    }

    // MR12: LPUSH/LLEN consistency - length equals number of pushes
    #[test]
    fn mr_list_len_consistency(key in prop::collection::vec(any::<u8>(), 1..32),
                               count in 1usize..50) {
        let mut store = fresh_store();

        for i in 0..count {
            store.lpush(&key, &[vec![i as u8]], 0).unwrap();
        }

        prop_assert_eq!(store.llen(&key, 0).unwrap(), count);
    }

    // MR13: SREM then SISMEMBER must be false
    #[test]
    fn mr_srem_removes_member(key in prop::collection::vec(any::<u8>(), 1..32),
                              member in prop::collection::vec(any::<u8>(), 1..32)) {
        let mut store = fresh_store();

        store.sadd(&key, std::slice::from_ref(&member), 0).unwrap();
        prop_assert!(store.sismember(&key, &member, 0).unwrap());

        store.srem(&key, &[member.as_slice()], 0).unwrap();
        prop_assert!(!store.sismember(&key, &member, 0).unwrap(), "SREM should remove member");
    }

    // MR14: HDEL removes field
    #[test]
    fn mr_hdel_removes_field(key in prop::collection::vec(any::<u8>(), 1..32),
                             field in prop::collection::vec(any::<u8>(), 1..32),
                             value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut store = fresh_store();

        store.hset(&key, field.clone(), value, 0).unwrap();
        prop_assert!(store.hexists(&key, &field, 0).unwrap());

        store.hdel(&key, &[field.as_slice()], 0).unwrap();
        prop_assert!(!store.hexists(&key, &field, 0).unwrap(), "HDEL should remove field");
    }

    // MR15: ZREM removes member
    #[test]
    fn mr_zrem_removes_member(key in prop::collection::vec(any::<u8>(), 1..32),
                              member in prop::collection::vec(any::<u8>(), 1..32),
                              score in -1000.0f64..1000.0f64) {
        prop_assume!(score.is_finite());

        let mut store = fresh_store();

        store.zadd(&key, &[(score, member.clone())], 0).unwrap();
        prop_assert!(store.zscore(&key, &member, 0).unwrap().is_some());

        store.zrem(&key, &[member.as_slice()], 0).unwrap();
        prop_assert!(store.zscore(&key, &member, 0).unwrap().is_none(), "ZREM should remove member");
    }

    // MR16: Multiple INCR is additive
    #[test]
    fn mr_incr_additive(key in prop::collection::vec(any::<u8>(), 1..32),
                        deltas in prop::collection::vec(-1000i64..1000i64, 1..20)) {
        let mut store = fresh_store();
        store.set(key.clone(), b"0".to_vec(), None, 0);

        let mut expected: i64 = 0;
        for delta in &deltas {
            store.incrby(&key, *delta, 0).unwrap();
            expected += delta;
        }

        let got = store.get(&key, 0).unwrap().unwrap();
        let got_val: i64 = String::from_utf8(got).unwrap().parse().unwrap();
        prop_assert_eq!(got_val, expected);
    }

    // MR17: COPY preserves value without modifying source
    #[test]
    fn mr_copy_preserves_both(src in prop::collection::vec(any::<u8>(), 1..32),
                              dst in prop::collection::vec(any::<u8>(), 1..32),
                              value in prop::collection::vec(any::<u8>(), 1..64)) {
        prop_assume!(src != dst);

        let mut store = fresh_store();
        store.set(src.clone(), value.clone(), None, 0);

        let copied = store.copy(&src, &dst, false, 0);
        prop_assert!(copied.is_ok());

        // Source still exists with same value
        let src_val = store.get(&src, 0).unwrap();
        prop_assert_eq!(src_val, Some(value.clone()));

        // Destination has same value
        let dst_val = store.get(&dst, 0).unwrap();
        prop_assert_eq!(dst_val, Some(value));
    }

    // MR18: STRLEN equals value length
    #[test]
    fn mr_strlen_equals_len(key in prop::collection::vec(any::<u8>(), 1..32),
                            value in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let strlen = store.strlen(&key, 0).unwrap();
        prop_assert_eq!(strlen, value.len());
    }

    // MR19: HLEN equals number of fields
    #[test]
    fn mr_hlen_equals_field_count(key in prop::collection::vec(any::<u8>(), 1..32),
                                   fields in prop::collection::vec(
                                       (prop::collection::vec(any::<u8>(), 1..16),
                                        prop::collection::vec(any::<u8>(), 1..32)),
                                       1..20
                                   )) {
        let mut store = fresh_store();

        // Use a set to track unique fields
        let mut unique_fields = std::collections::HashSet::new();
        for (field, value) in fields {
            store.hset(&key, field.clone(), value, 0).unwrap();
            unique_fields.insert(field);
        }

        let hlen = store.hlen(&key, 0).unwrap();
        prop_assert_eq!(hlen, unique_fields.len());
    }

    // MR20: EXISTS is false for nonexistent key
    #[test]
    fn mr_exists_false_for_nonexistent(key in prop::collection::vec(any::<u8>(), 1..32)) {
        let mut store = fresh_store();
        prop_assert!(!store.exists_no_touch(&key, 0));
    }

    // MR21: EXPIRE seconds and milliseconds are equivalent when deadlines match exactly.
    #[test]
    fn mr_expire_seconds_matches_expire_milliseconds(
        key in prop::collection::vec(any::<u8>(), 1..32),
        value in prop::collection::vec(any::<u8>(), 1..128),
        ttl_seconds in 1u32..600,
        set_now in 0u64..10_000,
        observe_delta_ms in 0u64..1_200_000
    ) {
        let ttl_ms = u64::from(ttl_seconds) * 1_000;
        let observe_now = set_now.saturating_add(observe_delta_ms);

        let mut seconds_store = fresh_store();
        seconds_store.set(key.clone(), value.clone(), None, set_now);
        prop_assert!(seconds_store.expire_seconds(&key, i64::from(ttl_seconds), set_now));

        let mut milliseconds_store = fresh_store();
        milliseconds_store.set(key.clone(), value, None, set_now);
        prop_assert!(milliseconds_store.expire_milliseconds(&key, ttl_ms as i64, set_now));

        prop_assert_eq!(
            seconds_store.get(&key, observe_now).unwrap(),
            milliseconds_store.get(&key, observe_now).unwrap(),
            "second and millisecond expiry forms should be observationally equivalent"
        );
        prop_assert_eq!(
            seconds_store.pttl(&key, observe_now),
            milliseconds_store.pttl(&key, observe_now),
            "matching expiry deadlines should yield matching remaining TTLs"
        );
    }

    // MR22: RENAME and COPY preserve the absolute expiry deadline rather than resetting it.
    #[test]
    fn mr_rename_and_copy_preserve_absolute_expiry_deadline(
        src in prop::collection::vec(any::<u8>(), 1..32),
        dst in prop::collection::vec(any::<u8>(), 1..32),
        value in prop::collection::vec(any::<u8>(), 1..128),
        ttl_ms in 1u64..20_000,
        op_elapsed_ms in 0u64..20_000,
        observe_extra_ms in 0u64..20_000
    ) {
        prop_assume!(src != dst);
        prop_assume!(op_elapsed_ms < ttl_ms);

        let set_now = 1_000u64;
        let op_now = set_now.saturating_add(op_elapsed_ms);
        let observe_now = op_now.saturating_add(observe_extra_ms);

        let mut baseline = fresh_store();
        baseline.set(src.clone(), value.clone(), Some(ttl_ms), set_now);
        let baseline_value = baseline.get(&src, observe_now).unwrap();
        let baseline_pttl = baseline.pttl(&src, observe_now);

        let mut renamed = fresh_store();
        renamed.set(src.clone(), value.clone(), Some(ttl_ms), set_now);
        renamed.rename(&src, &dst, op_now).unwrap();
        prop_assert_eq!(renamed.get(&src, observe_now).unwrap(), None);
        prop_assert_eq!(renamed.get(&dst, observe_now).unwrap(), baseline_value.clone());
        prop_assert_eq!(renamed.pttl(&dst, observe_now), baseline_pttl);

        let mut copied = fresh_store();
        copied.set(src.clone(), value, Some(ttl_ms), set_now);
        prop_assert!(copied.copy(&src, &dst, false, op_now).unwrap());
        prop_assert_eq!(copied.get(&src, observe_now).unwrap(), baseline_value.clone());
        prop_assert_eq!(copied.get(&dst, observe_now).unwrap(), baseline_value);

        let copied_src_pttl = copied.pttl(&src, observe_now);
        let copied_dst_pttl = copied.pttl(&dst, observe_now);
        prop_assert_eq!(copied_src_pttl, baseline_pttl);
        prop_assert_eq!(copied_dst_pttl, baseline_pttl);
    }

    // MR23: Redis hashtag extraction ignores wrapper differences once the same
    // first non-empty tag is selected.
    #[test]
    fn mr_cluster_hashtag_dominates_wrapper_variations(
        left_a in prop::collection::vec(any::<u8>().prop_filter("no opening brace", |b| *b != b'{'), 0..8),
        left_b in prop::collection::vec(any::<u8>().prop_filter("no opening brace", |b| *b != b'{'), 0..8),
        tag in prop::collection::vec(any::<u8>().prop_filter("non-empty tag cannot contain closing brace", |b| *b != b'}'), 1..16),
        right_a in prop::collection::vec(any::<u8>(), 0..8),
        right_b in prop::collection::vec(any::<u8>(), 0..8)
    ) {
        let key_a = tagged_key(&left_a, &tag, &right_a);
        let key_b = tagged_key(&left_b, &tag, &right_b);

        prop_assert_eq!(crc16_slot(&key_a), crc16_slot(&key_b));
        prop_assert_eq!(crc16_slot(&key_a), crc16_slot(&tag));
    }

    // MR24: Only the first valid hashtag contributes to the slot, and the
    // internal DB namespace prefix must not perturb that selection.
    #[test]
    fn mr_cluster_first_valid_hashtag_survives_later_tags_and_db_namespacing(
        db in 1usize..16,
        prefix in prop::collection::vec(any::<u8>().prop_filter("no opening brace", |b| *b != b'{'), 0..8),
        first_tag in prop::collection::vec(any::<u8>().prop_filter("first tag cannot contain closing brace", |b| *b != b'}'), 1..16),
        middle in prop::collection::vec(any::<u8>(), 0..8),
        second_tag in prop::collection::vec(any::<u8>().prop_filter("second tag cannot contain closing brace", |b| *b != b'}'), 1..16),
        suffix in prop::collection::vec(any::<u8>(), 0..8)
    ) {
        let mut key = tagged_key(&prefix, &first_tag, &middle);
        key.extend_from_slice(&tagged_key(b"", &second_tag, &suffix));

        prop_assert_eq!(crc16_slot(&key), crc16_slot(&first_tag));

        let encoded = encode_db_key(db, &key);
        prop_assert_eq!(crc16_slot(&encoded), crc16_slot(&first_tag));
    }

    // MR25: XADD/XLEN consistency - stream length equals number of added entries
    #[test]
    fn mr_stream_xadd_xlen_consistency(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..1000, prop::collection::vec(any::<u8>(), 1..32)),
            1..20
        )
    ) {
        let mut store = fresh_store();

        let mut added_ids = std::collections::BTreeSet::new();
        for (i, (ms, seq, field_data)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            if added_ids.insert(id) {
                let fields = vec![(b"field".to_vec(), field_data.clone())];
                store.xadd(&key, id, &fields, 0).unwrap();
            }
        }

        let xlen = store.xlen(&key, 0).unwrap();
        prop_assert_eq!(xlen, added_ids.len(), "XLEN must equal number of unique entries added");
    }

    // MR26: XRANGE returns entries in strictly ascending ID order
    #[test]
    fn mr_stream_xrange_ordering(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            2..15
        )
    ) {
        let mut store = fresh_store();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"f".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
        }

        let result = store.xrange(&key, (0, 0), (u64::MAX, u64::MAX), None, 0).unwrap();

        for window in result.windows(2) {
            let (id1, _) = &window[0];
            let (id2, _) = &window[1];
            prop_assert!(id1 < id2, "XRANGE results must be in strictly ascending order");
        }
    }

    // MR27: XREVRANGE is the exact reverse of XRANGE
    #[test]
    fn mr_stream_xrevrange_reverses_xrange(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            1..15
        )
    ) {
        let mut store = fresh_store();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"data".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
        }

        let xrange = store.xrange(&key, (0, 0), (u64::MAX, u64::MAX), None, 0).unwrap();
        let xrevrange = store.xrevrange(&key, (u64::MAX, u64::MAX), (0, 0), None, 0).unwrap();

        let xrange_reversed: Vec<_> = xrange.iter().rev().cloned().collect();
        prop_assert_eq!(xrevrange, xrange_reversed, "XREVRANGE must be exact reverse of XRANGE");
    }

    // MR28: XREAD returns entries strictly greater than start ID
    #[test]
    fn mr_stream_xread_exclusive(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            3..15
        ),
        split_index in 0usize..10
    ) {
        let mut store = fresh_store();
        let mut ids = Vec::new();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"v".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
            ids.push(id);
        }

        ids.sort();
        let split = split_index.min(ids.len().saturating_sub(1));
        let start_id = ids[split];

        let result = store.xread(&key, start_id, None, 0).unwrap();

        for (id, _) in &result {
            prop_assert!(*id > start_id, "XREAD must only return IDs strictly greater than start");
        }
    }

    // MR29: XTRIM MAXLEN leaves at most MAXLEN entries
    #[test]
    fn mr_stream_xtrim_maxlen(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            5..20
        ),
        max_len in 1usize..10
    ) {
        let mut store = fresh_store();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"x".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
        }

        let before_len = store.xlen(&key, 0).unwrap();
        let trimmed = store.xtrim(&key, max_len, None, 0).unwrap();
        let after_len = store.xlen(&key, 0).unwrap();

        prop_assert!(after_len <= max_len, "XTRIM MAXLEN must leave at most max_len entries");
        prop_assert_eq!(before_len, after_len + trimmed, "trimmed count must equal length difference");
    }

    // MR30: XTRIM MINID removes entries with ID < threshold
    #[test]
    fn mr_stream_xtrim_minid(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            5..15
        ),
        threshold_index in 1usize..10
    ) {
        let mut store = fresh_store();
        let mut ids = Vec::new();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"y".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
            ids.push(id);
        }

        ids.sort();
        let threshold_idx = threshold_index.min(ids.len().saturating_sub(1));
        let min_id = ids[threshold_idx];

        store.xtrim_minid(&key, min_id, None, 0).unwrap();

        let remaining = store.xrange(&key, (0, 0), (u64::MAX, u64::MAX), None, 0).unwrap();
        for (id, _) in &remaining {
            prop_assert!(*id >= min_id, "XTRIM MINID must remove all entries with ID < threshold");
        }
    }

    // MR31: XDEL reduces XLEN by exactly the count of deleted entries
    #[test]
    fn mr_stream_xdel_consistency(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            5..15
        ),
        delete_indices in prop::collection::vec(0usize..15, 1..5)
    ) {
        let mut store = fresh_store();
        let mut ids = Vec::new();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"z".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
            ids.push(id);
        }

        ids.sort();
        let ids_to_delete: Vec<_> = delete_indices
            .iter()
            .filter_map(|&i| ids.get(i % ids.len()).copied())
            .collect();

        let before_len = store.xlen(&key, 0).unwrap();
        let deleted = store.xdel(&key, &ids_to_delete, 0).unwrap();
        let after_len = store.xlen(&key, 0).unwrap();

        prop_assert_eq!(before_len, after_len + deleted, "XDEL must reduce XLEN by deleted count");
    }

    // MR32: XRANGE with COUNT limit returns at most COUNT entries
    #[test]
    fn mr_stream_xrange_count_limit(
        key in prop::collection::vec(any::<u8>(), 1..32),
        entries in prop::collection::vec(
            (1u64..1_000_000, 0u64..100),
            5..20
        ),
        count_limit in 1usize..10
    ) {
        let mut store = fresh_store();

        for (i, (ms, seq)) in entries.iter().enumerate() {
            let id = (*ms + i as u64, *seq);
            let fields = vec![(b"w".to_vec(), vec![i as u8])];
            store.xadd(&key, id, &fields, 0).unwrap();
        }

        let result = store.xrange(&key, (0, 0), (u64::MAX, u64::MAX), Some(count_limit), 0).unwrap();

        prop_assert!(result.len() <= count_limit, "XRANGE with COUNT must return at most COUNT entries");
    }

    // MR33: PFADD monotonicity - adding more elements never decreases PFCOUNT
    #[test]
    fn mr_hll_pfadd_monotonicity(
        key in prop::collection::vec(any::<u8>(), 1..32),
        elements in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..32),
            2..20
        )
    ) {
        let mut store = fresh_store();

        let mut prev_count = 0u64;
        for element in &elements {
            store.pfadd(&key, std::slice::from_ref(element), 0).unwrap();
            let count = store.pfcount(&[key.as_slice()], 0).unwrap();
            prop_assert!(count >= prev_count, "PFCOUNT must never decrease after PFADD");
            prev_count = count;
        }
    }

    // MR34: PFADD of same element is approximately idempotent
    #[test]
    fn mr_hll_pfadd_same_element_idempotent(
        key in prop::collection::vec(any::<u8>(), 1..32),
        element in prop::collection::vec(any::<u8>(), 1..32)
    ) {
        let mut store = fresh_store();

        store.pfadd(&key, std::slice::from_ref(&element), 0).unwrap();
        let count1 = store.pfcount(&[key.as_slice()], 0).unwrap();

        store.pfadd(&key, std::slice::from_ref(&element), 0).unwrap();
        let count2 = store.pfcount(&[key.as_slice()], 0).unwrap();

        store.pfadd(&key, std::slice::from_ref(&element), 0).unwrap();
        let count3 = store.pfcount(&[key.as_slice()], 0).unwrap();

        prop_assert_eq!(count1, count2, "PFADD same element twice must not increase count");
        prop_assert_eq!(count2, count3, "PFADD same element thrice must not increase count");
    }

    // MR35: PFCOUNT has an upper bound related to unique elements (with HLL tolerance)
    #[test]
    fn mr_hll_pfcount_upper_bound(
        key in prop::collection::vec(any::<u8>(), 1..32),
        elements in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..16),
            1..50
        )
    ) {
        let mut store = fresh_store();

        let unique: std::collections::HashSet<Vec<u8>> = elements.iter().cloned().collect();
        let unique_count = unique.len() as u64;

        for element in &elements {
            store.pfadd(&key, std::slice::from_ref(element), 0).unwrap();
        }

        let hll_count = store.pfcount(&[key.as_slice()], 0).unwrap();
        let tolerance = (unique_count as f64 * 0.05).max(5.0) as u64;
        prop_assert!(
            hll_count <= unique_count + tolerance,
            "PFCOUNT {} should be within tolerance of unique count {} (tolerance {})",
            hll_count, unique_count, tolerance
        );
    }

    // MR36: PFMERGE commutativity - order of sources doesn't affect result
    #[test]
    fn mr_hll_pfmerge_commutative(
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        src_a in prop::collection::vec(any::<u8>(), 1..16),
        src_b in prop::collection::vec(any::<u8>(), 1..16),
        elements_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..10),
        elements_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..10)
    ) {
        prop_assume!(dest1 != dest2 && src_a != src_b && dest1 != src_a && dest1 != src_b && dest2 != src_a && dest2 != src_b);

        let mut store1 = fresh_store();
        store1.pfadd(&src_a, &elements_a, 0).unwrap();
        store1.pfadd(&src_b, &elements_b, 0).unwrap();
        store1.pfmerge(&dest1, &[src_a.as_slice(), src_b.as_slice()], 0).unwrap();
        let count1 = store1.pfcount(&[dest1.as_slice()], 0).unwrap();

        let mut store2 = fresh_store();
        store2.pfadd(&src_a, &elements_a, 0).unwrap();
        store2.pfadd(&src_b, &elements_b, 0).unwrap();
        store2.pfmerge(&dest2, &[src_b.as_slice(), src_a.as_slice()], 0).unwrap();
        let count2 = store2.pfcount(&[dest2.as_slice()], 0).unwrap();

        prop_assert_eq!(count1, count2, "PFMERGE must be commutative");
    }

    // MR37: PFMERGE result count is at most sum of individual counts
    #[test]
    fn mr_hll_pfmerge_union_bound(
        dest in prop::collection::vec(any::<u8>(), 1..16),
        src_a in prop::collection::vec(any::<u8>(), 1..16),
        src_b in prop::collection::vec(any::<u8>(), 1..16),
        elements_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        elements_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(dest != src_a && dest != src_b && src_a != src_b);

        let mut store = fresh_store();
        store.pfadd(&src_a, &elements_a, 0).unwrap();
        store.pfadd(&src_b, &elements_b, 0).unwrap();

        let count_a = store.pfcount(&[src_a.as_slice()], 0).unwrap();
        let count_b = store.pfcount(&[src_b.as_slice()], 0).unwrap();

        store.pfmerge(&dest, &[src_a.as_slice(), src_b.as_slice()], 0).unwrap();
        let merged_count = store.pfcount(&[dest.as_slice()], 0).unwrap();

        prop_assert!(
            merged_count <= count_a + count_b,
            "PFMERGE count {} must be <= sum {} + {} = {}",
            merged_count, count_a, count_b, count_a + count_b
        );
    }

    // MR38: PFCOUNT of multiple keys equals PFCOUNT after PFMERGE
    #[test]
    fn mr_hll_pfcount_multi_equals_merge(
        dest in prop::collection::vec(any::<u8>(), 1..16),
        src_a in prop::collection::vec(any::<u8>(), 1..16),
        src_b in prop::collection::vec(any::<u8>(), 1..16),
        elements_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..10),
        elements_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..10)
    ) {
        prop_assume!(dest != src_a && dest != src_b && src_a != src_b);

        let mut store = fresh_store();
        store.pfadd(&src_a, &elements_a, 0).unwrap();
        store.pfadd(&src_b, &elements_b, 0).unwrap();

        let multi_count = store.pfcount(&[src_a.as_slice(), src_b.as_slice()], 0).unwrap();

        store.pfmerge(&dest, &[src_a.as_slice(), src_b.as_slice()], 0).unwrap();
        let merge_count = store.pfcount(&[dest.as_slice()], 0).unwrap();

        prop_assert_eq!(multi_count, merge_count, "PFCOUNT(A, B) must equal PFCOUNT(PFMERGE(A, B))");
    }

    // MR39: PFMERGE with single source preserves count
    #[test]
    fn mr_hll_pfmerge_single_source(
        dest in prop::collection::vec(any::<u8>(), 1..16),
        src in prop::collection::vec(any::<u8>(), 1..16),
        elements in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(dest != src);

        let mut store = fresh_store();
        store.pfadd(&src, &elements, 0).unwrap();
        let src_count = store.pfcount(&[src.as_slice()], 0).unwrap();

        store.pfmerge(&dest, &[src.as_slice()], 0).unwrap();
        let dest_count = store.pfcount(&[dest.as_slice()], 0).unwrap();

        prop_assert_eq!(src_count, dest_count, "PFMERGE with single source must preserve count");
    }

    // MR40: Empty HLL has count 0
    #[test]
    fn mr_hll_empty_count_zero(
        key in prop::collection::vec(any::<u8>(), 1..32)
    ) {
        let mut store = fresh_store();
        store.pfadd(&key, &[], 0).unwrap();
        let count = store.pfcount(&[key.as_slice()], 0).unwrap();
        prop_assert_eq!(count, 0, "Empty HLL must have count 0");
    }

    // MR41: SINTER commutativity - SINTER(A, B) = SINTER(B, A)
    #[test]
    fn mr_set_sinter_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let mut ab = store.sinter(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let mut ba = store.sinter(&[key_b.as_slice(), key_a.as_slice()], 0).unwrap();

        ab.sort();
        ba.sort();
        prop_assert_eq!(ab, ba, "SINTER must be commutative");
    }

    // MR42: SUNION commutativity - SUNION(A, B) = SUNION(B, A)
    #[test]
    fn mr_set_sunion_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let mut ab = store.sunion(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let mut ba = store.sunion(&[key_b.as_slice(), key_a.as_slice()], 0).unwrap();

        ab.sort();
        ba.sort();
        prop_assert_eq!(ab, ba, "SUNION must be commutative");
    }

    // MR43: SUNION cardinality bound - |A ∪ B| <= |A| + |B|
    #[test]
    fn mr_set_sunion_cardinality_bound(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let card_a = store.scard(&key_a, 0).unwrap();
        let card_b = store.scard(&key_b, 0).unwrap();
        let union = store.sunion(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();

        prop_assert!(union.len() <= card_a + card_b, "SUNION size must be <= sum of set sizes");
    }

    // MR44: SINTER cardinality bound - |A ∩ B| <= min(|A|, |B|)
    #[test]
    fn mr_set_sinter_cardinality_bound(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let card_a = store.scard(&key_a, 0).unwrap();
        let card_b = store.scard(&key_b, 0).unwrap();
        let inter = store.sinter(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();

        prop_assert!(inter.len() <= card_a.min(card_b), "SINTER size must be <= min of set sizes");
    }

    // MR45: SDIFF subset property - SDIFF(A, B) ⊆ A
    #[test]
    fn mr_set_sdiff_subset(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let diff = store.sdiff(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let a_members = store.smembers(&key_a, 0).unwrap();

        for member in &diff {
            prop_assert!(
                a_members.contains(member),
                "SDIFF(A, B) must be subset of A"
            );
        }
    }

    // MR46: SDIFF disjoint from intersection - (A - B) ∩ B = ∅
    #[test]
    fn mr_set_sdiff_disjoint_from_b(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let diff = store.sdiff(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let b_members = store.smembers(&key_b, 0).unwrap();

        for member in &diff {
            prop_assert!(
                !b_members.contains(member),
                "SDIFF(A, B) must be disjoint from B"
            );
        }
    }

    // MR47: SINTER subset of both - SINTER(A, B) ⊆ A and SINTER(A, B) ⊆ B
    #[test]
    fn mr_set_sinter_subset_of_both(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let inter = store.sinter(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let a_members = store.smembers(&key_a, 0).unwrap();
        let b_members = store.smembers(&key_b, 0).unwrap();

        for member in &inter {
            prop_assert!(a_members.contains(member), "SINTER result must be in A");
            prop_assert!(b_members.contains(member), "SINTER result must be in B");
        }
    }

    // MR48: Union contains both sets - A ⊆ SUNION(A, B) and B ⊆ SUNION(A, B)
    #[test]
    fn mr_set_sunion_contains_both(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15),
        members_b in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..15)
    ) {
        prop_assume!(key_a != key_b);

        let mut store = fresh_store();
        store.sadd(&key_a, &members_a, 0).unwrap();
        store.sadd(&key_b, &members_b, 0).unwrap();

        let union = store.sunion(&[key_a.as_slice(), key_b.as_slice()], 0).unwrap();
        let a_members = store.smembers(&key_a, 0).unwrap();
        let b_members = store.smembers(&key_b, 0).unwrap();

        for member in &a_members {
            prop_assert!(union.contains(member), "A must be subset of SUNION(A, B)");
        }
        for member in &b_members {
            prop_assert!(union.contains(member), "B must be subset of SUNION(A, B)");
        }
    }
}
