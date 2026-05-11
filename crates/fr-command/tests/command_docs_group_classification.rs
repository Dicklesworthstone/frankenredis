use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-7wcuq) Pin upstream-correct group classification for
// 15 commands that were misclassified by command_group_for_docs's
// prefix/catch-all heuristics. Authoritative source: legacy_redis_code/
// redis/src/commands/*.json `group` field.

fn group_of(cmd: &str) -> String {
    let mut store = Store::new();
    let out = dispatch_argv(
        &[b"COMMAND".to_vec(), b"DOCS".to_vec(), cmd.as_bytes().to_vec()],
        &mut store,
        0,
    )
    .expect("COMMAND DOCS should succeed");
    let RespFrame::Array(Some(entries)) = out else {
        panic!("expected outer Array");
    };
    let RespFrame::Array(Some(kv)) = &entries[1] else {
        panic!("expected inner kv array");
    };
    let mut i = 0;
    while i + 1 < kv.len() {
        if let RespFrame::BulkString(Some(k)) = &kv[i]
            && k.as_slice() == b"group"
            && let RespFrame::BulkString(Some(v)) = &kv[i + 1]
        {
            return String::from_utf8(v.clone()).unwrap();
        }
        i += 2;
    }
    panic!("group field missing");
}

#[test]
fn command_docs_group_corrections() {
    let cases = [
        // Were misclassified per `redis-cli -p 16380 COMMAND DOCS X`.
        ("WAIT", "generic"),
        ("WAITAOF", "generic"),
        ("MIGRATE", "generic"),
        ("FLUSHALL", "server"),
        ("FLUSHDB", "server"),
        ("DBSIZE", "server"),
        ("MEMORY", "server"),
        ("TIME", "server"),
        ("RESTORE-ASKING", "server"),
        ("LOLWUT", "server"),
        ("LCS", "string"),
        ("RPUSH", "list"),
        ("RPUSHX", "list"),
        ("RPOP", "list"),
        ("RPOPLPUSH", "list"),
    ];
    for (cmd, expected) in cases {
        assert_eq!(group_of(cmd), expected, "{cmd}");
    }
}

#[test]
fn command_docs_group_unaffected_commands_still_correct() {
    // Negative test — ensure nearby commands kept their right group.
    let cases = [
        ("LPUSH", "list"),
        ("BLPOP", "list"),
        ("SADD", "set"),
        ("ZADD", "sorted-set"),
        ("HSET", "hash"),
        ("XADD", "stream"),
        ("SETBIT", "bitmap"),
        ("GETBIT", "bitmap"),
        ("PFADD", "hyperloglog"),
        ("GEOADD", "geo"),
        ("CONFIG", "server"),
        ("CLUSTER", "cluster"),
        ("CLIENT", "connection"),
        ("EVAL", "scripting"),
        ("MULTI", "transactions"),
        ("PUBLISH", "pubsub"),
        ("EXPIRE", "generic"),
        ("DEL", "generic"),
        ("GET", "string"),
        ("SET", "string"),
    ];
    for (cmd, expected) in cases {
        assert_eq!(group_of(cmd), expected, "{cmd}");
    }
}
