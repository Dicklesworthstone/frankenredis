use fr_command::dispatch_argv;
use fr_protocol::RespFrame;
use fr_store::Store;

// (frankenredis-bpf4q) Pin that COMMAND DOCS for container commands
// (CLUSTER/CONFIG/CLIENT/ACL/OBJECT/MEMORY/SCRIPT/FUNCTION/COMMAND/
// LATENCY/SLOWLOG/MODULE/PUBSUB/XGROUP/XINFO) emits a `subcommands`
// field populated with one full DOCS entry per matching SUBCOMMAND_TABLE
// row. DEBUG carries no subcommand rows (vendored's DEBUG subcommands
// are routed internally without per-subcommand JSON files), so the
// field is correctly omitted.

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

fn field<'a>(kv: &'a [RespFrame], key: &str) -> Option<&'a RespFrame> {
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

fn assert_bulk(frame: &RespFrame, expected: &str) {
    match frame {
        RespFrame::BulkString(Some(b)) => {
            assert_eq!(b.as_slice(), expected.as_bytes(), "expected {expected}")
        }
        other => panic!("expected BulkString({expected}), got {other:?}"),
    }
}

#[test]
fn command_docs_cluster_emits_all_subcommands() {
    let out = run("CLUSTER");
    let kv = kv(&out);
    let subs = field(kv, "subcommands").expect("subcommands field must be present");
    let RespFrame::Array(Some(arr)) = subs else {
        panic!("subcommands must be array");
    };
    // RESP2 flat layout: 2N elements (name, docs).
    assert!(arr.len().is_multiple_of(2), "subcommands must be 2N");
    let count = arr.len() / 2;
    // Vendored emits 28 cluster subcommands (verified via differential
    // probe against Redis 7.2.4).
    assert_eq!(count, 28, "cluster subcommand count");

    // Verify each subcommand fullname starts with "cluster|" and the
    // accompanying docs map declares group=cluster (inherited from
    // parent via command_group_for_docs's split_once('|') recursion).
    let mut i = 0;
    while i + 1 < arr.len() {
        let RespFrame::BulkString(Some(sub_name)) = &arr[i] else {
            panic!("subcommand name must be BulkString");
        };
        let sub_str = std::str::from_utf8(sub_name).unwrap();
        assert!(sub_str.starts_with("cluster|"), "{sub_str}");

        let RespFrame::Array(Some(sub_kv)) = &arr[i + 1] else {
            panic!("subcommand docs must be Array");
        };
        assert_bulk(field(sub_kv, "group").unwrap(), "cluster");
        i += 2;
    }
}

#[test]
fn command_docs_subcommand_counts_match_subcommand_table() {
    // Sanity check that every container parent in
    // SUBCOMMAND_PARENTS_WITH_DOCS fans out the right number of
    // subcommands. The expected counts come from a differential probe
    // against vendored Redis 7.2.4 — they MUST stay in lockstep, since
    // any drift would indicate either a SUBCOMMAND_TABLE addition or
    // an upstream-incompatible removal.
    let expected: &[(&str, usize)] = &[
        ("CLUSTER", 28),
        ("CONFIG", 5),
        ("CLIENT", 18),
        ("COMMAND", 7),
        ("ACL", 13),
        ("OBJECT", 5),
        ("XGROUP", 6),
        ("XINFO", 4),
        ("MEMORY", 6),
        ("SCRIPT", 6),
        ("FUNCTION", 9),
        ("LATENCY", 7),
        ("SLOWLOG", 4),
        ("MODULE", 5),
        ("PUBSUB", 6),
    ];
    for (cmd, expected_count) in expected {
        let out = run(cmd);
        let kv = kv(&out);
        let subs = field(kv, "subcommands")
            .unwrap_or_else(|| panic!("{cmd}: subcommands field must be present"));
        let RespFrame::Array(Some(arr)) = subs else {
            panic!("{cmd}: subcommands must be array");
        };
        assert_eq!(arr.len() / 2, *expected_count, "{cmd}");
    }
}

#[test]
fn command_docs_debug_omits_subcommands_field() {
    // DEBUG handles its subcommands internally without separate JSON
    // files, so SUBCOMMAND_TABLE has no debug|* rows and the field is
    // suppressed — matching vendored.
    let out = run("DEBUG");
    let kv = kv(&out);
    assert!(field(kv, "subcommands").is_none());
}

#[test]
fn command_docs_non_container_omits_subcommands_field() {
    // Leaf commands must never grow a subcommands field. Guard against
    // an over-aggressive fan-out.
    for cmd in ["GET", "SET", "EXPIRE", "BITCOUNT", "LMPOP", "ZADD"] {
        let out = run(cmd);
        let kv = kv(&out);
        assert!(
            field(kv, "subcommands").is_none(),
            "{cmd} must not emit subcommands"
        );
    }
}
