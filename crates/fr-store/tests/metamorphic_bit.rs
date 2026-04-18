use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: SETBIT then GETBIT recovers the set value (Identity)
    #[test]
    fn mr_setbit_getbit_identity(
        key in prop::collection::vec(any::<u8>(), 1..16),
        offset in 0usize..65536, // Keep offset reasonable for tests
        value in any::<bool>()
    ) {
        let mut store = fresh_store();
        store.setbit(&key, offset, value, 0).unwrap();
        
        let retrieved = store.getbit(&key, offset, 0).unwrap();
        prop_assert_eq!(retrieved, value);
    }
    
    // MR2: SETBIT idempotency
    #[test]
    fn mr_setbit_idempotency(
        key in prop::collection::vec(any::<u8>(), 1..16),
        offset in 0usize..65536,
        value in any::<bool>()
    ) {
        let mut store = fresh_store();
        let _old1 = store.setbit(&key, offset, value, 0).unwrap();
        let old2 = store.setbit(&key, offset, value, 0).unwrap();
        
        // The second SETBIT should return the value we just set
        prop_assert_eq!(old2, value);
        
        let retrieved = store.getbit(&key, offset, 0).unwrap();
        prop_assert_eq!(retrieved, value);
    }
    
    // MR3: BITCOUNT monotonicity after setting a 0 to 1
    #[test]
    fn mr_bitcount_monotonicity_0_to_1(
        key in prop::collection::vec(any::<u8>(), 1..16),
        offset in 0usize..65536
    ) {
        let mut store = fresh_store();
        // Ensure bit is 0
        store.setbit(&key, offset, false, 0).unwrap();
        let count_before = store.bitcount(&key, None, None, 0).unwrap();
        
        // Set to 1
        store.setbit(&key, offset, true, 0).unwrap();
        let count_after = store.bitcount(&key, None, None, 0).unwrap();
        
        prop_assert_eq!(count_after, count_before + 1);
    }

    // MR4: BITCOUNT monotonicity after setting a 1 to 0
    #[test]
    fn mr_bitcount_monotonicity_1_to_0(
        key in prop::collection::vec(any::<u8>(), 1..16),
        offset in 0usize..65536
    ) {
        let mut store = fresh_store();
        // Ensure bit is 1
        store.setbit(&key, offset, true, 0).unwrap();
        let count_before = store.bitcount(&key, None, None, 0).unwrap();
        
        // Set to 0
        store.setbit(&key, offset, false, 0).unwrap();
        let count_after = store.bitcount(&key, None, None, 0).unwrap();
        
        prop_assert_eq!(count_after, count_before - 1);
    }
    
    // MR5: BITCOUNT boundary constraints
    #[test]
    fn mr_bitcount_bounds(
        key in prop::collection::vec(any::<u8>(), 1..16),
        initial_value in prop::collection::vec(any::<u8>(), 1..1024),
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), initial_value.clone(), None, 0);
        
        let count = store.bitcount(&key, None, None, 0).unwrap();
        let strlen = store.strlen(&key, 0).unwrap();
        
        // A byte has 8 bits, so total set bits must be <= strlen * 8
        prop_assert!(count <= strlen * 8);
    }
    
    // MR6: BITPOS exists if BITCOUNT > 0
    #[test]
    fn mr_bitpos_exists_if_bitcount_positive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        initial_value in prop::collection::vec(any::<u8>(), 1..1024),
    ) {
        let mut store = fresh_store();
        store.set(key.clone(), initial_value.clone(), None, 0);
        
        let count = store.bitcount(&key, None, None, 0).unwrap();
        let pos1 = store.bitpos(&key, true, None, None, 0).unwrap();
        
        if count > 0 {
            prop_assert!(pos1 >= 0);
        } else {
            prop_assert_eq!(pos1, -1);
        }
    }
}
