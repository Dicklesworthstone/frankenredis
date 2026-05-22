use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-kb8fl) Pin COMMAND DOCS for the pubsub family.
// Pre-fix, fr emitted generic `arg1 string optional`. Upstream layouts
// per legacy_redis_code/redis/src/commands.def:
//   SUBSCRIBE/SSUBSCRIBE: channel/shardchannel string [multiple]
//   UNSUBSCRIBE/SUNSUBSCRIBE: channel/shardchannel string [optional,multiple]
//   PSUBSCRIBE: pattern pattern [multiple]
//   PUNSUBSCRIBE: pattern pattern [optional,multiple]
//   PUBLISH: channel string + message string
//   SPUBLISH: shardchannel string + message string

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

fn assert_flags(arg: &RespFrame, expected: &[&str]) {
    let flags = arg_field(arg, "flags").unwrap();
    let RespFrame::Array(Some(items)) = flags else {
        panic!("flags must be array, got {flags:?}");
    };
    assert_eq!(items.len(), expected.len(), "flag count");
    for (i, f) in expected.iter().enumerate() {
        assert_eq!(items[i], RespFrame::SimpleString((*f).to_string()));
    }
}

fn assert_single(cmd: &str, expected_name: &str, expected_type: &str, expected_flags: &[&str]) {
    let out = run(cmd);
    let args = arguments_array(&out);
    assert_eq!(args.len(), 1, "{cmd}: one arg");
    assert_bulk(arg_field(&args[0], "name").unwrap(), expected_name);
    assert_bulk(arg_field(&args[0], "type").unwrap(), expected_type);
    assert_bulk(arg_field(&args[0], "display_text").unwrap(), expected_name);
    assert_flags(&args[0], expected_flags);
}

#[test]
fn command_docs_subscribe_matches_upstream_layout() {
    assert_single("SUBSCRIBE", "channel", "string", &["multiple"]);
}

#[test]
fn command_docs_ssubscribe_matches_upstream_layout() {
    assert_single("SSUBSCRIBE", "shardchannel", "string", &["multiple"]);
}

#[test]
fn command_docs_unsubscribe_matches_upstream_layout() {
    assert_single(
        "UNSUBSCRIBE",
        "channel",
        "string",
        &["optional", "multiple"],
    );
}

#[test]
fn command_docs_sunsubscribe_matches_upstream_layout() {
    assert_single(
        "SUNSUBSCRIBE",
        "shardchannel",
        "string",
        &["optional", "multiple"],
    );
}

#[test]
fn command_docs_psubscribe_matches_upstream_layout() {
    assert_single("PSUBSCRIBE", "pattern", "pattern", &["multiple"]);
}

#[test]
fn command_docs_punsubscribe_matches_upstream_layout() {
    assert_single(
        "PUNSUBSCRIBE",
        "pattern",
        "pattern",
        &["optional", "multiple"],
    );
}

#[test]
fn command_docs_publish_matches_upstream_layout() {
    let out = run("PUBLISH");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 2);
    assert_bulk(arg_field(&args[0], "name").unwrap(), "channel");
    assert_bulk(arg_field(&args[0], "type").unwrap(), "string");
    assert_bulk(arg_field(&args[1], "name").unwrap(), "message");
    assert_bulk(arg_field(&args[1], "type").unwrap(), "string");
}

#[test]
fn command_docs_spublish_matches_upstream_layout() {
    let out = run("SPUBLISH");
    let args = arguments_array(&out);
    assert_eq!(args.len(), 2);
    assert_bulk(arg_field(&args[0], "name").unwrap(), "shardchannel");
    assert_bulk(arg_field(&args[0], "type").unwrap(), "string");
    assert_bulk(arg_field(&args[1], "name").unwrap(), "message");
    assert_bulk(arg_field(&args[1], "type").unwrap(), "string");
}
