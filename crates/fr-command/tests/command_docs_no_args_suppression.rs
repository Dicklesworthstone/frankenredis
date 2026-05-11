use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-f7670) Pin that container commands (DEBUG, CLUSTER,
// CONFIG, CLIENT, ...) and no-arg commands no longer emit the
// arity-derived `arg1 string optional` placeholder. fr now harvests
// UPSTREAM_COMMAND_DOCS_HAS_ARGS at build time and the consumer
// suppresses the fallback when the upstream JSON declared no
// `arguments` array. Vendored omits the entire `arguments` field for
// these commands; fr now mirrors that.

fn run(name: &str) -> RespFrame {
    let mut store = Store::new();
    dispatch_argv(
        &[b"COMMAND".to_vec(), b"DOCS".to_vec(), name.as_bytes().to_vec()],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS should succeed")
}

fn kv(out: &RespFrame) -> &[RespFrame] {
    let RespFrame::Array(Some(entries)) = out else {
        panic!("expected outer Array");
    };
    let RespFrame::Array(Some(kv)) = &entries[1] else {
        panic!("expected inner kv array");
    };
    kv
}

fn has_top_level_field(kv: &[RespFrame], key: &str) -> bool {
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == key.as_bytes()
        {
            return true;
        }
        i += 2;
    }
    false
}

#[test]
fn command_docs_containers_omit_arguments_field() {
    // Pre-fix, every -2-arity container emitted a fake
    // `arguments name=arg1 type=string optional=1` placeholder.
    // Upstream containers carry no positional args (cmd->args is NULL
    // because subcommand handlers route the args themselves) and so
    // emit no `arguments` field at all.
    for cmd in [
        "DEBUG",
        "CLUSTER",
        "CONFIG",
        "CLIENT",
        "FUNCTION",
        "LATENCY",
        "MEMORY",
        "MODULE",
        "OBJECT",
        "PUBSUB",
        "SCRIPT",
        "SLOWLOG",
        "ACL",
        "XGROUP",
        "XINFO",
    ] {
        let out = run(cmd);
        let kv = kv(&out);
        assert!(
            !has_top_level_field(kv, "arguments"),
            "{cmd}: must NOT emit a top-level `arguments` field",
        );
    }
}

#[test]
fn command_docs_real_commands_still_emit_arguments() {
    // Negative test — commands with declared upstream args must still
    // emit the field. Guards against an over-aggressive suppression.
    for cmd in ["GET", "SET", "LPUSH", "ZADD", "EXPIRE", "BITCOUNT", "LMPOP"] {
        let out = run(cmd);
        let kv = kv(&out);
        assert!(
            has_top_level_field(kv, "arguments"),
            "{cmd}: must emit `arguments` field",
        );
    }
}
