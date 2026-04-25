use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: HMSET then HMGET recovers all values correctly
    #[test]
    fn mr_hmset_hmget_consistency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        fields_values in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            prop::collection::vec(any::<u8>(), 1..64),
            1..20
        ),
        missing_fields in prop::collection::hash_set(
            prop::collection::vec(any::<u8>(), 1..16),
            1..10
        )
    ) {
        let mut store = fresh_store();

        let mut fv_pairs = Vec::new();
        for (f, v) in &fields_values {
            fv_pairs.push((f.clone(), v.clone()));
        }

        // Use multiple HSETs as HMSET is not exposed as a single method on Store
        for (f, v) in &fv_pairs {
            store.hset(&key, f.clone(), v.clone(), 0).unwrap();
        }

        let mut query_fields: Vec<Vec<u8>> = fields_values.keys().cloned().collect();
        for mf in &missing_fields {
            if !fields_values.contains_key(mf) {
                query_fields.push(mf.clone());
            }
        }

        let query_refs: Vec<&[u8]> = query_fields.iter().map(|f| f.as_slice()).collect();

        let retrieved = store.hmget(&key, &query_refs, 0).unwrap();
        prop_assert_eq!(retrieved.len(), query_fields.len());

        for (i, val) in retrieved.into_iter().enumerate() {
            let field = &query_fields[i];
            let expected = fields_values.get(field).cloned();
            prop_assert_eq!(val, expected);
        }
    }

    // MR2: HLEN matches the number of unique fields set
    #[test]
    fn mr_hlen_tracks_unique_fields(
        key in prop::collection::vec(any::<u8>(), 1..16),
        fields_values in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            prop::collection::vec(any::<u8>(), 1..64),
            1..20
        )
    ) {
        let mut store = fresh_store();

        for (f, v) in &fields_values {
            store.hset(&key, f.clone(), v.clone(), 0).unwrap();
        }

        let hlen = store.hlen(&key, 0).unwrap();
        prop_assert_eq!(hlen, fields_values.len());
    }

    // MR3: HKEYS and HVALS lengths match HLEN and contain all expected elements
    #[test]
    fn mr_hkeys_hvals_consistency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        fields_values in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            prop::collection::vec(any::<u8>(), 1..64),
            1..20
        )
    ) {
        let mut store = fresh_store();

        for (f, v) in &fields_values {
            store.hset(&key, f.clone(), v.clone(), 0).unwrap();
        }

        let hkeys = store.hkeys(&key, 0).unwrap();
        let hvals = store.hvals(&key, 0).unwrap();
        let hlen = store.hlen(&key, 0).unwrap();

        prop_assert_eq!(hkeys.len(), hlen);
        prop_assert_eq!(hvals.len(), hlen);

        let mut expected_keys: Vec<_> = fields_values.keys().cloned().collect();
        let mut actual_keys = hkeys.clone();
        expected_keys.sort();
        actual_keys.sort();
        prop_assert_eq!(actual_keys, expected_keys);

        // Note: HVALS doesn't guarantee order if there are duplicate values,
        // but we can sort and compare since it's a multiset
        let mut expected_vals: Vec<_> = fields_values.values().cloned().collect();
        let mut actual_vals = hvals.clone();
        expected_vals.sort();
        actual_vals.sort();
        prop_assert_eq!(actual_vals, expected_vals);
    }
}
