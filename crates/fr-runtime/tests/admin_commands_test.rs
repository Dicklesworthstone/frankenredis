//! Integration tests for administrative commands: MEMORY, LATENCY, DEBUG, INFO.
//! These commands are implemented but had thin dedicated test coverage.

use fr_protocol::RespFrame;
use fr_runtime::Runtime;

fn command(parts: &[&[u8]]) -> RespFrame {
    RespFrame::Array(Some(
        parts
            .iter()
            .map(|part| RespFrame::BulkString(Some((*part).to_vec())))
            .collect(),
    ))
}

fn is_bulk_string(frame: &RespFrame) -> bool {
    matches!(frame, RespFrame::BulkString(Some(_)))
}

fn extract_bulk(frame: &RespFrame) -> String {
    match frame {
        RespFrame::BulkString(Some(data)) => String::from_utf8_lossy(data).to_string(),
        other => panic!("expected bulk string, got: {other:?}"),
    }
}

// ── MEMORY ──────────────────────────────────────────

#[test]
fn memory_usage_existing_key() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"mem_key", b"hello world"]), 0);

    let usage = rt.execute_frame(command(&[b"MEMORY", b"USAGE", b"mem_key"]), 1);
    match usage {
        RespFrame::Integer(n) => assert!(n > 0, "MEMORY USAGE should return positive size"),
        other => panic!("expected integer from MEMORY USAGE, got: {other:?}"),
    }
}

#[test]
fn memory_usage_missing_key() {
    let mut rt = Runtime::default_strict();
    let usage = rt.execute_frame(command(&[b"MEMORY", b"USAGE", b"nosuchkey"]), 0);
    assert_eq!(usage, RespFrame::BulkString(None));
}

#[test]
fn memory_usage_with_samples() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"mem_key", b"value"]), 0);

    let usage = rt.execute_frame(
        command(&[b"MEMORY", b"USAGE", b"mem_key", b"SAMPLES", b"5"]),
        1,
    );
    match usage {
        RespFrame::Integer(n) => assert!(n > 0),
        other => panic!("expected integer, got: {other:?}"),
    }
}

#[test]
fn memory_doctor_returns_bulk() {
    let mut rt = Runtime::default_strict();
    let doctor = rt.execute_frame(command(&[b"MEMORY", b"DOCTOR"]), 0);
    assert!(
        is_bulk_string(&doctor),
        "MEMORY DOCTOR should return bulk string"
    );
}

#[test]
fn memory_purge_ok() {
    let mut rt = Runtime::default_strict();
    let purge = rt.execute_frame(command(&[b"MEMORY", b"PURGE"]), 0);
    assert_eq!(purge, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn memory_stats_returns_keyed_array() {
    // Upstream `object.c::memoryCommand` reply for MEMORY STATS is a
    // RESP map/array of (bulk-string-key, integer-value) pairs — see
    // legacy_redis_code/redis/src/object.c:1566 (`addReplyMapLen` + a
    // run of `addReplyBulkCString` / `addReplyLongLong`). The previous
    // shape of this test asserted a single bulk-string body with
    // `key:value\n` lines (mistaking MEMORY STATS for INFO-style
    // output) and so always failed against our Redis-correct array
    // reply. (br-frankenredis-3kdz)
    let mut rt = Runtime::default_strict();
    let stats = rt.execute_frame(command(&[b"MEMORY", b"STATS"]), 0);
    let RespFrame::Array(Some(items)) = stats else {
        panic!("MEMORY STATS must reply with an array, got: {stats:?}");
    };
    assert!(
        !items.is_empty(),
        "MEMORY STATS must produce a non-empty key-value pair array"
    );
    assert!(
        items.len() >= 2 && items.len() % 2 == 0,
        "MEMORY STATS reply length must be an even number of entries (key, value, ...), got {}",
        items.len()
    );

    // Pull keys (every even-indexed bulk string) and verify the canonical
    // upstream-documented allocation-stat keys are all present. This
    // catches both shape regressions (someone returning a single bulk
    // string again) and silent label drift.
    let keys: Vec<String> = items
        .chunks(2)
        .filter_map(|pair| match &pair[0] {
            RespFrame::BulkString(Some(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect();
    for required in &["peak.allocated", "total.allocated", "startup.allocated"] {
        assert!(
            keys.iter().any(|k| k == required),
            "MEMORY STATS missing required key {required:?}; saw {keys:?}"
        );
    }

    // Every value paired with one of the well-known integer keys must
    // itself be an integer — guards against a paired bulk-string value
    // accidentally drifting in (which would break any client lib that
    // parses these as longs).
    for pair in items.chunks(2) {
        if let RespFrame::BulkString(Some(k)) = &pair[0] {
            let key = String::from_utf8_lossy(k);
            if matches!(
                key.as_ref(),
                "peak.allocated"
                    | "total.allocated"
                    | "startup.allocated"
                    | "replication.backlog"
                    | "aof.buffer"
                    | "keys.count"
            ) {
                assert!(
                    matches!(pair[1], RespFrame::Integer(_)),
                    "MEMORY STATS key {key:?} must pair with an integer value, got {:?}",
                    pair[1]
                );
            }
        }
    }
}

#[test]
fn memory_malloc_stats_returns_bulk() {
    let mut rt = Runtime::default_strict();
    let stats = rt.execute_frame(command(&[b"MEMORY", b"MALLOC-STATS"]), 0);
    assert!(
        is_bulk_string(&stats),
        "MEMORY MALLOC-STATS should return bulk string"
    );
}

#[test]
fn memory_help_returns_array() {
    let mut rt = Runtime::default_strict();
    let help = rt.execute_frame(command(&[b"MEMORY", b"HELP"]), 0);
    match help {
        RespFrame::Array(Some(items)) => {
            assert!(
                !items.is_empty(),
                "MEMORY HELP should return non-empty array"
            );
        }
        other => panic!("expected array from MEMORY HELP, got: {other:?}"),
    }
}

#[test]
fn memory_wrong_arity() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"MEMORY"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn memory_unknown_subcommand() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"MEMORY", b"NOSUCH"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

// ── LATENCY ─────────────────────────────────────────

#[test]
fn latency_latest_returns_array() {
    let mut rt = Runtime::default_strict();
    let latest = rt.execute_frame(command(&[b"LATENCY", b"LATEST"]), 0);
    match latest {
        RespFrame::Array(Some(_)) => {}
        other => panic!("expected array from LATENCY LATEST, got: {other:?}"),
    }
}

#[test]
fn latency_history_returns_empty_array() {
    let mut rt = Runtime::default_strict();
    let history = rt.execute_frame(command(&[b"LATENCY", b"HISTORY", b"command"]), 0);
    assert_eq!(history, RespFrame::Array(Some(Vec::new())));
}

#[test]
fn latency_reset_returns_integer() {
    let mut rt = Runtime::default_strict();
    let reset = rt.execute_frame(command(&[b"LATENCY", b"RESET"]), 0);
    assert_eq!(reset, RespFrame::Integer(0));
}

#[test]
fn latency_doctor_returns_bulk_string() {
    let mut rt = Runtime::default_strict();
    let doctor = rt.execute_frame(command(&[b"LATENCY", b"DOCTOR"]), 0);
    assert!(
        is_bulk_string(&doctor),
        "LATENCY DOCTOR should return bulk string"
    );
}

#[test]
fn latency_graph_without_samples_returns_error() {
    // (br-frankenredis-latgrapherr)
    let mut rt = Runtime::default_strict();
    let graph = rt.execute_frame(command(&[b"LATENCY", b"GRAPH", b"command"]), 0);
    assert_eq!(
        graph,
        RespFrame::Error("ERR No samples available for event 'command'".to_string())
    );
}

#[test]
fn latency_history_wrong_arity() {
    let mut rt = Runtime::default_strict();
    let history = rt.execute_frame(command(&[b"LATENCY", b"HISTORY"]), 0);
    assert!(matches!(history, RespFrame::Error(_)));
}

#[test]
fn latency_graph_wrong_arity() {
    let mut rt = Runtime::default_strict();
    let graph = rt.execute_frame(command(&[b"LATENCY", b"GRAPH"]), 0);
    assert!(matches!(graph, RespFrame::Error(_)));
}

#[test]
fn latency_help_returns_array() {
    let mut rt = Runtime::default_strict();
    let help = rt.execute_frame(command(&[b"LATENCY", b"HELP"]), 0);
    match help {
        RespFrame::Array(Some(items)) => {
            assert!(
                !items.is_empty(),
                "LATENCY HELP should return non-empty array"
            );
        }
        other => panic!("expected array from LATENCY HELP, got: {other:?}"),
    }
}

#[test]
fn latency_wrong_arity() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"LATENCY"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

// ── DEBUG ───────────────────────────────────────────

// Each DEBUG test below explicitly enables the command via
// set_enable_debug_command("yes") because the runtime defaults to
// "no" — the upstream Redis 7.2 default — so DEBUG is gated behind
// the canonical lockout error unless the operator opts in via the
// startup config or `--enable-debug-command` CLI flag.
// (br-frankenredis-j29y)

#[test]
fn debug_sleep_zero() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SLEEP", b"0"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_set_active_expire() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SET-ACTIVE-EXPIRE", b"1"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_jmap_returns_ok() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_reload_requires_configured_persistence() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 0);
    assert_eq!(
        resp,
        RespFrame::Error(
            "ERR DEBUG RELOAD requires configured appendonly or RDB persistence".to_string()
        )
    );
}

#[test]
fn debug_object_requires_key_argument() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"OBJECT"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_jmap_rejects_extra_arguments() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP", b"extra"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_set_active_expire_accepts_nonzero_atoi_values() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SET-ACTIVE-EXPIRE", b"2"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_wrong_arity() {
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_default_denies_with_upstream_lockout_error() {
    let mut rt = Runtime::default_strict();
    // Default runtime mirrors upstream: enable-debug-command=no.
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SLEEP", b"0"]), 0);
    let err = match resp {
        RespFrame::Error(e) => e,
        other => panic!("expected Error, got {other:?}"),
    };
    assert!(
        err.contains("DEBUG command not allowed"),
        "lockout wording must match upstream: {err}"
    );
}

// ── INFO ────────────────────────────────────────────

#[test]
fn info_returns_bulk_string() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO"]), 0);
    assert!(is_bulk_string(&info), "INFO should return bulk string");

    let text = extract_bulk(&info);
    assert!(
        text.contains("redis_version"),
        "INFO should contain redis_version"
    );
    assert!(
        text.contains("connected_clients"),
        "INFO should contain connected_clients"
    );
}

#[test]
fn info_server_section() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO", b"server"]), 0);
    let text = extract_bulk(&info);
    assert!(text.contains("redis_version"));
}

#[test]
fn info_memory_section() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO", b"memory"]), 0);
    let text = extract_bulk(&info);
    assert!(text.contains("used_memory"));
}

#[test]
fn info_stats_section() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO", b"stats"]), 0);
    let text = extract_bulk(&info);
    assert!(text.contains("total_commands_processed"));
}

#[test]
fn info_replication_section() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO", b"replication"]), 0);
    let text = extract_bulk(&info);
    assert!(text.contains("role:master"));
}

#[test]
fn info_keyspace_section_empty() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"INFO", b"keyspace"]), 0);
    let text = extract_bulk(&info);
    // Empty store should have keyspace section header but no db lines
    assert!(text.contains("Keyspace") || text.contains("keyspace"));
}

#[test]
fn info_keyspace_with_data() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"k1", b"v1"]), 0);
    rt.execute_frame(command(&[b"SET", b"k2", b"v2"]), 0);

    let info = rt.execute_frame(command(&[b"INFO", b"keyspace"]), 1);
    let text = extract_bulk(&info);
    assert!(
        text.contains("db0:keys="),
        "INFO keyspace should show db0 with keys"
    );
}

// ── ROLE ────────────────────────────────────────────

#[test]
fn role_returns_master() {
    let mut rt = Runtime::default_strict();
    let role = rt.execute_frame(command(&[b"ROLE"]), 0);
    match role {
        RespFrame::Array(Some(items)) => {
            assert!(!items.is_empty());
            assert_eq!(items[0], RespFrame::BulkString(Some(b"master".to_vec())));
        }
        other => panic!("expected array from ROLE, got: {other:?}"),
    }
}

// ── COMMAND ─────────────────────────────────────────

#[test]
fn command_count_returns_positive() {
    let mut rt = Runtime::default_strict();
    let count = rt.execute_frame(command(&[b"COMMAND", b"COUNT"]), 0);
    match count {
        RespFrame::Integer(n) => assert!(n > 200, "should have 200+ commands"),
        other => panic!("expected integer from COMMAND COUNT, got: {other:?}"),
    }
}

#[test]
fn command_info_known_command() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"COMMAND", b"INFO", b"GET"]), 0);
    match info {
        RespFrame::Array(Some(items)) => {
            assert_eq!(items.len(), 1, "COMMAND INFO GET should return 1 entry");
        }
        other => panic!("expected array from COMMAND INFO, got: {other:?}"),
    }
}

#[test]
fn command_info_unknown_command() {
    let mut rt = Runtime::default_strict();
    let info = rt.execute_frame(command(&[b"COMMAND", b"INFO", b"NOSUCHCMD"]), 0);
    match info {
        RespFrame::Array(Some(items)) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0], RespFrame::BulkString(None));
        }
        other => panic!("expected array with null from COMMAND INFO, got: {other:?}"),
    }
}

#[test]
fn command_getkeys_set() {
    let mut rt = Runtime::default_strict();
    let keys = rt.execute_frame(
        command(&[b"COMMAND", b"GETKEYS", b"SET", b"mykey", b"val"]),
        0,
    );
    assert_eq!(
        keys,
        RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"mykey".to_vec()))]))
    );
}

#[test]
fn command_getkeys_ping_reports_no_key_arguments() {
    let mut rt = Runtime::default_strict();
    let out = rt.execute_frame(command(&[b"COMMAND", b"GETKEYS", b"PING"]), 0);
    assert_eq!(
        out,
        RespFrame::Error("ERR The command has no key arguments".to_string())
    );
}

#[test]
fn command_getkeys_unknown_command_uses_redis_error() {
    let mut rt = Runtime::default_strict();
    let out = rt.execute_frame(command(&[b"COMMAND", b"GETKEYS", b"NOSUCHCMD", b"arg1"]), 0);
    assert_eq!(
        out,
        RespFrame::Error("ERR Invalid command specified".to_string())
    );
}

#[test]
fn command_getkeysandflags_set_and_rename_match_upstream_roles() {
    let mut rt = Runtime::default_strict();

    let set = rt.execute_frame(
        command(&[b"COMMAND", b"GETKEYSANDFLAGS", b"SET", b"alpha", b"1"]),
        0,
    );
    assert_eq!(
        set,
        RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"alpha".to_vec())),
            RespFrame::Array(Some(vec![
                RespFrame::SimpleString("OW".to_string()),
                RespFrame::SimpleString("update".to_string()),
            ])),
        ]))]))
    );

    let rename = rt.execute_frame(
        command(&[b"COMMAND", b"GETKEYSANDFLAGS", b"RENAME", b"src", b"dst"]),
        0,
    );
    assert_eq!(
        rename,
        RespFrame::Array(Some(vec![
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"src".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::SimpleString("RW".to_string()),
                    RespFrame::SimpleString("access".to_string()),
                    RespFrame::SimpleString("delete".to_string()),
                ])),
            ])),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"dst".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::SimpleString("OW".to_string()),
                    RespFrame::SimpleString("update".to_string()),
                ])),
            ])),
        ]))
    );
}
