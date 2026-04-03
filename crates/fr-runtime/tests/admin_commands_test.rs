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
fn memory_stats_returns_bulk() {
    let mut rt = Runtime::default_strict();
    let stats = rt.execute_frame(command(&[b"MEMORY", b"STATS"]), 0);
    let text = extract_bulk(&stats);
    assert!(
        text.contains("peak.allocated:") || text.contains("total.allocated:"),
        "MEMORY STATS should contain allocation stats"
    );
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
    let mut rt = Runtime::default_strict();
    let graph = rt.execute_frame(command(&[b"LATENCY", b"GRAPH", b"command"]), 0);
    assert_eq!(
        graph,
        RespFrame::Error("No samples available for event 'command'".to_string())
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

#[test]
fn debug_sleep_zero() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SLEEP", b"0"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_set_active_expire() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SET-ACTIVE-EXPIRE", b"1"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_jmap_returns_ok() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_reload_returns_ok() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
}

#[test]
fn debug_object_requires_key_argument() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"OBJECT"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_jmap_rejects_extra_arguments() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP", b"extra"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_set_active_expire_validates_value() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG", b"SET-ACTIVE-EXPIRE", b"2"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
}

#[test]
fn debug_wrong_arity() {
    let mut rt = Runtime::default_strict();
    let resp = rt.execute_frame(command(&[b"DEBUG"]), 0);
    assert!(matches!(resp, RespFrame::Error(_)));
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
