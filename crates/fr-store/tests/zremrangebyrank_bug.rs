use fr_store::Store;

#[test]
fn test_zremrangebyrank_negative_out_of_bounds() {
    let mut store = Store::new();
    let key = b"myzset";

    store
        .zadd(
            key,
            &[
                (1.0, b"a".to_vec()),
                (2.0, b"b".to_vec()),
                (3.0, b"c".to_vec()),
            ],
            0,
        )
        .unwrap();
    assert_eq!(store.zcard(key, 0).unwrap(), 3);

    // -10 to -5 should remove nothing.
    let removed = store.zremrangebyrank(key, -10, -5, 0).unwrap();
    assert_eq!(removed, 0); // Fails here if it returns 3!
    assert_eq!(store.zcard(key, 0).unwrap(), 3);
}
