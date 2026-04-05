use fr_store::Store;

#[test]
fn test_zadd_empty_leak() {
    let mut store = Store::new();
    // Use NX. Since it's new, it wouldn't normally filter out, but what if members is empty?
    let res = store.zadd(b"myzset", &[], 0);
    assert_eq!(res, Ok(0));
    assert!(!store.exists(b"myzset", 0));
}
