use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

#[test]
fn test_cluster_keyslot() {
    let mut store = Store::new();
    let out = dispatch_argv(
        &[b"CLUSTER".to_vec(), b"KEYSLOT".to_vec(), b"hello".to_vec()],
        &mut store,
        0,
    )
    .unwrap();
    
    // Hash of "hello" is 866
    assert_eq!(out, RespFrame::Integer(866));
}

#[test]
fn test_cluster_keyslot_hashtag() {
    let mut store = Store::new();
    let out = dispatch_argv(
        &[b"CLUSTER".to_vec(), b"KEYSLOT".to_vec(), b"{foo}bar".to_vec()],
        &mut store,
        0,
    )
    .unwrap();
    
    // Hash of "{foo}bar" is just the hash of "foo" = 12182
    assert_eq!(out, RespFrame::Integer(12182));
}

#[test]
fn test_cluster_getkeysinslot_and_countkeysinslot() {
    let mut store = Store::new();
    
    // The keys "foo" and "{foo}bar" will both hash to slot 12182
    dispatch_argv(&[b"SET".to_vec(), b"foo".to_vec(), b"val".to_vec()], &mut store, 0).unwrap();
    dispatch_argv(&[b"SET".to_vec(), b"{foo}bar".to_vec(), b"val".to_vec()], &mut store, 0).unwrap();
    
    // 1. COUNTKEYSINSLOT
    let out = dispatch_argv(
        &[b"CLUSTER".to_vec(), b"COUNTKEYSINSLOT".to_vec(), b"12182".to_vec()],
        &mut store,
        0,
    )
    .unwrap();
    assert_eq!(out, RespFrame::Integer(2));
    
    // 2. GETKEYSINSLOT count 1
    let out = dispatch_argv(
        &[b"CLUSTER".to_vec(), b"GETKEYSINSLOT".to_vec(), b"12182".to_vec(), b"1".to_vec()],
        &mut store,
        0,
    )
    .unwrap();
    
    match out {
        RespFrame::Array(Some(arr)) => {
            assert_eq!(arr.len(), 1);
        },
        _ => panic!("expected array"),
    }

    // 3. GETKEYSINSLOT count 10
    let out = dispatch_argv(
        &[b"CLUSTER".to_vec(), b"GETKEYSINSLOT".to_vec(), b"12182".to_vec(), b"10".to_vec()],
        &mut store,
        0,
    )
    .unwrap();
    
    match out {
        RespFrame::Array(Some(arr)) => {
            assert_eq!(arr.len(), 2);
        },
        _ => panic!("expected array"),
    }
}
