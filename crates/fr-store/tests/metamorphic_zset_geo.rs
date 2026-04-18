use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: ZUNIONSTORE Commutativity - ZUNIONSTORE(A, B) == ZUNIONSTORE(B, A)
    #[test]
    fn mr_zunionstore_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        pairs_a in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -100.0..100.0f64), 1..10),
        pairs_b in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -100.0..100.0f64), 1..10)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        for (member, score) in &pairs_a {
            store1.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store1.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }
        store1.zunionstore(&dest1, &[&key_a, &key_b], &[1.0, 1.0], b"SUM", 0).unwrap();

        let mut store2 = fresh_store();
        for (member, score) in &pairs_a {
            store2.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store2.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }
        store2.zunionstore(&dest2, &[&key_b, &key_a], &[1.0, 1.0], b"SUM", 0).unwrap();

        let res1 = store1.zrange_withscores(&dest1, 0, -1, 0).unwrap();
        let res2 = store2.zrange_withscores(&dest2, 0, -1, 0).unwrap();

        prop_assert_eq!(&res1.len(), &res2.len());
        for ((m1, s1), (m2, s2)) in res1.iter().zip(res2.iter()) {
            prop_assert_eq!(m1, m2);
            prop_assert!((s1 - s2).abs() < 1e-9, "scores must be equal");
        }
    }

    // MR2: ZINTERSTORE Commutativity - ZINTERSTORE(A, B) == ZINTERSTORE(B, A)
    #[test]
    fn mr_zinterstore_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        pairs_a in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -100.0..100.0f64), 1..10),
        pairs_b in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -100.0..100.0f64), 1..10)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        for (member, score) in &pairs_a {
            store1.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store1.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }
        store1.zinterstore(&dest1, &[&key_a, &key_b], &[1.0, 1.0], b"SUM", 0).unwrap();

        let mut store2 = fresh_store();
        for (member, score) in &pairs_a {
            store2.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store2.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }
        store2.zinterstore(&dest2, &[&key_b, &key_a], &[1.0, 1.0], b"SUM", 0).unwrap();

        let res1 = store1.zrange_withscores(&dest1, 0, -1, 0).unwrap();
        let res2 = store2.zrange_withscores(&dest2, 0, -1, 0).unwrap();

        prop_assert_eq!(&res1.len(), &res2.len());
        for ((m1, s1), (m2, s2)) in res1.iter().zip(res2.iter()) {
            prop_assert_eq!(m1, m2);
            prop_assert!((s1 - s2).abs() < 1e-9, "scores must be equal");
        }
    }

    // MR3: ZUNIONSTORE Multiplicative Scaling - Scaling weights by K scales resulting scores by K (SUM aggregate)
    #[test]
    fn mr_zunionstore_scaling(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        pairs_a in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10),
        pairs_b in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10),
        k in 0.5..10.0f64
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store = fresh_store();
        for (member, score) in &pairs_a {
            store.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }

        // Base case: weight 1.0
        store.zunionstore(&dest1, &[&key_a, &key_b], &[1.0, 1.0], b"SUM", 0).unwrap();
        // Scaled case: weight K
        store.zunionstore(&dest2, &[&key_a, &key_b], &[k, k], b"SUM", 0).unwrap();

        let res1 = store.zrange_withscores(&dest1, 0, -1, 0).unwrap();
        let res2 = store.zrange_withscores(&dest2, 0, -1, 0).unwrap();

        prop_assert_eq!(&res1.len(), &res2.len());
        for ((m1, s1), (m2, s2)) in res1.iter().zip(res2.iter()) {
            prop_assert_eq!(m1, m2);
            prop_assert!((s1 * k - s2).abs() < 1e-6, "scaled score must match k * base_score");
        }
    }

    // MR4: ZINTERSTORE Subset Property - Output elements must be in BOTH inputs
    #[test]
    fn mr_zinterstore_subset(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        pairs_a in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10),
        pairs_b in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10)
    ) {
        prop_assume!(key_a != key_b && dest != key_a && dest != key_b);

        let mut store = fresh_store();
        let mut members_a = std::collections::HashSet::new();
        let mut members_b = std::collections::HashSet::new();

        for (member, score) in &pairs_a {
            store.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
            members_a.insert(member.clone());
        }
        for (member, score) in &pairs_b {
            store.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
            members_b.insert(member.clone());
        }

        store.zinterstore(&dest, &[&key_a, &key_b], &[1.0, 1.0], b"SUM", 0).unwrap();
        let res = store.zrange_withscores(&dest, 0, -1, 0).unwrap();

        for (m, _) in &res {
            prop_assert!(members_a.contains(m), "ZINTERSTORE result must be in A");
            prop_assert!(members_b.contains(m), "ZINTERSTORE result must be in B");
        }
        
        let expected_card = members_a.intersection(&members_b).count();
        prop_assert_eq!(res.len(), expected_card, "ZINTERSTORE output size must match intersection size");
    }

    // MR5: ZUNIONSTORE Superset Property - Output size must be <= |A| + |B| and >= max(|A|, |B|)
    #[test]
    fn mr_zunionstore_size_bounds(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        pairs_a in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10),
        pairs_b in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -50.0..50.0f64), 1..10)
    ) {
        prop_assume!(key_a != key_b && dest != key_a && dest != key_b);

        let mut store = fresh_store();
        for (member, score) in &pairs_a {
            store.zadd(&key_a, &[(*score, member.clone())], 0).unwrap();
        }
        for (member, score) in &pairs_b {
            store.zadd(&key_b, &[(*score, member.clone())], 0).unwrap();
        }

        let card_a = store.zcard(&key_a, 0).unwrap();
        let card_b = store.zcard(&key_b, 0).unwrap();

        store.zunionstore(&dest, &[&key_a, &key_b], &[1.0, 1.0], b"SUM", 0).unwrap();
        let card_u = store.zcard(&dest, 0).unwrap();

        prop_assert!(card_u <= card_a + card_b, "ZUNIONSTORE size must be <= sum of inputs");
        prop_assert!(card_u >= card_a.max(card_b), "ZUNIONSTORE size must be >= max of inputs");
    }
}
