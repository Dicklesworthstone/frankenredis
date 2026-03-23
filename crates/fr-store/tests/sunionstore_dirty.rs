use fr_store::Store;

#[test]
fn sunionstore_dirty() {
    let mut store = Store::new();
    let old_dirty = store.dirty;
    let _ = store.sunionstore(b"dest", &[b"missing"], 0).unwrap();
    assert_eq!(
        store.dirty, old_dirty,
        "Should not increment dirty if nothing changed"
    );
}
