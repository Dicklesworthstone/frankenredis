use fr_command::dispatch_argv;
use fr_store::Store;

#[test]
fn test_float_format() {
    let mut store = Store::new();
    let out = dispatch_argv(
        &[
            b"INCRBYFLOAT".to_vec(),
            b"k".to_vec(),
            b"2.000000000000000001".to_vec(),
        ],
        &mut store,
        0,
    )
    .unwrap();
    println!("INCRBYFLOAT RESULT: {:?}", out);
}
