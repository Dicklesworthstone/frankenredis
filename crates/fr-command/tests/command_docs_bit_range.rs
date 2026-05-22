use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-i0roi) Pin COMMAND DOCS BITCOUNT and BITPOS layouts.
// Pre-fix, fr emitted generic `arg2 string optional`. Upstream layout:
//   BITCOUNT: key + range block (start, end, unit oneof BYTE/BIT)
//   BITPOS:   key + bit + range block (start + end-unit-block (end +
//             unit oneof BYTE/BIT))
// Source: legacy_redis_code/redis/src/commands.def.

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

fn bulk_eq(frame: &RespFrame, expected: &str) -> bool {
    matches!(frame, RespFrame::BulkString(Some(b)) if b.as_slice() == expected.as_bytes())
}

fn assert_bulk(frame: &RespFrame, expected: &str) {
    assert!(
        bulk_eq(frame, expected),
        "expected {expected}, got {frame:?}"
    );
}

fn assert_unit_oneof(unit: &RespFrame) {
    assert_bulk(arg_field(unit, "name").unwrap(), "unit");
    assert_bulk(arg_field(unit, "type").unwrap(), "oneof");
    assert_bulk(arg_field(unit, "since").unwrap(), "7.0.0");
    let RespFrame::Array(Some(sub)) = arg_field(unit, "arguments").unwrap() else {
        panic!("unit arguments must be array");
    };
    assert_eq!(sub.len(), 2);
    assert_bulk(arg_field(&sub[0], "token").unwrap(), "BYTE");
    assert_bulk(arg_field(&sub[1], "token").unwrap(), "BIT");
}

#[test]
fn command_docs_bitcount_matches_upstream_layout() {
    let out = run("BITCOUNT");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 2);
    assert_bulk(arg_field(&args[0], "name").unwrap(), "key");
    assert_bulk(arg_field(&args[1], "name").unwrap(), "range");
    assert_bulk(arg_field(&args[1], "type").unwrap(), "block");

    let RespFrame::Array(Some(sub)) = arg_field(&args[1], "arguments").unwrap() else {
        panic!("range arguments must be array");
    };
    assert_eq!(sub.len(), 3);
    assert_bulk(arg_field(&sub[0], "name").unwrap(), "start");
    assert_bulk(arg_field(&sub[1], "name").unwrap(), "end");
    assert_unit_oneof(&sub[2]);
}

#[test]
fn command_docs_bitpos_matches_upstream_layout() {
    let out = run("BITPOS");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 3);
    assert_bulk(arg_field(&args[0], "name").unwrap(), "key");
    assert_bulk(arg_field(&args[1], "name").unwrap(), "bit");
    assert_bulk(arg_field(&args[1], "type").unwrap(), "integer");
    assert_bulk(arg_field(&args[2], "name").unwrap(), "range");
    assert_bulk(arg_field(&args[2], "type").unwrap(), "block");

    let RespFrame::Array(Some(range_sub)) = arg_field(&args[2], "arguments").unwrap() else {
        panic!("range arguments must be array");
    };
    assert_eq!(range_sub.len(), 2);
    assert_bulk(arg_field(&range_sub[0], "name").unwrap(), "start");
    assert_bulk(arg_field(&range_sub[1], "name").unwrap(), "end-unit-block");
    assert_bulk(arg_field(&range_sub[1], "type").unwrap(), "block");

    let RespFrame::Array(Some(end_block)) = arg_field(&range_sub[1], "arguments").unwrap() else {
        panic!("end-unit-block arguments must be array");
    };
    assert_eq!(end_block.len(), 2);
    assert_bulk(arg_field(&end_block[0], "name").unwrap(), "end");
    assert_unit_oneof(&end_block[1]);
}
