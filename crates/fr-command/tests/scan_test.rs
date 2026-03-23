use fr_store::Store;

#[test]
fn test_scan_count_semantics() {
    let mut store = Store::new();
    for i in 0..1000 {
        store.set(format!("key:{}", i).into_bytes(), b"v".to_vec(), None, 0);
    }

    // Scan with count 10 and a pattern that matches nothing.
    // If it behaves like Redis, it should look at ~10 elements, return 0 elements, and a new cursor > 0.
    // If it acts like current code, it will scan all 1000 elements, return 0 elements, and cursor 0.
    let (cursor, elements) = store.scan(0, Some(b"nomatch*"), 10, 0);
    println!("cursor: {}, elements: {}", cursor, elements.len());
}
