use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: HSET then HGET recovers the same value (Identity)
    #[test]
    fn mr_hset_hget_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        store.hset(&key, field.clone(), value.clone(), 0).unwrap();

        let retrieved = store.hget(&key, &field, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value));
    }

    // MR2: HDEL is idempotent and removes the field
    #[test]
    fn mr_hdel_idempotency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        store.hset(&key, field.clone(), value.clone(), 0).unwrap();

        let deleted1 = store.hdel(&key, &[&field], 0).unwrap();
        let deleted2 = store.hdel(&key, &[&field], 0).unwrap();

        prop_assert_eq!(deleted1, 1);
        prop_assert_eq!(deleted2, 0);

        let retrieved = store.hget(&key, &field, 0).unwrap();
        prop_assert_eq!(retrieved, None);
    }

    // MR3: Multiple HSETs -> HGETALL matches exactly
    #[test]
    fn mr_hset_hgetall_completeness(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            prop::collection::vec(any::<u8>(), 1..16),
            1..20
        )
    ) {
        let mut store = fresh_store();
        for (f, v) in &pairs {
            store.hset(&key, f.clone(), v.clone(), 0).unwrap();
        }

        let all = store.hgetall(&key, 0).unwrap();
        let mut retrieved_map = std::collections::HashMap::new();
        for (f, v) in all {
            retrieved_map.insert(f, v);
        }

        prop_assert_eq!(retrieved_map, pairs);    }

    // MR4: HSETNX behaves conditionally
    #[test]
    fn mr_hsetnx_conditional(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        value1 in prop::collection::vec(any::<u8>(), 1..16),
        value2 in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        prop_assume!(value1 != value2);

        let mut store = fresh_store();

        let added1 = store.hsetnx(&key, field.clone(), value1.clone(), 0).unwrap();
        prop_assert!(added1);

        let added2 = store.hsetnx(&key, field.clone(), value2.clone(), 0).unwrap();
        prop_assert!(!added2);

        let retrieved = store.hget(&key, &field, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value1));
    }
}
