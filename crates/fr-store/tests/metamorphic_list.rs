use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: RPUSH then LRANGE gives elements in pushed order
    #[test]
    fn mr_rpush_lrange_order(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.rpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let retrieved = store.lrange(&key, 0, -1, 0).unwrap();
        prop_assert_eq!(&retrieved, &values);
    }

    // MR2: LPUSH then LRANGE gives elements in reverse pushed order
    #[test]
    fn mr_lpush_lrange_reverse_order(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.lpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let retrieved = store.lrange(&key, 0, -1, 0).unwrap();
        let mut expected = values.clone();
        expected.reverse();
        prop_assert_eq!(&retrieved, &expected);
    }

    // MR3: RPUSH then LPOP gives FIFO order
    #[test]
    fn mr_rpush_lpop_fifo(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.rpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let mut popped = Vec::new();
        for _ in 0..values.len() {
            if let Some(v) = store.lpop(&key, 0).unwrap() {
                popped.push(v);
            }
        }

        prop_assert_eq!(&popped, &values);
    }

    // MR4: RPUSH then RPOP gives LIFO order
    #[test]
    fn mr_rpush_rpop_lifo(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.rpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let mut popped = Vec::new();
        for _ in 0..values.len() {
            if let Some(v) = store.rpop(&key, 0).unwrap() {
                popped.push(v);
            }
        }

        let mut expected = values.clone();
        expected.reverse();
        prop_assert_eq!(&popped, &expected);
    }

    // MR5: LTRIM(0, -1) is idempotent and doesn't change the list
    #[test]
    fn mr_ltrim_idempotent(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        store.rpush(&key, &values, 0).unwrap();

        let before = store.lrange(&key, 0, -1, 0).unwrap();
        store.ltrim(&key, 0, -1, 0).unwrap();
        let after = store.lrange(&key, 0, -1, 0).unwrap();

        prop_assert_eq!(&before, &after);
    }
}
