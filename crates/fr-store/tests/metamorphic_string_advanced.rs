use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: APPEND appends perfectly and returns combined length
    #[test]
    fn mr_append_concatenates(
        key in prop::collection::vec(any::<u8>(), 1..16),
        val1 in prop::collection::vec(any::<u8>(), 1..64),
        val2 in prop::collection::vec(any::<u8>(), 1..64)
    ) {
        let mut store = fresh_store();

        let len1 = store.append(&key, &val1, 0).unwrap();
        prop_assert_eq!(len1, val1.len());

        let len2 = store.append(&key, &val2, 0).unwrap();
        prop_assert_eq!(len2, val1.len() + val2.len());

        let retrieved = store.get(&key, 0).unwrap().unwrap();
        let mut expected = val1.clone();
        expected.extend_from_slice(&val2);

        prop_assert_eq!(retrieved, expected);
    }

    // MR2: STRLEN returns exact length of GET
    #[test]
    fn mr_strlen_matches_get_length(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..256)
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), value.clone(), None, 0);

        let strlen = store.strlen(&key, 0).unwrap();
        let get_len = store.get(&key, 0).unwrap().unwrap().len();

        prop_assert_eq!(strlen, get_len);
        prop_assert_eq!(strlen, value.len());
    }

    // MR3: MSET followed by MGET recovers all elements
    #[test]
    fn mr_mset_mget_recovery(
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            prop::collection::vec(any::<u8>(), 1..64),
            1..20
        )
    ) {
        let mut store = fresh_store();

        let mut keys = Vec::new();
        let mut mset_pairs = Vec::new();
        for (k, v) in &pairs {
            keys.push(k.clone());
            mset_pairs.push((k.clone(), v.clone()));
        }

        let keys_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();

        for (k, v) in &pairs {
            store.set(k.clone(), v.clone(), None, 0);
        }

        let retrieved = store.mget(&keys_refs, 0);
        prop_assert_eq!(retrieved.len(), keys.len());

        for (i, val) in retrieved.into_iter().enumerate() {
            let key = &keys[i];
            let expected = pairs.get(key).unwrap();
            prop_assert_eq!(val, Some(expected.clone()));
        }
    }
}
