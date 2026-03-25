use fr_store::Store;
use crate::dispatch_argv;

#[test]
fn zadd_xx_missing_key() {
    let mut store = Store::new();
    let _out = dispatch_argv(
        &[b"ZADD".to_vec(), b"myzset".to_vec(), b"XX".to_vec(), b"1.0".to_vec(), b"a".to_vec()],
        &mut store,
        0,
    ).unwrap();
    assert!(!store.exists(b"myzset", 0));
}
