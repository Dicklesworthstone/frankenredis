use fr_store::Store;

#[test]
fn sdiff_missing_first_wrong_second() {
    let mut store = Store::new();
    store.set(b"str".to_vec(), b"val".to_vec(), None, 0);
    // If keys[0] is missing, does it return empty or wrong type for keys[1]?
    let res = store.sdiff(&[b"missing", b"str"], 0);
    println!("{:?}", res);
}
