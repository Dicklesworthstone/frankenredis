//! Metamorphic property tests for core Redis command semantics.
//!
//! These tests verify invariant relationships between command sequences
//! rather than specific outputs, catching bugs that unit tests miss.

use fr_protocol::RespFrame;
use fr_runtime::Runtime;
use proptest::prelude::*;

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
            .collect(),
    ))
}

fn fresh_runtime() -> Runtime {
    Runtime::default_strict()
}

// ============================================================================
// MR1: INCR/DECR Additivity
// INCR(INCR(key)) == INCRBY(key, 2)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_incr_additivity(initial in -1000i64..1000i64, n in 1i64..100i64) {
        let mut rt1 = fresh_runtime();
        let mut rt2 = fresh_runtime();

        let initial_str = initial.to_string();
        let double_n_str = (n * 2).to_string();

        // rt1: SET key initial, then INCR n times, INCR n times
        rt1.execute_frame(command(&[b"SET", b"key", initial_str.as_bytes()]), 0);
        for _ in 0..n {
            rt1.execute_frame(command(&[b"INCR", b"key"]), 0);
        }
        for _ in 0..n {
            rt1.execute_frame(command(&[b"INCR", b"key"]), 0);
        }
        let result1 = rt1.execute_frame(command(&[b"GET", b"key"]), 0);

        // rt2: SET key initial, then INCRBY 2n
        rt2.execute_frame(command(&[b"SET", b"key", initial_str.as_bytes()]), 0);
        rt2.execute_frame(command(&[b"INCRBY", b"key", double_n_str.as_bytes()]), 0);
        let result2 = rt2.execute_frame(command(&[b"GET", b"key"]), 0);

        prop_assert_eq!(result1, result2, "INCR additivity violated");
    }

    #[test]
    fn mr_incr_decr_inverse(initial in -1000i64..1000i64, delta in 1i64..100i64) {
        let mut rt = fresh_runtime();

        let initial_str = initial.to_string();
        let delta_str = delta.to_string();

        rt.execute_frame(command(&[b"SET", b"key", initial_str.as_bytes()]), 0);
        rt.execute_frame(command(&[b"INCRBY", b"key", delta_str.as_bytes()]), 0);
        rt.execute_frame(command(&[b"DECRBY", b"key", delta_str.as_bytes()]), 0);
        let result = rt.execute_frame(command(&[b"GET", b"key"]), 0);

        let expected = RespFrame::BulkString(Some(initial_str.into_bytes()));
        prop_assert_eq!(result, expected, "INCR/DECR should be inverse operations");
    }
}

// ============================================================================
// MR2: SET/GET Round-Trip (Invertive)
// GET(key) after SET(key, value) == value
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_set_get_roundtrip(value in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut rt = fresh_runtime();

        rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(b"key".to_vec())),
                RespFrame::BulkString(Some(value.clone())),
            ])),
            0,
        );

        let result = rt.execute_frame(command(&[b"GET", b"key"]), 0);
        let expected = RespFrame::BulkString(Some(value));
        prop_assert_eq!(result, expected, "SET/GET round-trip violated");
    }

    #[test]
    fn mr_mset_mget_roundtrip(
        values in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 1..32), prop::collection::vec(any::<u8>(), 0..64)),
            1..10
        )
    ) {
        let mut rt = fresh_runtime();

        // Build MSET command
        let mut mset_args: Vec<RespFrame> = vec![RespFrame::BulkString(Some(b"MSET".to_vec()))];
        for (key, val) in &values {
            mset_args.push(RespFrame::BulkString(Some(key.clone())));
            mset_args.push(RespFrame::BulkString(Some(val.clone())));
        }
        rt.execute_frame(RespFrame::Array(Some(mset_args)), 0);

        // Verify each key
        for (key, expected_val) in &values {
            let get_args = vec![
                RespFrame::BulkString(Some(b"GET".to_vec())),
                RespFrame::BulkString(Some(key.clone())),
            ];
            let result = rt.execute_frame(RespFrame::Array(Some(get_args)), 0);
            let expected = RespFrame::BulkString(Some(expected_val.clone()));
            prop_assert_eq!(result, expected, "MSET/GET round-trip violated for key");
        }
    }
}

// ============================================================================
// MR3: SADD Idempotence (Equivalence)
// SADD(key, x); SADD(key, x) has same final set as SADD(key, x)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_sadd_idempotent(members in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..32), 1..10)) {
        let mut rt1 = fresh_runtime();
        let mut rt2 = fresh_runtime();

        // rt1: Add each member once
        for member in &members {
            let cmd = vec![
                RespFrame::BulkString(Some(b"SADD".to_vec())),
                RespFrame::BulkString(Some(b"myset".to_vec())),
                RespFrame::BulkString(Some(member.clone())),
            ];
            rt1.execute_frame(RespFrame::Array(Some(cmd)), 0);
        }

        // rt2: Add each member twice
        for member in &members {
            let cmd = vec![
                RespFrame::BulkString(Some(b"SADD".to_vec())),
                RespFrame::BulkString(Some(b"myset".to_vec())),
                RespFrame::BulkString(Some(member.clone())),
            ];
            rt2.execute_frame(RespFrame::Array(Some(cmd.clone())), 0);
            rt2.execute_frame(RespFrame::Array(Some(cmd)), 0);
        }

        // Both should have same cardinality
        let card1 = rt1.execute_frame(command(&[b"SCARD", b"myset"]), 0);
        let card2 = rt2.execute_frame(command(&[b"SCARD", b"myset"]), 0);
        prop_assert_eq!(card1, card2, "SADD idempotence violated");

        // Same members should exist
        for member in &members {
            let cmd = vec![
                RespFrame::BulkString(Some(b"SISMEMBER".to_vec())),
                RespFrame::BulkString(Some(b"myset".to_vec())),
                RespFrame::BulkString(Some(member.clone())),
            ];
            let is1 = rt1.execute_frame(RespFrame::Array(Some(cmd.clone())), 0);
            let is2 = rt2.execute_frame(RespFrame::Array(Some(cmd)), 0);
            prop_assert_eq!(is1, RespFrame::Integer(1));
            prop_assert_eq!(is2, RespFrame::Integer(1));
        }
    }
}

// ============================================================================
// MR4: LPUSH/LPOP Inverse
// LPUSH then LPOP returns the pushed value
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_lpush_lpop_inverse(value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut rt = fresh_runtime();

        let push_cmd = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"LPUSH".to_vec())),
            RespFrame::BulkString(Some(b"mylist".to_vec())),
            RespFrame::BulkString(Some(value.clone())),
        ]));
        rt.execute_frame(push_cmd, 0);

        let result = rt.execute_frame(command(&[b"LPOP", b"mylist"]), 0);
        let expected = RespFrame::BulkString(Some(value));
        prop_assert_eq!(result, expected, "LPUSH/LPOP inverse violated");
    }

    #[test]
    fn mr_rpush_rpop_inverse(value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut rt = fresh_runtime();

        let push_cmd = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RPUSH".to_vec())),
            RespFrame::BulkString(Some(b"mylist".to_vec())),
            RespFrame::BulkString(Some(value.clone())),
        ]));
        rt.execute_frame(push_cmd, 0);

        let result = rt.execute_frame(command(&[b"RPOP", b"mylist"]), 0);
        let expected = RespFrame::BulkString(Some(value));
        prop_assert_eq!(result, expected, "RPUSH/RPOP inverse violated");
    }

    #[test]
    fn mr_lpush_rpop_queue_order(values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..32), 1..10)) {
        let mut rt = fresh_runtime();

        // LPUSH all values (stack behavior: last pushed is at head)
        for val in &values {
            let cmd = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"LPUSH".to_vec())),
                RespFrame::BulkString(Some(b"mylist".to_vec())),
                RespFrame::BulkString(Some(val.clone())),
            ]));
            rt.execute_frame(cmd, 0);
        }

        // RPOP returns in FIFO order (first LPUSH'd value comes out first via RPOP)
        for val in &values {
            let result = rt.execute_frame(command(&[b"RPOP", b"mylist"]), 0);
            let expected = RespFrame::BulkString(Some(val.clone()));
            prop_assert_eq!(result, expected, "LPUSH/RPOP queue order violated");
        }
    }
}

// ============================================================================
// MR5: ZADD Score Ordering (Monotonic)
// Elements with lower scores appear before elements with higher scores
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_zadd_score_ordering(scores in prop::collection::vec(-1000.0f64..1000.0f64, 2..10)) {
        let mut rt = fresh_runtime();

        // Add elements with given scores
        for (i, score) in scores.iter().enumerate() {
            let member = format!("m{}", i);
            let score_str = format!("{}", score);
            let cmd = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ZADD".to_vec())),
                RespFrame::BulkString(Some(b"myzset".to_vec())),
                RespFrame::BulkString(Some(score_str.into_bytes())),
                RespFrame::BulkString(Some(member.into_bytes())),
            ]));
            rt.execute_frame(cmd, 0);
        }

        // ZRANGE returns elements in score order
        let result = rt.execute_frame(command(&[b"ZRANGE", b"myzset", b"0", b"-1", b"WITHSCORES"]), 0);
        if let RespFrame::Array(Some(items)) = result {
            let mut prev_score: Option<f64> = None;
            for chunk in items.chunks(2) {
                if let [_, RespFrame::BulkString(Some(score_bytes))] = chunk {
                    let score_str = String::from_utf8_lossy(score_bytes);
                    if let Ok(score) = score_str.parse::<f64>() {
                        if let Some(prev) = prev_score {
                            prop_assert!(score >= prev, "ZADD score ordering violated: {} < {}", score, prev);
                        }
                        prev_score = Some(score);
                    }
                }
            }
        }
    }
}

// ============================================================================
// MR6: DEL Completeness
// After DEL(key), EXISTS(key) == 0 and GET(key) == nil
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_del_completeness(value in prop::collection::vec(any::<u8>(), 1..64)) {
        let mut rt = fresh_runtime();

        // SET then DEL
        let set_cmd = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"key".to_vec())),
            RespFrame::BulkString(Some(value)),
        ]));
        rt.execute_frame(set_cmd, 0);
        rt.execute_frame(command(&[b"DEL", b"key"]), 0);

        // EXISTS should be 0
        let exists = rt.execute_frame(command(&[b"EXISTS", b"key"]), 0);
        prop_assert_eq!(exists, RespFrame::Integer(0), "DEL did not remove key from EXISTS");

        // GET should be nil
        let get = rt.execute_frame(command(&[b"GET", b"key"]), 0);
        prop_assert_eq!(get, RespFrame::BulkString(None), "DEL did not remove key value");
    }

    #[test]
    fn mr_flushdb_completeness(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut rt = fresh_runtime();

        // Add several keys
        for key in &keys {
            let cmd = RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SET".to_vec())),
                RespFrame::BulkString(Some(key.clone())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ]));
            rt.execute_frame(cmd, 0);
        }

        // FLUSHDB
        rt.execute_frame(command(&[b"FLUSHDB"]), 0);

        // DBSIZE should be 0
        let dbsize = rt.execute_frame(command(&[b"DBSIZE"]), 0);
        prop_assert_eq!(dbsize, RespFrame::Integer(0), "FLUSHDB did not clear all keys");
    }
}

// ============================================================================
// MR7: EXPIRE/TTL Consistency
// After EXPIRE(key, ttl), TTL(key) <= ttl
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn mr_expire_ttl_consistency(ttl in 10i64..1000i64) {
        let mut rt = fresh_runtime();

        rt.execute_frame(command(&[b"SET", b"key", b"value"]), 0);

        let ttl_str = ttl.to_string();
        rt.execute_frame(command(&[b"EXPIRE", b"key", ttl_str.as_bytes()]), 0);

        let result = rt.execute_frame(command(&[b"TTL", b"key"]), 0);
        if let RespFrame::Integer(remaining) = result {
            prop_assert!(remaining <= ttl, "TTL {} exceeds set EXPIRE {}", remaining, ttl);
            prop_assert!(remaining > 0, "TTL should be positive immediately after EXPIRE");
        } else {
            prop_assert!(false, "TTL should return integer");
        }
    }
}

// ============================================================================
// MR8: APPEND Length Additivity
// STRLEN after APPEND(key, s) increases by len(s)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_append_length_additive(
        initial in prop::collection::vec(any::<u8>(), 0..64),
        suffix in prop::collection::vec(any::<u8>(), 1..64)
    ) {
        let mut rt = fresh_runtime();

        // SET initial value
        let set_cmd = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"key".to_vec())),
            RespFrame::BulkString(Some(initial.clone())),
        ]));
        rt.execute_frame(set_cmd, 0);

        let len_before = rt.execute_frame(command(&[b"STRLEN", b"key"]), 0);

        // APPEND suffix
        let append_cmd = RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"APPEND".to_vec())),
            RespFrame::BulkString(Some(b"key".to_vec())),
            RespFrame::BulkString(Some(suffix.clone())),
        ]));
        rt.execute_frame(append_cmd, 0);

        let len_after = rt.execute_frame(command(&[b"STRLEN", b"key"]), 0);

        if let (RespFrame::Integer(before), RespFrame::Integer(after)) = (len_before, len_after) {
            prop_assert_eq!(
                after,
                before + suffix.len() as i64,
                "APPEND length additivity violated"
            );
        }
    }
}

// ============================================================================
// MR9: HINCRBY Commutativity
// HINCRBY(key, field, a); HINCRBY(key, field, b) == HINCRBY(key, field, b); HINCRBY(key, field, a)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn mr_hincrby_commutative(a in -100i64..100i64, b in -100i64..100i64) {
        let mut rt1 = fresh_runtime();
        let mut rt2 = fresh_runtime();

        let a_str = a.to_string();
        let b_str = b.to_string();

        // rt1: HINCRBY a then b
        rt1.execute_frame(command(&[b"HINCRBY", b"h", b"f", a_str.as_bytes()]), 0);
        rt1.execute_frame(command(&[b"HINCRBY", b"h", b"f", b_str.as_bytes()]), 0);

        // rt2: HINCRBY b then a
        rt2.execute_frame(command(&[b"HINCRBY", b"h", b"f", b_str.as_bytes()]), 0);
        rt2.execute_frame(command(&[b"HINCRBY", b"h", b"f", a_str.as_bytes()]), 0);

        let result1 = rt1.execute_frame(command(&[b"HGET", b"h", b"f"]), 0);
        let result2 = rt2.execute_frame(command(&[b"HGET", b"h", b"f"]), 0);

        prop_assert_eq!(result1, result2, "HINCRBY commutativity violated");
    }
}
