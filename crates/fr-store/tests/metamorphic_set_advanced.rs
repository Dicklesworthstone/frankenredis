use fr_store::Store;
use proptest::prelude::*;
use std::collections::HashSet;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: SMOVE moves element from source to destination
    #[test]
    fn mr_smove_transfer_element(
        src in prop::collection::vec(any::<u8>(), 1..16),
        dst in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16),
        other_members in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        prop_assume!(src != dst);

        let mut store = fresh_store();
        store.sadd(&src, std::slice::from_ref(&member), 0).unwrap();

        let other_vec: Vec<Vec<u8>> = other_members.into_iter().filter(|m| m != &member).collect();
        if !other_vec.is_empty() {
            store.sadd(&src, &other_vec, 0).unwrap();
        }

        let card_src_before = store.scard(&src, 0).unwrap();
        let card_dst_before = store.scard(&dst, 0).unwrap();

        let moved = store.smove(&src, &dst, &member, 0).unwrap();
        prop_assert!(moved);

        let card_src_after = store.scard(&src, 0).unwrap();
        let card_dst_after = store.scard(&dst, 0).unwrap();

        prop_assert_eq!(card_src_after, card_src_before - 1);
        prop_assert_eq!(card_dst_after, card_dst_before + 1);

        let retrieved_src = store.sismember(&src, &member, 0).unwrap();
        let retrieved_dst = store.sismember(&dst, &member, 0).unwrap();

        prop_assert!(!retrieved_src);
        prop_assert!(retrieved_dst);
    }

    // MR2: SMOVE on missing element does nothing
    #[test]
    fn mr_smove_missing_element_idempotency(
        src in prop::collection::vec(any::<u8>(), 1..16),
        dst in prop::collection::vec(any::<u8>(), 1..16),
        member in prop::collection::vec(any::<u8>(), 1..16)
    ) {
        prop_assume!(src != dst);

        let mut store = fresh_store();
        let card_src_before = store.scard(&src, 0).unwrap();
        let card_dst_before = store.scard(&dst, 0).unwrap();

        let moved = store.smove(&src, &dst, &member, 0).unwrap();
        prop_assert!(!moved);

        let card_src_after = store.scard(&src, 0).unwrap();
        let card_dst_after = store.scard(&dst, 0).unwrap();

        prop_assert_eq!(card_src_after, card_src_before);
        prop_assert_eq!(card_dst_after, card_dst_before);
    }

    // MR3: SPOP removes exactly one element
    #[test]
    fn mr_spop_removes_element(
        key in prop::collection::vec(any::<u8>(), 1..16),
        members in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        let members_vec: Vec<Vec<u8>> = members.into_iter().collect();
        store.sadd(&key, &members_vec, 0).unwrap();

        let card_before = store.scard(&key, 0).unwrap();

        if let Some(popped) = store.spop(&key, 0).unwrap() {
            let card_after = store.scard(&key, 0).unwrap();
            prop_assert_eq!(card_after, card_before - 1);

            let still_member = store.sismember(&key, &popped, 0).unwrap();
            prop_assert!(!still_member);
        } else {
            prop_assert_eq!(card_before, 0);
        }
    }

    // MR4: SPOP count removes expected amount
    #[test]
    fn mr_spop_count_removes_elements(
        key in prop::collection::vec(any::<u8>(), 1..16),
        members in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 5..20),
        count in 1usize..10
    ) {
        let mut store = fresh_store();
        let members_vec: Vec<Vec<u8>> = members.clone().into_iter().collect();
        store.sadd(&key, &members_vec, 0).unwrap();

        let card_before = store.scard(&key, 0).unwrap();

        let popped = store.spop_count(&key, count, 0).unwrap();
        let expected_popped_count = std::cmp::min(card_before, count);

        prop_assert_eq!(popped.len(), expected_popped_count);

        let card_after = store.scard(&key, 0).unwrap();
        prop_assert_eq!(card_after, card_before - expected_popped_count);

        for p in popped {
            let still_member = store.sismember(&key, &p, 0).unwrap();
            prop_assert!(!still_member);
        }
    }

    // MR5: SRANDMEMBER does not modify the set
    #[test]
    fn mr_srandmember_idempotency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        members in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 1..20)
    ) {
        let mut store = fresh_store();
        let members_vec: Vec<Vec<u8>> = members.into_iter().collect();
        store.sadd(&key, &members_vec, 0).unwrap();

        let card_before = store.scard(&key, 0).unwrap();

        if let Some(rand_member) = store.srandmember(&key, 0).unwrap() {
            let card_after = store.scard(&key, 0).unwrap();
            prop_assert_eq!(card_after, card_before);

            let is_member = store.sismember(&key, &rand_member, 0).unwrap();
            prop_assert!(is_member);
        } else {
            prop_assert_eq!(card_before, 0);
        }
    }

    // MR6: SRANDMEMBER count positive returns subset
    #[test]
    fn mr_srandmember_count_positive_subset(
        key in prop::collection::vec(any::<u8>(), 1..16),
        members in prop::collection::hash_set(prop::collection::vec(any::<u8>(), 1..16), 5..20),
        count in 1i64..10
    ) {
        let mut store = fresh_store();
        let members_vec: Vec<Vec<u8>> = members.clone().into_iter().collect();
        store.sadd(&key, &members_vec, 0).unwrap();

        let card_before = store.scard(&key, 0).unwrap();

        let rand_members = store.srandmember_count(&key, count, 0).unwrap();
        let expected_count = std::cmp::min(card_before, count as usize);

        prop_assert_eq!(rand_members.len(), expected_count);

        // Should contain unique elements since count > 0
        let unique_returned: HashSet<_> = rand_members.iter().collect();
        prop_assert_eq!(unique_returned.len(), rand_members.len());

        for rm in rand_members {
            let is_member = store.sismember(&key, &rm, 0).unwrap();
            prop_assert!(is_member);
        }

        let card_after = store.scard(&key, 0).unwrap();
        prop_assert_eq!(card_after, card_before);
    }
}
