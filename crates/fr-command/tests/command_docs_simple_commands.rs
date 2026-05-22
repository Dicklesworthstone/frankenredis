use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-5ozae) Pin COMMAND DOCS for ~25 simple commands whose
// arg layouts are flat (key only, key + 1-2 scalars, key + key, or key
// multiple). Pre-fix, all of these emitted generic arg2/arg3 string
// placeholders; this batch mirrors per-command MAKE_ARG entries from
// legacy_redis_code/redis/src/commands.def. Also covers the DEL/EXISTS
// group reclassification from the catch-all "string" to "generic".

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

fn kv_field<'a>(out: &'a RespFrame, key: &str) -> Option<&'a RespFrame> {
    let RespFrame::Array(Some(entries)) = out else {
        panic!("expected outer Array");
    };
    let RespFrame::Array(Some(kv)) = &entries[1] else {
        panic!("expected inner kv array");
    };
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == key.as_bytes()
        {
            return Some(&kv[i + 1]);
        }
        i += 2;
    }
    None
}

fn arguments_array(out: &RespFrame) -> &Vec<RespFrame> {
    let RespFrame::Array(Some(arr)) = kv_field(out, "arguments").unwrap() else {
        panic!("arguments is not an array");
    };
    arr
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

fn assert_bulk(frame: &RespFrame, expected: &str) {
    match frame {
        RespFrame::BulkString(Some(b)) => {
            assert_eq!(b.as_slice(), expected.as_bytes(), "expected {expected}")
        }
        other => panic!("expected BulkString({expected}), got {other:?}"),
    }
}

fn assert_arg(arg: &RespFrame, name: &str, ty: &str, key_spec: Option<i64>) {
    assert_bulk(arg_field(arg, "name").unwrap(), name);
    assert_bulk(arg_field(arg, "type").unwrap(), ty);
    assert_bulk(arg_field(arg, "display_text").unwrap(), name);
    if let Some(idx) = key_spec {
        assert_eq!(
            arg_field(arg, "key_spec_index").unwrap(),
            &RespFrame::Integer(idx)
        );
    } else {
        assert!(arg_field(arg, "key_spec_index").is_none());
    }
}

#[test]
fn command_docs_single_key_only_commands() {
    for cmd in [
        "STRLEN",
        "GETDEL",
        "TYPE",
        "EXPIRETIME",
        "PEXPIRETIME",
        "PERSIST",
        "DUMP",
        "INCR",
        "DECR",
        "TTL",
        "PTTL",
    ] {
        let out = run(cmd);
        let args = arguments_array(&out);
        assert_eq!(args.len(), 1, "{cmd}: one arg");
        assert_arg(&args[0], "key", "key", Some(0));
    }
}

#[test]
fn command_docs_key_plus_one_scalar() {
    let cases = [
        ("GETBIT", "offset", "integer"),
        ("INCRBY", "increment", "integer"),
        ("DECRBY", "decrement", "integer"),
        ("INCRBYFLOAT", "increment", "double"),
        ("APPEND", "value", "string"),
    ];
    for (cmd, scalar_name, scalar_ty) in cases {
        let out = run(cmd);
        let args = arguments_array(&out);
        assert_eq!(args.len(), 2, "{cmd}: two args");
        assert_arg(&args[0], "key", "key", Some(0));
        assert_arg(&args[1], scalar_name, scalar_ty, None);
    }
}

#[test]
fn command_docs_key_plus_two_scalars() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("GETRANGE", &[("start", "integer"), ("end", "integer")]),
        ("SETRANGE", &[("offset", "integer"), ("value", "string")]),
        ("SETBIT", &[("offset", "integer"), ("value", "integer")]),
    ];
    for (cmd, scalars) in cases {
        let out = run(cmd);
        let args = arguments_array(&out);
        assert_eq!(args.len(), 1 + scalars.len(), "{cmd}: arg count");
        assert_arg(&args[0], "key", "key", Some(0));
        for (i, (n, t)) in scalars.iter().enumerate() {
            assert_arg(&args[i + 1], n, t, None);
        }
    }
}

#[test]
fn command_docs_rename_family() {
    for cmd in ["RENAME", "RENAMENX"] {
        let out = run(cmd);
        let args = arguments_array(&out);
        assert_eq!(args.len(), 2, "{cmd}: two args");
        assert_arg(&args[0], "key", "key", Some(0));
        assert_arg(&args[1], "newkey", "key", Some(1));
    }
}

#[test]
fn command_docs_key_multiple_commands() {
    for cmd in ["MGET", "DEL", "UNLINK", "EXISTS"] {
        let out = run(cmd);
        let args = arguments_array(&out);
        assert_eq!(args.len(), 1, "{cmd}: one arg");
        assert_arg(&args[0], "key", "key", Some(0));
        let RespFrame::Array(Some(flags)) = arg_field(&args[0], "flags").unwrap() else {
            panic!("{cmd}: flags must be array");
        };
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0], RespFrame::SimpleString("multiple".to_string()));
    }
}

#[test]
fn command_docs_del_exists_classified_as_generic() {
    for cmd in ["DEL", "EXISTS"] {
        let out = run(cmd);
        assert_bulk(kv_field(&out, "group").unwrap(), "generic");
    }
}
