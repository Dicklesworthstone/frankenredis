use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: LSET then LINDEX recovers the value
    #[test]
    fn mr_lset_lindex_recovery(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        new_value in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.rpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let len = values.len() as i64;
        // Test with a positive index
        let pos_idx = len / 2;
        store.lset(&key, pos_idx, new_value.clone(), 0).unwrap();
        let retrieved = store.lindex(&key, pos_idx, 0).unwrap();
        prop_assert_eq!(retrieved, Some(new_value.clone()));

        // Test with a negative index
        let neg_idx = -1;
        store.lset(&key, neg_idx, new_value.clone(), 0).unwrap();
        let retrieved2 = store.lindex(&key, neg_idx, 0).unwrap();
        prop_assert_eq!(retrieved2, Some(new_value));
    }

    // MR2: LINDEX boundary behavior
    #[test]
    fn mr_lindex_bounds(
        key in prop::collection::vec(any::<u8>(), 1..16),
        values in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        idx in -100i64..100i64
    ) {
        let mut store = fresh_store();
        for v in &values {
            store.rpush(&key, std::slice::from_ref(v), 0).unwrap();
        }

        let len = values.len() as i64;
        let retrieved = store.lindex(&key, idx, 0).unwrap();

        let mut actual_idx = idx;
        if actual_idx < 0 {
            actual_idx += len;
        }

        if actual_idx < 0 || actual_idx >= len {
            prop_assert_eq!(retrieved, None);
        } else {
            prop_assert_eq!(retrieved, Some(values[actual_idx as usize].clone()));
        }
    }

    // MR3: LINSERT BEFORE / AFTER bounds checks and ordering
    #[test]
    fn mr_linsert_ordering(
        key in prop::collection::vec(any::<u8>(), 1..16),
        val1 in prop::collection::vec(any::<u8>(), 1..16),
        val2 in prop::collection::vec(any::<u8>(), 1..16),
        val3 in prop::collection::vec(any::<u8>(), 1..16),
        val4 in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        prop_assume!(val1 != val2 && val1 != val3 && val1 != val4);
        let mut store = fresh_store();
        
        // Setup: list has [val1, val2]
        store.rpush(&key, &[val1.clone(), val2.clone()], 0).unwrap();
        
        // Insert before: [val3, val1, val2]
        let len1 = store.linsert_before(&key, &val1, val3.clone(), 0).unwrap();
        prop_assert_eq!(len1, 3);
        
        // Insert after: [val3, val1, val4, val2]
        let len2 = store.linsert_after(&key, &val1, val4.clone(), 0).unwrap();
        prop_assert_eq!(len2, 4);

        let retrieved = store.lrange(&key, 0, -1, 0).unwrap();
        prop_assert_eq!(retrieved, vec![val3, val1, val4, val2]);
    }
}
