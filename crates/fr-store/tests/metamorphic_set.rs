use fr_store::Store;
use proptest::prelude::*;
use std::collections::HashSet;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: SADD then SISMEMBER recovers the set member (Identity)
    #[test]
    fn mr_sadd_sismember_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        store.sadd(&key, std::slice::from_ref(&member), 0).unwrap();

        let retrieved = store.sismember(&key, &member, 0).unwrap();
        prop_assert!(retrieved);
    }

    // MR2: SREM idempotency and completeness
    #[test]
    fn mr_srem_idempotency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        let mut store = fresh_store();
        store.sadd(&key, std::slice::from_ref(&member), 0).unwrap();

        let deleted1 = store.srem(&key, &[&member], 0).unwrap();
        let deleted2 = store.srem(&key, &[&member], 0).unwrap();

        prop_assert_eq!(deleted1, 1);
        prop_assert_eq!(deleted2, 0);

        let retrieved = store.sismember(&key, &member, 0).unwrap();
        prop_assert!(!retrieved);
    }

    // MR3: SUNIONSTORE Commutativity
    #[test]
    fn mr_sunionstore_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        let a_vec: Vec<Vec<u8>> = members_a.iter().cloned().collect();
        let b_vec: Vec<Vec<u8>> = members_b.iter().cloned().collect();

        store1.sadd(&key_a, &a_vec, 0).unwrap();
        store1.sadd(&key_b, &b_vec, 0).unwrap();
        store1.sunionstore(&dest1, &[&key_a, &key_b], 0).unwrap();

        let mut store2 = fresh_store();
        store2.sadd(&key_a, &a_vec, 0).unwrap();
        store2.sadd(&key_b, &b_vec, 0).unwrap();
        store2.sunionstore(&dest2, &[&key_b, &key_a], 0).unwrap();

        let res1: HashSet<_> = store1.smembers(&dest1, 0).unwrap().into_iter().collect();
        let res2: HashSet<_> = store2.smembers(&dest2, 0).unwrap().into_iter().collect();

        prop_assert_eq!(res1, res2);
    }

    // MR4: SINTERSTORE Commutativity
    #[test]
    fn mr_sinterstore_commutative(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest1 in prop::collection::vec(any::<u8>(), 1..16),
        dest2 in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b && dest1 != dest2 && dest1 != key_a && dest1 != key_b && dest2 != key_a && dest2 != key_b);

        let mut store1 = fresh_store();
        let a_vec: Vec<Vec<u8>> = members_a.iter().cloned().collect();
        let b_vec: Vec<Vec<u8>> = members_b.iter().cloned().collect();

        store1.sadd(&key_a, &a_vec, 0).unwrap();
        store1.sadd(&key_b, &b_vec, 0).unwrap();
        store1.sinterstore(&dest1, &[&key_a, &key_b], 0).unwrap();

        let mut store2 = fresh_store();
        store2.sadd(&key_a, &a_vec, 0).unwrap();
        store2.sadd(&key_b, &b_vec, 0).unwrap();
        store2.sinterstore(&dest2, &[&key_b, &key_a], 0).unwrap();

        let res1: HashSet<_> = store1.smembers(&dest1, 0).unwrap().into_iter().collect();
        let res2: HashSet<_> = store2.smembers(&dest2, 0).unwrap().into_iter().collect();

        prop_assert_eq!(res1, res2);
    }

    // MR5: SINTERSTORE bounds
    #[test]
    fn mr_sinterstore_bounds(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b && dest != key_a && dest != key_b);

        let mut store = fresh_store();
        let a_vec: Vec<Vec<u8>> = members_a.iter().cloned().collect();
        let b_vec: Vec<Vec<u8>> = members_b.iter().cloned().collect();

        store.sadd(&key_a, &a_vec, 0).unwrap();
        store.sadd(&key_b, &b_vec, 0).unwrap();

        let card_a = store.scard(&key_a, 0).unwrap();
        let card_b = store.scard(&key_b, 0).unwrap();

        store.sinterstore(&dest, &[&key_a, &key_b], 0).unwrap();
        let card_i = store.scard(&dest, 0).unwrap();

        prop_assert!(card_i <= card_a.min(card_b));
    }

    // MR6: SUNIONSTORE bounds
    #[test]
    fn mr_sunionstore_bounds(
        key_a in prop::collection::vec(any::<u8>(), 1..16),
        key_b in prop::collection::vec(any::<u8>(), 1..16),
        dest in prop::collection::vec(any::<u8>(), 1..16),
        members_a in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20),
        members_b in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(key_a != key_b && dest != key_a && dest != key_b);

        let mut store = fresh_store();
        let a_vec: Vec<Vec<u8>> = members_a.iter().cloned().collect();
        let b_vec: Vec<Vec<u8>> = members_b.iter().cloned().collect();

        store.sadd(&key_a, &a_vec, 0).unwrap();
        store.sadd(&key_b, &b_vec, 0).unwrap();

        let card_a = store.scard(&key_a, 0).unwrap();
        let card_b = store.scard(&key_b, 0).unwrap();

        store.sunionstore(&dest, &[&key_a, &key_b], 0).unwrap();
        let card_u = store.scard(&dest, 0).unwrap();

        prop_assert!(card_u <= card_a + card_b);
        prop_assert!(card_u >= card_a.max(card_b));
    }
}
