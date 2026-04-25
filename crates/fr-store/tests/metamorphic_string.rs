use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: GETRANGE of full string matches the string itself
    #[test]
    fn mr_getrange_full_string(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let retrieved = store.getrange(&key, 0, -1, 0).unwrap();
        prop_assert_eq!(&retrieved, &value);
    }

    // MR2: SETRANGE at 0 with identical string is idempotent
    #[test]
    fn mr_setrange_idempotent(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let len = store.setrange(&key, 0, &value, 0).unwrap();
        prop_assert_eq!(len, value.len());

        let retrieved = store.get(&key, 0).unwrap();
        prop_assert_eq!(retrieved, Some(value));
    }

    // MR3: SETRANGE extends length correctly
    #[test]
    fn mr_setrange_extends_length(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..128),
        append in prop::collection::vec(any::<u8>(), 1..128),
        offset in 0usize..512
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let new_len = store.setrange(&key, offset, &append, 0).unwrap();

        let expected_len = std::cmp::max(value.len(), offset + append.len());
        prop_assert_eq!(new_len, expected_len);

        let strlen = store.strlen(&key, 0).unwrap();
        prop_assert_eq!(strlen, expected_len);
    }

    // MR4: GETRANGE bounds semantics
    #[test]
    fn mr_getrange_bounds(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256),
        start in -500i64..500i64,
        end in -500i64..500i64
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let retrieved = store.getrange(&key, start, end, 0).unwrap();

        let len = value.len() as i64;
        let mut s = if start < 0 { len + start } else { start };
        let e = if end < 0 { len + end } else { end };

        if s < 0 { s = 0; }

        let expected = if s > e || len == 0 || s >= len {
            Vec::new()
        } else {
            let e_idx = std::cmp::min(e, len - 1) as usize;
            value[s as usize..e_idx + 1].to_vec()
        };

        prop_assert_eq!(retrieved, expected);
    }
}
