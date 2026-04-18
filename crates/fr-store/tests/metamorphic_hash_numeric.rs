use fr_store::Store;
use proptest::prelude::*;

fn fresh_store() -> Store {
    Store::new()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // MR1: HINCRBY sequence matches direct addition
    #[test]
    fn mr_hincrby_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -100i64..100i64,
        steps in 1usize..20
    ) {
        let mut store = fresh_store();
        store.hset(&key, field.clone(), start_val.to_string().into_bytes(), 0).unwrap();
        
        let mut expected = start_val;
        for _ in 0..steps {
            expected += 1;
            let result = store.hincrby(&key, &field, 1, 0).unwrap();
            prop_assert_eq!(result, expected);
        }
        
        let final_val = store.hget(&key, &field, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        prop_assert_eq!(final_str.parse::<i64>().unwrap(), expected);
    }
    
    // MR2: HINCRBY sum matches expected
    #[test]
    fn mr_hincrby_variable_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -1000i64..1000i64,
        increments in prop::collection::vec(-100i64..100i64, 1..20)
    ) {
        let mut store = fresh_store();
        store.hset(&key, field.clone(), start_val.to_string().into_bytes(), 0).unwrap();
        
        let mut expected = start_val;
        for inc in increments {
            expected += inc;
            let result = store.hincrby(&key, &field, inc, 0).unwrap();
            prop_assert_eq!(result, expected);
        }
        
        let final_val = store.hget(&key, &field, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        prop_assert_eq!(final_str.parse::<i64>().unwrap(), expected);
    }
    
    // MR3: HINCRBYFLOAT sum matches expected
    #[test]
    fn mr_hincrbyfloat_additive(
        key in prop::collection::vec(any::<u8>(), 1..16),
        field in prop::collection::vec(any::<u8>(), 1..16),
        start_val in -100.0..100.0f64,
        increments in prop::collection::vec(-10.0..10.0f64, 1..20)
    ) {
        let mut store = fresh_store();
        store.hset(&key, field.clone(), format!("{:.5}", start_val).into_bytes(), 0).unwrap();
        
        let mut expected = format!("{:.5}", start_val).parse::<f64>().unwrap();
        for inc in increments {
            expected += inc;
            let result = store.hincrbyfloat(&key, &field, inc, 0).unwrap();
            prop_assert!((result - expected).abs() < 1e-9);
        }
        
        let final_val = store.hget(&key, &field, 0).unwrap().unwrap();
        let final_str = String::from_utf8(final_val).unwrap();
        let parsed_final = final_str.parse::<f64>().unwrap();
        prop_assert!((parsed_final - expected).abs() < 1e-9);
    }
}
