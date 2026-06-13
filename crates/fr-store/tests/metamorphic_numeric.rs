use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: GETSET returns old value and sets new value
    #[test]
    fn mr_getset_recovery(
        key in prop::collection::vec(any::<u8>(), 1..16),
        old_val in prop::collection::vec(any::<u8>(), 1..16),
        new_val in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), old_val.clone(), None, 0);

        let retrieved_old = store.getset(key.clone(), new_val.clone(), 0).unwrap();
        prop_assert_eq!(retrieved_old, Some(old_val));

        let retrieved_new = store.get(&key, 0).unwrap();
        prop_assert_eq!(retrieved_new, Some(new_val));
    }

    // MR2: DEL removes keys and returns count of removed keys
    #[test]
    fn mr_del_count_and_removal(
        keys in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        missing_keys in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..10)
    ) {
        let mut store = fresh_store();

        let mut all_keys_to_del = Vec::new();
        for k in &keys {
            store.set(k.clone(), vec![1, 2, 3], None, 0);
            all_keys_to_del.push(k.clone());
        }

        for mk in &missing_keys {
            if !keys.contains(mk) {
                all_keys_to_del.push(mk.clone());
            }
        }

        let del_count = store.del(&all_keys_to_del, 0);
        prop_assert_eq!(del_count, keys.len() as u64);

        for k in &all_keys_to_del {
            prop_assert!(!store.exists(k, 0));
        }
    }

    // MR3: INCR sequence matches direct addition
    #[test]
    fn mr_incr_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -100i64..100i64,
        steps in 1usize..20
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), start_val.to_string().into_bytes(), None, 0);

        let mut expected = start_val;
        for _ in 0..steps {
            expected += 1;
            let result = store.incr(&key, 0).unwrap();
            prop_assert_eq!(result, expected);
        }

        let final_val = store.get(&key, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        prop_assert_eq!(final_str.parse::<i64>().unwrap(), expected);
    }

    // MR4: INCRBY sum matches expected
    #[test]
    fn mr_incrby_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -1000i64..1000i64,
        increments in prop::collection::vec(-100i64..100i64, 1..20)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), start_val.to_string().into_bytes(), None, 0);

        let mut expected = start_val;
        for inc in increments {
            expected += inc;
            let result = store.incrby(&key, inc, 0).unwrap();
            prop_assert_eq!(result, expected);
        }

        let final_val = store.get(&key, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        prop_assert_eq!(final_str.parse::<i64>().unwrap(), expected);
    }

    // MR5: INCRBYFLOAT sum matches expected
    #[test]
    fn mr_incrbyfloat_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -100.0..100.0f64,
        increments in prop::collection::vec(-10.0..10.0f64, 1..20)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), format!("{:.5}", start_val).into_bytes(), None, 0);

        let mut expected = format!("{:.5}", start_val).parse::<f64>().unwrap();
        for inc in increments {
            expected += inc;
            let result = store.incrbyfloat(&key, inc, 0).unwrap();
            prop_assert!((result - expected).abs() < 1e-9);
        }

        let final_val = store.get(&key, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        let parsed_final = final_str.parse::<f64>().unwrap();
        prop_assert!((parsed_final - expected).abs() < 1e-9);
    }
}
