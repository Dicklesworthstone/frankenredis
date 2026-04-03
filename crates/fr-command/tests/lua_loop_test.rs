use fr_command::eval_script;
use fr_store::Store;

#[test]
fn test_lua_empty_loop() {
    let mut store = Store::new();
    let script = b"while true do end";
    let res = eval_script(script, &[], &[], &mut store, 0);
    assert!(matches!(res, Err(_) | Ok(fr_protocol::RespFrame::Error(_))));
}
