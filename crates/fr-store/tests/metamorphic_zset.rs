use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: ZADD then ZSCORE recovers the score (Identity)
    #[test]
    fn mr_zadd_zscore_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16),
        score in -100.0..100.0f64
    ) {
        let mut store = fresh_store();
        store.zadd(&key, &[(score, member.clone())], 0).unwrap();

        let retrieved = store.zscore(&key, &member, 0).unwrap();
        prop_assert_eq!(retrieved, Some(score));
    }

    // MR2: ZREM idempotency and completeness
    #[test]
    fn mr_zrem_idempotency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16),
        score in -100.0..100.0f64
    ) {
        let mut store = fresh_store();
        store.zadd(&key, &[(score, member.clone())], 0).unwrap();

        let deleted1 = store.zrem(&key, &[&member], 0).unwrap();
        let deleted2 = store.zrem(&key, &[&member], 0).unwrap();

        prop_assert_eq!(deleted1, 1);
        prop_assert_eq!(deleted2, 0);

        let retrieved = store.zscore(&key, &member, 0).unwrap();
        prop_assert_eq!(retrieved, None);
    }

    // MR3: ZINCRBY is additive
    #[test]
    fn mr_zincrby_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16),
        initial_score in -50.0..50.0f64,
        increments in prop::collection::vec(-10.0..10.0f64, 1..10)
    ) {
        let mut store = fresh_store();
        store.zadd(&key, &[(initial_score, member.clone())], 0).unwrap();

        let mut expected = initial_score;
        for inc in increments {
            expected += inc;
            let res = store.zincrby(&key, member.clone(), inc, 0).unwrap();
            prop_assert!((res - expected).abs() < 1e-9);
        }

        let final_score = store.zscore(&key, &member, 0).unwrap().unwrap();
        prop_assert!((final_score - expected).abs() < 1e-9);
    }

    // MR4: ZADD updates existing score
    #[test]
    fn mr_zadd_updates_score(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16),
        score1 in -100.0..100.0f64,
        score2 in -100.0..100.0f64
    ) {
        let mut store = fresh_store();
        let added1 = store.zadd(&key, &[(score1, member.clone())], 0).unwrap();
        let added2 = store.zadd(&key, &[(score2, member.clone())], 0).unwrap();

        prop_assert_eq!(added1, 1);
        prop_assert_eq!(added2, 0); // Member already exists, just updated

        let retrieved = store.zscore(&key, &member, 0).unwrap();
        prop_assert_eq!(retrieved, Some(score2));
    }

    // MR5: ZCARD consistency
    #[test]
    fn mr_zcard_consistency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::vec((prop::collection::vec(any::<u8>(), 1..16), -100.0..100.0f64), 1..20)
    ) {
        let mut store = fresh_store();

        let mut unique_members = std::collections::HashSet::new();
        for (member, score) in &pairs {
            store.zadd(&key, &[(*score, member.clone())], 0).unwrap();
            unique_members.insert(member.clone());

            let card = store.zcard(&key, 0).unwrap();
            prop_assert_eq!(card, unique_members.len());
        }
    }
}
