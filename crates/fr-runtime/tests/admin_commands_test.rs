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
fn debug_jmap_rejects_with_subcommand_envelope() {
    // Upstream debug.c::debugCommand has no JMAP subcommand; it
    // falls through to addReplySubcommandSyntaxError. Differential
    // probe vs vendored 7.2.4 confirmed both `DEBUG JMAP` and
    // `DEBUG jmap` return the envelope with the input-case token
    // preserved. (frankenredis-dbgjmap)
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP"]), 0);
    assert_eq!(
        resp,
        RespFrame::Error(
            "ERR unknown subcommand or wrong number of arguments for 'JMAP'. Try DEBUG HELP."
                .to_string()
        )
    );
    let resp = rt.execute_frame(command(&[b"DEBUG", b"jmap"]), 0);
    assert_eq!(
        resp,
        RespFrame::Error(
            "ERR unknown subcommand or wrong number of arguments for 'jmap'. Try DEBUG HELP."
                .to_string()
        )
    );
}

#[test]
fn debug_reload_round_trips_in_memory_without_persistence_per_upstream() {
    // Upstream Redis 7.2.4 debug.c::debugCommand reloads via an
    // in-memory RDB save+load round-trip even when neither AOF nor
    // RDB persistence is configured — the goal is integrity
    // verification of the serializer/deserializer pair, not file IO.
    // (Pinned by frankenredis-8hzzv; this used to assert the inverse,
    // which was a fr-only divergence.)
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    rt.execute_frame(command(&[b"SET", b"k", b"v"]), 0);
    let resp = rt.execute_frame(command(&[b"DEBUG", b"RELOAD"]), 0);
    assert_eq!(resp, RespFrame::SimpleString("OK".to_string()));
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"k"]), 0),
        RespFrame::BulkString(Some(b"v".to_vec())),
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
    // (frankenredis-dbgjmap) Upstream emits the same subcommand-syntax
    // envelope regardless of extra argv tail, since JMAP is unknown.
    let mut rt = Runtime::default_strict();
    rt.set_enable_debug_command("yes");
    let resp = rt.execute_frame(command(&[b"DEBUG", b"JMAP", b"extra"]), 0);
    assert_eq!(
        resp,
        RespFrame::Error(
            "ERR unknown subcommand or wrong number of arguments for 'JMAP'. Try DEBUG HELP."
                .to_string()
        )
    );
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

#[test]
fn info_keyspace_reports_avg_ttl_for_volatile_keys() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"persist", b"v"]), 100);
    rt.execute_frame(command(&[b"SET", b"soon", b"v", b"PX", b"1000"]), 100);
    rt.execute_frame(command(&[b"SET", b"later", b"v", b"PX", b"3000"]), 100);

    let info = rt.execute_frame(command(&[b"INFO", b"keyspace"]), 600);
    let text = extract_bulk(&info);
    assert!(
        text.contains("db0:keys=3,expires=2,avg_ttl=1500\r\n"),
        "INFO keyspace should report current volatile TTL mean, got {text:?}"
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

// ── commandstats counts what a SCRIPT ran (frankenredis-scriptstats) ──
//
// Upstream routes redis.call through the same call() a client command takes, so
// `redis.call('GET', k)` lands in cmdstat_get exactly like a direct GET -- which is
// what makes INFO commandstats usable for finding what a workload does when the
// workload is scripts. fr dispatched straight into fr_command::dispatch_argv, which
// the runtime's histogram recorder never sees, so a script-driven server reported
// only cmdstat_eval and the inner commands were invisible.
//
// The three outcomes are separate counters and are NOT interchangeable:
//   * admitted and succeeded  -> calls
//   * admitted and errored    -> calls AND failed_calls
//   * never admitted (arity)  -> rejected_calls ONLY, calls stays 0
//   * unknown command         -> no row at all
// Expected values are what redis 7.2.4 reported for the same sequences.

/// One `cmdstat_<name>` field, or None when the row is absent entirely.
fn cmdstat_field(rt: &mut Runtime, row: &str, field: &str) -> Option<String> {
    let info = extract_bulk(&rt.execute_frame(command(&[b"INFO", b"commandstats"]), 0));
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{row}:")) {
            for part in rest.split(',') {
                if let Some(v) = part.strip_prefix(&format!("{field}=")) {
                    return Some(v.trim().to_string());
                }
            }
            return None;
        }
    }
    None
}

fn resetstat(rt: &mut Runtime) {
    rt.execute_frame(command(&[b"CONFIG", b"RESETSTAT"]), 0);
}

#[test]
fn commandstats_counts_commands_issued_by_a_script() {
    let mut rt = Runtime::default_strict();

    // A successful inner write is counted like a direct one.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[b"EVAL", b"redis.call('SET', KEYS[1], 'v') return 1", b"1", b"k"]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("1"));
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_set", "failed_calls").as_deref(),
        Some("0")
    );
    // ...and the EVAL itself is still counted once, not replaced by the inner row.
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_eval", "calls").as_deref(), Some("1"));

    // Every inner call counts, not just the first.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[
            b"EVAL",
            b"for i=1,3 do redis.call('SET', KEYS[1], i) end return 1",
            b"1",
            b"k",
        ]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("3"));

    // A container subcommand is keyed by its `parent|sub` fullname, as upstream does.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[b"EVAL", b"return redis.call('OBJECT', 'ENCODING', KEYS[1])", b"1", b"k"]),
        0,
    );
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_object|encoding", "calls").as_deref(),
        Some("1")
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_object", "calls"), None);

    // A script that calls nothing must not invent rows -- the anti-vacuity half, since a
    // fix that counted unconditionally would satisfy every assertion above.
    resetstat(&mut rt);
    rt.execute_frame(command(&[b"EVAL", b"return 1", b"0"]), 0);
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls"), None);
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_get", "calls"), None);
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_eval", "calls").as_deref(), Some("1"));

    // A direct command is still counted exactly once, not twice.
    resetstat(&mut rt);
    rt.execute_frame(command(&[b"SET", b"direct", b"v"]), 0);
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("1"));
}

#[test]
fn commandstats_separates_failed_and_rejected_inner_calls() {
    let mut rt = Runtime::default_strict();

    // Admitted, then errored: calls AND failed_calls, per upstream's call().
    resetstat(&mut rt);
    rt.execute_frame(command(&[b"LPUSH", b"l", b"a"]), 0);
    rt.execute_frame(
        command(&[b"EVAL", b"return redis.call('GET', KEYS[1])", b"1", b"l"]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_get", "calls").as_deref(), Some("1"));
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_get", "failed_calls").as_deref(),
        Some("1")
    );
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_get", "rejected_calls").as_deref(),
        Some("0")
    );

    // redis.pcall swallows the error, but the command still ran and still failed.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[b"EVAL", b"redis.pcall('GET', KEYS[1]) return 1", b"1", b"l"]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_get", "calls").as_deref(), Some("1"));
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_get", "failed_calls").as_deref(),
        Some("1")
    );

    // Wrong arity is REJECTED: it never ran, so calls stays 0. Classifying this from the
    // error alone would read calls=1,failed_calls=1 -- the distinction this pins.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[
            b"EVAL",
            b"local ok = pcall(function() redis.call('GET') end) return tostring(ok)",
            b"0",
        ]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_get", "calls").as_deref(), Some("0"));
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_get", "rejected_calls").as_deref(),
        Some("1")
    );
    assert_eq!(
        cmdstat_field(&mut rt, "cmdstat_get", "failed_calls").as_deref(),
        Some("0")
    );

    // An unknown command has no table entry, so upstream has no row for it at all.
    resetstat(&mut rt);
    rt.execute_frame(
        command(&[
            b"EVAL",
            b"local ok = pcall(function() redis.call('NOSUCHCMD') end) return tostring(ok)",
            b"0",
        ]),
        0,
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_nosuchcmd", "calls"), None);
}

// ── latency-tracking gates the PERCENTILES, and a toggle discards them ──
//
// (frankenredis-trackgate) Upstream frees a command's latency histogram when
// `latency-tracking` goes off and allocates a fresh one on the next command once it
// is back on. fr recorded the buckets under the same flag but never cleared them and
// never gated the section, so percentiles collected while tracking was ON stayed
// visible after it was turned OFF -- redis reports the section absent immediately.
//
// Expected values are what redis 7.2.4 reported for the same sequences.

fn latencystats_has(rt: &mut Runtime, cmd: &str) -> bool {
    let info = extract_bulk(&rt.execute_frame(command(&[b"INFO", b"latencystats"]), 0));
    info.lines()
        .any(|line| line.starts_with(&format!("latency_percentiles_usec_{cmd}:")))
}

fn set_tracking(rt: &mut Runtime, on: bool) {
    let value: &[u8] = if on { b"yes" } else { b"no" };
    rt.execute_frame(command(&[b"CONFIG", b"SET", b"latency-tracking", value]), 0);
}

#[test]
fn latency_tracking_toggle_discards_the_percentiles() {
    let mut rt = Runtime::default_strict();

    // Collected while ON: the section is there.
    set_tracking(&mut rt, true);
    resetstat(&mut rt);
    rt.execute_frame(command(&[b"SET", b"a", b"v"]), 0);
    assert!(latencystats_has(&mut rt, "set"), "percentiles expected while tracking");
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("1"));

    // Turned OFF: the percentiles go immediately, WITHOUT waiting for another command.
    // fr kept showing them.
    set_tracking(&mut rt, false);
    assert!(
        !latencystats_has(&mut rt, "set"),
        "percentiles must vanish as soon as tracking is off"
    );
    // ...and the command counters are untouched by the toggle: this is the half of the
    // flag that must NOT change, so a fix that simply reset everything would fail here.
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("1"));

    // Back ON, before any new command: still absent, because the old buckets were
    // discarded rather than merely hidden. This is what separates clearing from gating.
    set_tracking(&mut rt, true);
    assert!(
        !latencystats_has(&mut rt, "set"),
        "discarded percentiles must not reappear when tracking is re-enabled"
    );
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("1"));

    // One more command and the section is rebuilt.
    rt.execute_frame(command(&[b"SET", b"b", b"v"]), 0);
    assert!(latencystats_has(&mut rt, "set"), "percentiles rebuild after a command");
    assert_eq!(cmdstat_field(&mut rt, "cmdstat_set", "calls").as_deref(), Some("2"));
}

#[test]
fn latencystats_is_absent_while_tracking_is_off() {
    let mut rt = Runtime::default_strict();
    set_tracking(&mut rt, false);
    resetstat(&mut rt);
    for _ in 0..5 {
        rt.execute_frame(command(&[b"SET", b"c", b"v"]), 0);
    }
    assert!(
        !latencystats_has(&mut rt, "set"),
        "no percentiles may accumulate while tracking is off"
    );
    // Re-enabling must not surface anything gathered during the untracked window.
    set_tracking(&mut rt, true);
    assert!(
        !latencystats_has(&mut rt, "set"),
        "the untracked window must leave no percentiles behind"
    );
}
