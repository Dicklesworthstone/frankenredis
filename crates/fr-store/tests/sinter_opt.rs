use fr_store::Store;

#[test]
fn test_sinter_opt() {
    let mut store = Store::new();
    store
        .sadd(b"big", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
        .unwrap();
    store.sadd(b"small", &[b"b".to_vec()], 0).unwrap();
    let res = store.sinter(&[b"big", b"small"], 0).unwrap();
    assert_eq!(res, vec![b"b".to_vec()]);
}
