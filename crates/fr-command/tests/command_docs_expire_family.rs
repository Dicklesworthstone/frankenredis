use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-wb78v) Pin COMMAND DOCS for the four expire-family
// commands. Pre-fix, fr emitted generic `arg2 string optional` instead
// of the upstream-specific second argument and the condition oneof
// (NX/XX/GT/LT pure-tokens, since 7.0.0). Source:
// legacy_redis_code/redis/src/commands.def EXPIRE_Args / PEXPIRE_Args /
// EXPIREAT_Args / PEXPIREAT_Args.

fn run(name: &str) -> RespFrame {
    let mut store = Store::new();
    dispatch_argv(
        &[
            b"COMMAND".to_vec(),
            b"DOCS".to_vec(),
            name.as_bytes().to_vec(),
        ],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS should succeed")
}

fn arguments_array(out: &RespFrame) -> &Vec<RespFrame> {
    let RespFrame::Array(Some(entries)) = out else {
        panic!("expected outer Array");
    };
    let RespFrame::Array(Some(kv)) = &entries[1] else {
        panic!("expected inner kv array");
    };
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == b"arguments"
            && let RespFrame::Array(Some(arr)) = &kv[i + 1]
        {
            return arr;
        }
        i += 2;
    }
    panic!("kv stream missing 'arguments'");
}

fn arg_field<'a>(arg: &'a RespFrame, key: &str) -> Option<&'a RespFrame> {
    let RespFrame::Array(Some(items)) = arg else {
        return None;
    };
    let mut i = 0;
    while i + 1 < items.len() {
        if let RespFrame::BulkString(Some(k)) = &items[i]
            && k.as_slice() == key.as_bytes()
        {
            return Some(&items[i + 1]);
        }
        i += 2;
    }
    None
}

fn assert_bulk_eq(frame: &RespFrame, expected: &str) {
    match frame {
        RespFrame::BulkString(Some(b)) => {
            assert_eq!(b.as_slice(), expected.as_bytes(), "expected {expected}")
        }
        other => panic!("expected BulkString({expected}), got {other:?}"),
    }
}

fn assert_condition_arg(arg: &RespFrame) {
    assert_bulk_eq(arg_field(arg, "name").unwrap(), "condition");
    assert_bulk_eq(arg_field(arg, "type").unwrap(), "oneof");
    assert_bulk_eq(arg_field(arg, "since").unwrap(), "7.0.0");
    let flags = arg_field(arg, "flags").unwrap();
    let RespFrame::Array(Some(items)) = flags else {
        panic!("flags must be array");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], RespFrame::SimpleString("optional".to_string()));

    let subargs = arg_field(arg, "arguments").unwrap();
    let RespFrame::Array(Some(sub)) = subargs else {
        panic!("subargs must be array");
    };
    assert_eq!(sub.len(), 4);
    let expected = [("nx", "NX"), ("xx", "XX"), ("gt", "GT"), ("lt", "LT")];
    for (idx, (n, t)) in expected.iter().enumerate() {
        assert_bulk_eq(arg_field(&sub[idx], "name").unwrap(), n);
        assert_bulk_eq(arg_field(&sub[idx], "type").unwrap(), "pure-token");
        assert_bulk_eq(arg_field(&sub[idx], "token").unwrap(), t);
    }
}

#[test]
fn command_docs_expire_matches_upstream_layout() {
    let out = run("EXPIRE");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 3);
    assert_bulk_eq(arg_field(&args[0], "name").unwrap(), "key");
    assert_bulk_eq(arg_field(&args[0], "type").unwrap(), "key");
    assert_bulk_eq(arg_field(&args[1], "name").unwrap(), "seconds");
    assert_bulk_eq(arg_field(&args[1], "type").unwrap(), "integer");
    assert_condition_arg(&args[2]);
}

#[test]
fn command_docs_pexpire_matches_upstream_layout() {
    let out = run("PEXPIRE");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 3);
    assert_bulk_eq(arg_field(&args[1], "name").unwrap(), "milliseconds");
    assert_bulk_eq(arg_field(&args[1], "type").unwrap(), "integer");
    assert_condition_arg(&args[2]);
}

#[test]
fn command_docs_expireat_matches_upstream_layout() {
    let out = run("EXPIREAT");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 3);
    assert_bulk_eq(arg_field(&args[1], "name").unwrap(), "unix-time-seconds");
    assert_bulk_eq(arg_field(&args[1], "type").unwrap(), "unix-time");
    assert_condition_arg(&args[2]);
}

#[test]
fn command_docs_pexpireat_matches_upstream_layout() {
    let out = run("PEXPIREAT");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 3);
    assert_bulk_eq(
        arg_field(&args[1], "name").unwrap(),
        "unix-time-milliseconds",
    );
    assert_bulk_eq(arg_field(&args[1], "type").unwrap(), "unix-time");
    assert_condition_arg(&args[2]);
}
