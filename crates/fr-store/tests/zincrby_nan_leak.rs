use fr_store::{Store, StoreError};

#[test]
fn test_zincrby_nan_leak() {
    let mut store = Store::new();
    let res = store.zincrby(b"myzset", b"mymember".to_vec(), f64::NAN, 0);
    assert_eq!(res, Err(StoreError::IncrFloatNaN));
    // The key should not exist!
    assert!(!store.exists(b"myzset", 0));
}
