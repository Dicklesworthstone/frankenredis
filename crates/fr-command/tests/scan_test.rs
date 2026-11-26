use fr_store::Store;

#[test]
fn test_scan_count_semantics() {
    let mut store = Store::new();
    for i in 0..1000 {
        store.set(format!("key:{}", i).into_bytes(), b"v".to_vec(), None, 0);
    }

    // Scan with count 10 and a pattern that matches nothing.
    // Matching Redis's incremental SCAN work budget, it inspects ~10 elements,
    // returns 0 matches, and returns a non-zero continuation cursor.
    let (cursor, elements) = store.scan(0, Some(b"nomatch*"), 10, 0);
    assert!(elements.is_empty());
    assert_ne!(
        cursor, 0,
        "SCAN COUNT 10 over 1000 non-matching keys must return an incremental cursor"
    );

    // Driving the cursor to 0 terminates cleanly with zero matches.
    let mut next = cursor;
    let mut steps = 1usize;
    while next != 0 {
        let (c, batch) = store.scan(next, Some(b"nomatch*"), 10, 0);
        assert!(batch.is_empty());
        next = c;
        steps += 1;
        assert!(steps < 2_000, "SCAN did not terminate");
    }
    assert!(steps > 10, "expected incremental SCAN steps, got {steps}");
}
