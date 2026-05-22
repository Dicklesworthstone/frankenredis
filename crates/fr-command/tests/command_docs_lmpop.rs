use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-xifsa) Pin COMMAND DOCS LMPOP byte-equivalence with
// vendored Redis 7.2.4 — the upstream layout is:
//   numkeys (integer), key (key, key_spec_index=0, multiple),
//   where (oneof of LEFT/RIGHT pure-tokens), count (integer, COUNT, optional).
// Pre-fix, fr emitted generic arg1/arg2/arg3 string placeholders.
#[test]
fn command_docs_lmpop_matches_upstream_layout() {
    let mut store = Store::new();
    let out = dispatch_argv(
        &[b"COMMAND".to_vec(), b"DOCS".to_vec(), b"LMPOP".to_vec()],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS LMPOP should succeed");

    let entries = match &out {
        RespFrame::Array(Some(items)) => items,
        other => panic!("expected outer Array, got {other:?}"),
    };
    assert_eq!(entries.len(), 2, "outer array is [name, kv-array]");

    let kv = match &entries[1] {
        RespFrame::Array(Some(items)) => items,
        other => panic!("expected inner kv array, got {other:?}"),
    };

    // Find the "arguments" key in the kv stream.
    let mut args: Option<&Vec<RespFrame>> = None;
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == b"arguments"
            && let RespFrame::Array(Some(arr)) = &kv[i + 1]
        {
            args = Some(arr);
            break;
        }
        i += 2;
    }
    let args = args.expect("kv stream must contain arguments array");
    assert_eq!(args.len(), 4, "lmpop has 4 top-level args");

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

    // numkeys: integer
    assert_bulk_eq(arg_field(&args[0], "name").unwrap(), "numkeys");
    assert_bulk_eq(arg_field(&args[0], "type").unwrap(), "integer");
    assert_bulk_eq(arg_field(&args[0], "display_text").unwrap(), "numkeys");
    assert!(arg_field(&args[0], "key_spec_index").is_none());
    assert!(arg_field(&args[0], "flags").is_none());

    // key: key, key_spec_index=0, multiple flag
    assert_bulk_eq(arg_field(&args[1], "name").unwrap(), "key");
    assert_bulk_eq(arg_field(&args[1], "type").unwrap(), "key");
    assert_bulk_eq(arg_field(&args[1], "display_text").unwrap(), "key");
    assert_eq!(
        arg_field(&args[1], "key_spec_index").unwrap(),
        &RespFrame::Integer(0)
    );
    let flags = arg_field(&args[1], "flags").expect("key arg has flags");
    let RespFrame::Array(Some(flag_items)) = flags else {
        panic!("flags must be array, got {flags:?}");
    };
    assert_eq!(flag_items.len(), 1);
    assert_eq!(
        flag_items[0],
        RespFrame::SimpleString("multiple".to_string())
    );

    // where: oneof with two pure-token subargs (LEFT, RIGHT)
    assert_bulk_eq(arg_field(&args[2], "name").unwrap(), "where");
    assert_bulk_eq(arg_field(&args[2], "type").unwrap(), "oneof");
    let subargs = arg_field(&args[2], "arguments").expect("oneof has arguments");
    let RespFrame::Array(Some(sub)) = subargs else {
        panic!("subargs must be array");
    };
    assert_eq!(sub.len(), 2);
    assert_bulk_eq(arg_field(&sub[0], "type").unwrap(), "pure-token");
    assert_bulk_eq(arg_field(&sub[0], "token").unwrap(), "LEFT");
    assert_bulk_eq(arg_field(&sub[1], "type").unwrap(), "pure-token");
    assert_bulk_eq(arg_field(&sub[1], "token").unwrap(), "RIGHT");

    // count: integer, COUNT token, optional flag
    assert_bulk_eq(arg_field(&args[3], "name").unwrap(), "count");
    assert_bulk_eq(arg_field(&args[3], "type").unwrap(), "integer");
    assert_bulk_eq(arg_field(&args[3], "token").unwrap(), "COUNT");
    let flags = arg_field(&args[3], "flags").expect("count arg has flags");
    let RespFrame::Array(Some(flag_items)) = flags else {
        panic!("flags must be array");
    };
    assert_eq!(flag_items.len(), 1);
    assert_eq!(
        flag_items[0],
        RespFrame::SimpleString("optional".to_string())
    );
}
