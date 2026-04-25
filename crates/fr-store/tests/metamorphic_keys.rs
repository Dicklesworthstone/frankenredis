use fr_store::{PttlValue, Store};
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: EXISTS returns true after SET
    #[test]
    fn mr_exists_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();

        let exists_before = store.exists(&key, 0);
        prop_assert!(!exists_before);

        store.set(key.clone(), value.clone(), None, 0);

        let exists_after = store.exists(&key, 0);
        prop_assert!(exists_after);
    }

    // MR2: RENAME transfers value and drops old key
    #[test]
    fn mr_rename_transfer(
        key1 in prop::collection::vec(any::<u8>(), 1..16),
        key2 in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        prop_assume!(key1 != key2);
        let mut store = fresh_store();

        store.set(key1.clone(), value.clone(), None, 0);
        store.rename(&key1, &key2, 0).unwrap();

        let retrieved_old = store.get(&key1, 0).unwrap();
        prop_assert_eq!(retrieved_old, None);

        let retrieved_new = store.get(&key2, 0).unwrap();
        prop_assert_eq!(retrieved_new, Some(value));
    }

    // MR3: RENAMENX fails if target exists, leaving both intact
    #[test]
    fn mr_renamenx_conditional(
        key1 in prop::collection::vec(any::<u8>(), 1..16),
        key2 in prop::collection::vec(any::<u8>(), 1..16),
        val1 in prop::collection::vec(any::<u8>(), 1..16),
        val2 in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        prop_assume!(key1 != key2);
        let mut store = fresh_store();

        store.set(key1.clone(), val1.clone(), None, 0);
        store.set(key2.clone(), val2.clone(), None, 0);

        let renamed = store.renamenx(&key1, &key2, 0).unwrap();
        prop_assert!(!renamed);

        let retrieved1 = store.get(&key1, 0).unwrap();
        prop_assert_eq!(retrieved1, Some(val1));

        let retrieved2 = store.get(&key2, 0).unwrap();
        prop_assert_eq!(retrieved2, Some(val2));
    }

    // MR4: PERSIST removes expiration correctly
    #[test]
    fn mr_persist_removes_expiry(
        key in prop::collection::vec(any::<u8>(), 1..16),
        value in prop::collection::vec(any::<u8>(), 1..16),
        ttl in 1000u64..1000000u64
    ) {
        let mut store = fresh_store();

        store.set(key.clone(), value.clone(), Some(ttl), 0);

        let pttl_before = store.pttl(&key, 0);
        match pttl_before {
            PttlValue::Remaining(ms) => prop_assert_eq!(ms, ttl as i64),
            _ => prop_assert!(false, "Expected TTL to exist"),
        }

        let persisted = store.persist(&key, 0);
        prop_assert!(persisted);

        let pttl_after = store.pttl(&key, 0);
        prop_assert_eq!(pttl_after, PttlValue::NoExpiry);
    }
}
