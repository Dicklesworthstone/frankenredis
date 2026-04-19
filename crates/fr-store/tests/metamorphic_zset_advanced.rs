use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: ZPOPMIN removes elements in strictly ascending score order
    #[test]
    fn mr_zpopmin_ascending_order(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            -100.0..100.0f64,
            1..20
        )
    ) {
        let mut store = fresh_store();
        
        let mut zadd_args = Vec::new();
        for (member, score) in &pairs {
            zadd_args.push((*score, member.clone()));
        }
        
        store.zadd(&key, &zadd_args, 0).unwrap();
        
        let mut prev_score = f64::NEG_INFINITY;
        
        for _ in 0..pairs.len() {
            let popped = store.zpopmin(&key, 0).unwrap().unwrap();
            let (member, score) = popped;
            
            // Validate ascending order
            prop_assert!(score >= prev_score);
            prev_score = score;
            
            // Validate member is removed
            prop_assert_eq!(store.zscore(&key, &member, 0).unwrap(), None);
        }
        
        // Ensure set is empty
        prop_assert_eq!(store.zcard(&key, 0).unwrap(), 0);
    }
    
    // MR2: ZPOPMAX removes elements in strictly descending score order
    #[test]
    fn mr_zpopmax_descending_order(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            -100.0..100.0f64,
            1..20
        )
    ) {
        let mut store = fresh_store();
        
        let mut zadd_args = Vec::new();
        for (member, score) in &pairs {
            zadd_args.push((*score, member.clone()));
        }
        
        store.zadd(&key, &zadd_args, 0).unwrap();
        
        let mut prev_score = f64::INFINITY;
        
        for _ in 0..pairs.len() {
            let popped = store.zpopmax(&key, 0).unwrap().unwrap();
            let (member, score) = popped;
            
            // Validate descending order
            prop_assert!(score <= prev_score);
            prev_score = score;
            
            // Validate member is removed
            prop_assert_eq!(store.zscore(&key, &member, 0).unwrap(), None);
        }
        
        // Ensure set is empty
        prop_assert_eq!(store.zcard(&key, 0).unwrap(), 0);
    }
    
    // MR3: ZRANK returns index matching the element's position in ZRANGE
    #[test]
    fn mr_zrank_matches_zrange_index(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            -100.0..100.0f64,
            1..20
        )
    ) {
        let mut store = fresh_store();
        
        let mut zadd_args = Vec::new();
        for (member, score) in &pairs {
            zadd_args.push((*score, member.clone()));
        }
        
        store.zadd(&key, &zadd_args, 0).unwrap();
        
        let range = store.zrange(&key, 0, -1, 0).unwrap();
        
        for (expected_idx, member) in range.into_iter().enumerate() {
            let rank = store.zrank(&key, &member, 0).unwrap().unwrap();
            prop_assert_eq!(rank, expected_idx);
        }
    }
    
    // MR4: ZREVRANK returns index matching the element's position in ZREVRANGE
    #[test]
    fn mr_zrevrank_matches_zrevrange_index(
        key in prop::collection::vec(any::<u8>(), 1..16),
        pairs in prop::collection::hash_map(
            prop::collection::vec(any::<u8>(), 1..16),
            -100.0..100.0f64,
            1..20
        )
    ) {
        let mut store = fresh_store();
        
        let mut zadd_args = Vec::new();
        for (member, score) in &pairs {
            zadd_args.push((*score, member.clone()));
        }
        
        store.zadd(&key, &zadd_args, 0).unwrap();
        
        let revrange = store.zrevrange(&key, 0, -1, 0).unwrap();
        
        for (expected_idx, member) in revrange.into_iter().enumerate() {
            let revrank = store.zrevrank(&key, &member, 0).unwrap().unwrap();
            prop_assert_eq!(revrank, expected_idx);
        }
    }
}
