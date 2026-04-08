#[test]
fn test_unpack_oom() {
    let mut store = fr_store::Store::new();
    let script = b"return unpack({1, 2, 3}, 1, 9223372036854775807)";
    let _result = fr_command::lua_eval::eval_script(script, &[], &[], &mut store, 0);
}