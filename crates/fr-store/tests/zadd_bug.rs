use fr_store::{Store, ZaddOptions};

#[test]
fn zadd_xx_missing_key_creates_empty_set() {
    let mut store = Store::new();
    let members = vec![(1.0, b"a".to_vec())];

    // Call zadd with XX on a non-existent key
    let opts = ZaddOptions {
        xx: true,
        ..ZaddOptions::default()
    };

    let _ = store
        .zadd_with_options(b"myzset", &members, opts, 0)
        .unwrap();

    // Redis should NOT have created the key
    assert!(
        !store.exists(b"myzset", 0),
        "Key should not exist if XX was used on missing key"
    );
}
