use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fr_conformance::{
    CaseOutcome, HarnessConfig, LiveOracleConfig, run_fixture, run_live_redis_diff,
    run_live_redis_diff_for_cases, run_live_redis_multi_client_diff, run_protocol_fixture,
    run_replay_fixture, run_replication_handshake_fixture, run_smoke,
};
use fr_protocol::{RespFrame, parse_frame};
use fr_runtime::Runtime;

const CORE_STREAM_LIVE_STABLE_CASES: &[&str] = &[
    "xlen_missing_key",
    "xrange_invalid_bound_error",
    "xrevrange_invalid_bound_error",
    "xdel_missing_key",
    "xdel_missing_key_invalid_id_zero",
    "xadd_explicit_setup",
    "xadd_lower_id_error",
    "xadd_equal_id_error",
    "xrange_count_zero",
    "xrange_missing_key",
    "xrevrange_missing_key",
    "xadd_read1_first",
    "xadd_read1_second",
    "xadd_read2_first",
    "xadd_partial_auto_first_entry",
    "xadd_partial_auto_same_ms_increments_seq",
    "xadd_partial_auto_same_ms_increments_again",
    "xadd_partial_auto_new_ms_resets_seq",
    "xadd_partial_auto_verify_length",
    "xadd_partial_auto_lower_ms_rejected",
];

const CORE_SCRIPTING_LIVE_STABLE_CASES: &[&str] = &[
    "eval_return_integer",
    "eval_return_string",
    "eval_client_setname_rejected_from_script",
    "eval_client_getname_rejected_from_script",
    "eval_client_id_rejected_from_script",
    "eval_return_nil",
    "eval_return_true_as_integer_1",
    "eval_return_false_as_nil",
    "eval_return_table_as_array",
    "eval_arithmetic",
    "eval_string_concat",
    "eval_local_variable",
    "eval_if_true_branch",
    "eval_if_false_branch",
    "eval_numeric_for_loop",
    "eval_keys_and_argv",
    "eval_argv_access",
    "eval_redis_call_set",
    "eval_redis_call_get",
    "eval_redis_call_incr",
    "eval_tonumber",
    "eval_tostring",
    "eval_type_function",
    "eval_string_len_operator",
    "eval_table_len_operator",
    "eval_math_floor",
    "eval_string_sub",
    "eval_string_upper",
    "eval_pcall_success",
    "eval_wrong_arity",
    "eval_invalid_numkeys",
    "eval_while_loop",
    "eval_repeat_until",
    "eval_table_insert_and_return",
    "eval_status_reply",
    "eval_error_reply",
    "eval_pcall_catches_error",
    "eval_closure_captures_upvalue",
    "eval_closure_shared_upvalue_counter",
    "eval_closure_returns_function_result",
    "eval_local_recursive_function",
    "eval_local_recursive_fibonacci",
    "eval_generic_for_ipairs",
    "eval_generic_for_pairs",
    "eval_nested_for_loops",
    "eval_for_loop_with_step",
    "eval_for_loop_negative_step",
    "eval_while_with_break",
    "eval_for_with_break",
    "eval_string_gmatch_basic",
    "eval_table_sort_basic",
    "eval_table_sort_custom_comparator",
    "eval_multiple_return_values_first",
    "eval_redis_call_with_keys_argv",
    "eval_string_format_basic",
    "eval_math_functions",
];

struct VendoredRedisOracle {
    child: Child,
    port: u16,
}

impl VendoredRedisOracle {
    fn start(cfg: &HarnessConfig) -> Self {
        let server_path = cfg.oracle_root.join("src/redis-server");
        assert!(
            server_path.exists(),
            "vendored redis-server missing at {}",
            server_path.display()
        );

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for vendored redis");
        let port = listener
            .local_addr()
            .expect("ephemeral port address")
            .port();
        drop(listener);

        let child = Command::new(&server_path)
            .args([
                "--save",
                "",
                "--appendonly",
                "no",
                "--bind",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vendored redis-server");

        assert!(
            wait_for_redis_ready(port),
            "vendored redis-server did not become ready on 127.0.0.1:{port}"
        );

        Self { child, port }
    }

    fn start_with_config_file(cfg: &HarnessConfig) -> Self {
        let server_path = cfg.oracle_root.join("src/redis-server");
        assert!(
            server_path.exists(),
            "vendored redis-server missing at {}",
            server_path.display()
        );

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port for vendored redis");
        let port = listener
            .local_addr()
            .expect("ephemeral port address")
            .port();
        drop(listener);

        let config_path = write_vendored_redis_config(port);
        let child = Command::new(&server_path)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn vendored redis-server from config file");

        assert!(
            wait_for_redis_ready(port),
            "vendored redis-server did not become ready on 127.0.0.1:{port}"
        );

        Self { child, port }
    }
}

impl Drop for VendoredRedisOracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for_redis_ready(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            if stream.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                let mut response = [0_u8; 16];
                if let Ok(bytes_read) = stream.read(&mut response)
                    && &response[..bytes_read] == b"+PONG\r\n"
                {
                    let _ = stream.shutdown(Shutdown::Both);
                    return true;
                }
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
        sleep(Duration::from_millis(25));
    }
    false
}

fn write_vendored_redis_config(port: u16) -> PathBuf {
    let timestamp_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    let temp_root = std::env::temp_dir();
    let config_path = temp_root.join(format!(
        "fr_conformance_vendored_redis_{}_{}_{}.conf",
        std::process::id(),
        port,
        timestamp_nanos
    ));
    let config = format!(
        "bind 127.0.0.1\nport {port}\nsave \"\"\nappendonly no\ndir {}\n",
        temp_root.display()
    );
    fs::write(&config_path, config).expect("write vendored redis config");
    config_path
}

fn command_frame(argv: &[&str]) -> RespFrame {
    RespFrame::Array(Some(
        argv.iter()
            .map(|arg| RespFrame::BulkString(Some(arg.as_bytes().to_vec())))
            .collect(),
    ))
}

fn send_frame_and_read(stream: &mut TcpStream, frame: &RespFrame) -> RespFrame {
    stream
        .write_all(&frame.to_bytes())
        .expect("write RESP command to vendored redis");
    stream
        .flush()
        .expect("flush RESP command to vendored redis");
    read_frame_from_stream(stream)
}

fn read_frame_from_stream(stream: &mut TcpStream) -> RespFrame {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        let n = stream.read(&mut chunk).expect("read RESP reply");
        assert!(n > 0, "vendored redis closed connection before replying");
        buf.extend_from_slice(&chunk[..n]);
        match parse_frame(&buf) {
            Ok(parsed) => return parsed.frame,
            Err(fr_protocol::RespParseError::Incomplete) => {}
            Err(err) => panic!("vendored redis emitted invalid RESP: {err}"),
        }
    }
}

fn acl_log_field<'a>(entry: &'a [RespFrame], key: &str) -> &'a RespFrame {
    entry
        .chunks_exact(2)
        .find_map(|pair| match pair {
            [RespFrame::BulkString(Some(field_name)), value]
                if field_name.eq_ignore_ascii_case(key.as_bytes()) =>
            {
                Some(value)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("ACL LOG entry missing field '{key}'"))
}

fn bulk_text(frame: &RespFrame) -> String {
    match frame {
        RespFrame::BulkString(Some(bytes)) => String::from_utf8_lossy(bytes).to_string(),
        other => panic!("expected bulk string, got {other:?}"),
    }
}

fn int_value(frame: &RespFrame) -> i64 {
    match frame {
        RespFrame::Integer(value) => *value,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn assert_acl_log_failed_auth_shape(reply: &RespFrame) {
    let entries = match reply {
        RespFrame::Array(Some(entries)) => entries,
        other => panic!("expected ACL LOG array reply, got {other:?}"),
    };
    let expected_usernames = ["disabled_user", "nobody", "testuser"];
    assert_eq!(entries.len(), expected_usernames.len());
    let mut expected_entry_id = expected_usernames.len() as i64 - 1;
    for (entry, username) in entries.iter().zip(expected_usernames) {
        let fields = match entry {
            RespFrame::Array(Some(fields)) => fields,
            other => panic!("expected ACL LOG entry array, got {other:?}"),
        };
        assert_eq!(int_value(acl_log_field(fields, "count")), 1);
        assert_eq!(bulk_text(acl_log_field(fields, "reason")), "auth");
        assert_eq!(bulk_text(acl_log_field(fields, "context")), "toplevel");
        assert_eq!(bulk_text(acl_log_field(fields, "object")), "AUTH");
        assert_eq!(bulk_text(acl_log_field(fields, "username")), username);
        let age_seconds = bulk_text(acl_log_field(fields, "age-seconds"));
        assert!(
            age_seconds.parse::<f64>().is_ok(),
            "expected numeric age-seconds, got {age_seconds}"
        );
        let client_info = bulk_text(acl_log_field(fields, "client-info"));
        assert!(client_info.contains("cmd=auth"));
        assert!(client_info.contains("user=default"));
        assert!(client_info.contains("resp=2"));
        assert_eq!(
            int_value(acl_log_field(fields, "entry-id")),
            expected_entry_id
        );
        expected_entry_id -= 1;
        let created = int_value(acl_log_field(fields, "timestamp-created"));
        let updated = int_value(acl_log_field(fields, "timestamp-last-updated"));
        assert!(created >= 0);
        assert!(updated >= created);
    }
}

fn dynamic_replication_metadata_case(name: &str) -> bool {
    matches!(
        name,
        "psync_returns_fullresync"
            | "psync_with_replid_returns_fullresync"
            | "psync_with_full_replid"
            | "psync_with_zero_offset"
            | "psync_case_insensitive"
            | "psync_wrong_arity_too_many"
            | "role_reports_master_after_promotion"
            | "role_returns_master"
            | "role_case_insensitive"
            | "role_still_master_after_operations"
    )
}

fn parse_fullresync_reply(frame: &RespFrame) -> (String, i64) {
    let reply = match frame {
        RespFrame::SimpleString(reply) => reply,
        other => panic!("expected FULLRESYNC simple string, got {other:?}"),
    };
    let parts = reply.split_whitespace().collect::<Vec<_>>();
    assert_eq!(
        parts.len(),
        3,
        "expected FULLRESYNC <replid> <offset>, got {reply}"
    );
    assert_eq!(
        parts[0], "FULLRESYNC",
        "expected FULLRESYNC reply, got {reply}"
    );
    assert_eq!(parts[1].len(), 40, "expected 40-char replid, got {reply}");
    assert!(
        parts[1].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected hex replid, got {reply}"
    );
    let offset = parts[2]
        .parse::<i64>()
        .unwrap_or_else(|err| panic!("expected integer FULLRESYNC offset in {reply}: {err}"));
    assert!(
        offset >= 0,
        "expected nonnegative FULLRESYNC offset, got {reply}"
    );
    (parts[1].to_string(), offset)
}

fn parse_master_role_reply(frame: &RespFrame) -> i64 {
    let items = match frame {
        RespFrame::Array(Some(items)) => items,
        other => panic!("expected ROLE array reply, got {other:?}"),
    };
    assert_eq!(items.len(), 3, "expected ROLE master triple, got {items:?}");
    assert_eq!(bulk_text(&items[0]), "master");
    let offset = int_value(&items[1]);
    assert!(
        offset >= 0,
        "expected nonnegative ROLE offset, got {items:?}"
    );
    match &items[2] {
        RespFrame::Array(Some(replicas)) => assert!(replicas.is_empty(), "expected no replicas"),
        other => panic!("expected empty replica array in ROLE reply, got {other:?}"),
    }
    offset
}

fn assert_nondecreasing(label: &str, offsets: &[i64]) {
    for pair in offsets.windows(2) {
        assert!(
            pair[0] <= pair[1],
            "{label} offsets must be nondecreasing, got {offsets:?}"
        );
    }
}

fn assert_replication_dynamic_metadata_matches_contract(failures: &[&CaseOutcome]) {
    let mut live_replids = Vec::new();
    let mut runtime_replids = Vec::new();
    let mut live_offsets = Vec::new();
    let mut runtime_offsets = Vec::new();

    for failure in failures {
        match failure.name.as_str() {
            "psync_returns_fullresync"
            | "psync_with_replid_returns_fullresync"
            | "psync_with_full_replid"
            | "psync_with_zero_offset"
            | "psync_case_insensitive"
            | "psync_wrong_arity_too_many" => {
                let (live_replid, live_offset) = parse_fullresync_reply(&failure.expected);
                let (runtime_replid, runtime_offset) = parse_fullresync_reply(&failure.actual);
                live_replids.push(live_replid);
                runtime_replids.push(runtime_replid);
                live_offsets.push(live_offset);
                runtime_offsets.push(runtime_offset);
            }
            "role_reports_master_after_promotion"
            | "role_returns_master"
            | "role_case_insensitive"
            | "role_still_master_after_operations" => {
                live_offsets.push(parse_master_role_reply(&failure.expected));
                runtime_offsets.push(parse_master_role_reply(&failure.actual));
            }
            other => panic!("unexpected dynamic replication case {other}"),
        }
    }

    if let Some(first) = live_replids.first() {
        assert!(
            live_replids.iter().all(|replid| replid == first),
            "live Redis replids changed within one smoke run: {live_replids:?}"
        );
    }
    if let Some(first) = runtime_replids.first() {
        assert!(
            runtime_replids.iter().all(|replid| replid == first),
            "runtime replids changed within one smoke run: {runtime_replids:?}"
        );
    }

    assert_nondecreasing("live Redis replication", &live_offsets);
    assert_nondecreasing("runtime replication", &runtime_offsets);
}

#[test]
fn smoke_report_is_stable() {
    let cfg = HarnessConfig::default_paths();
    let report = run_smoke(&cfg);
    assert_eq!(report.suite, "smoke");
    assert!(report.fixture_count >= 1);
    assert!(report.oracle_present);

    let fixture_path = cfg.fixture_root.join("core_strings.json");
    assert!(Path::new(&fixture_path).exists());

    let diff = run_fixture(&cfg, "core_strings.json").expect("fixture runs");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());

    let errors = run_fixture(&cfg, "core_errors.json").expect("error fixture");
    assert_eq!(errors.total, errors.passed);
    assert!(errors.failed.is_empty());

    let dispatch =
        run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet-003 dispatch fixture");
    assert_eq!(dispatch.total, dispatch.passed);
    assert!(dispatch.failed.is_empty());

    let auth_acl =
        run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet-004 auth/acl fixture");
    assert_eq!(auth_acl.total, auth_acl.passed);
    assert!(auth_acl.failed.is_empty());

    let repl_handshake =
        run_replication_handshake_fixture(&cfg, "fr_p2c_006_replication_handshake.json")
            .expect("replication handshake fixture");
    assert_eq!(repl_handshake.total, repl_handshake.passed);
    assert!(repl_handshake.failed.is_empty());

    let protocol = run_protocol_fixture(&cfg, "protocol_negative.json").expect("protocol fixture");
    assert_eq!(protocol.total, protocol.passed);
    assert!(protocol.failed.is_empty());

    let replay = run_replay_fixture(&cfg, "persist_replay.json").expect("replay fixture");
    assert_eq!(replay.total, replay.passed);
    assert!(replay.failed.is_empty());
}

#[test]
fn fr_p2c_001_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_001_eventloop_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_002_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_protocol_fixture(&cfg, "protocol_negative.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_003_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_003_dispatch_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_004_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_004_acl_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_005_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_replay_fixture(&cfg, "persist_replay.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_006_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_006_replication_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_006_replication_handshake_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_replication_handshake_fixture(&cfg, "fr_p2c_006_replication_handshake.json")
        .expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_007_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_007_cluster_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_008_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_008_expire_evict_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn fr_p2c_009_e2e_contract_smoke() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "fr_p2c_009_tls_config_journey.json").expect("packet fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_hash_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hash.json").expect("hash fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_list_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_list.json").expect("list fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_set_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_set.json").expect("set fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_zset_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_zset.json").expect("zset fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_geo_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_geo.json").expect("geo fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_stream_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_stream.json").expect("stream fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_generic_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_generic.json").expect("generic fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_acl_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_acl.json").expect("acl fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_acl_log_failed_auth_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let mut runtime = Runtime::default_strict();
    let mut live = TcpStream::connect(("127.0.0.1", oracle_server.port))
        .expect("connect to vendored redis for ACL LOG smoke");
    live.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis read timeout");
    live.set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set vendored redis write timeout");

    let sequence = [
        (40_u64, ["ACL", "SETUSER", "testuser", "on", ">secret123"]),
        (43_u64, ["AUTH", "testuser", "wrongpass", "", ""]),
        (44_u64, ["AUTH", "nobody", "pass", "", ""]),
        (
            50_u64,
            ["ACL", "SETUSER", "disabled_user", "off", ">pass456"],
        ),
        (51_u64, ["AUTH", "disabled_user", "pass456", "", ""]),
    ];

    for (now_ms, argv) in sequence {
        let argv = argv
            .into_iter()
            .filter(|arg| !arg.is_empty())
            .collect::<Vec<_>>();
        let frame = command_frame(&argv);
        let _runtime_reply = runtime.execute_frame(frame.clone(), now_ms);
        let _live_reply = send_frame_and_read(&mut live, &frame);
    }

    let runtime_acl_log = runtime.execute_frame(command_frame(&["ACL", "LOG"]), 100);
    let live_acl_log = send_frame_and_read(&mut live, &command_frame(&["ACL", "LOG"]));

    assert_acl_log_failed_auth_shape(&runtime_acl_log);
    assert_acl_log_failed_auth_shape(&live_acl_log);
}

#[test]
fn core_hyperloglog_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_hyperloglog.json").expect("hyperloglog fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_bitmap_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_bitmap.json").expect("bitmap fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_transaction_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_transaction.json").expect("transaction fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_connection_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_connection.json").expect("connection fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_expiry_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_expiry.json").expect("expiry fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_client_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_client.json").expect("client fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_server_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_server.json").expect("server fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_scripting_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_scripting.json").expect("scripting fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_pubsub_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_pubsub.json").expect("pubsub fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_replication_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_replication.json").expect("replication fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_sort_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_sort.json").expect("sort fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_scan_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_scan.json").expect("scan fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_config_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_config.json").expect("config fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_cluster_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_cluster.json").expect("cluster fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_copy_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_copy.json").expect("copy fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_function_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_function.json").expect("function fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_wait_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_wait.json").expect("wait fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_blocking_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_blocking.json").expect("blocking fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_strings_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_strings.json").expect("strings fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_errors_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_errors.json").expect("errors fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_object_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_object.json").expect("object fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_pfdebug_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_pfdebug.json").expect("pfdebug fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_migrate_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_migrate.json").expect("migrate fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_module_sentinel_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_module_sentinel.json").expect("module/sentinel fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_module_sentinel_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff(&cfg, "core_module_sentinel.json", &oracle)
        .expect("module/sentinel live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_pubsub_multi_client_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_multi_client_diff(&cfg, "core_pubsub_multi_client.json", &oracle)
        .expect("pubsub multi-client live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_blocking_multi_client_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_multi_client_diff(&cfg, "core_blocking_multi_client.json", &oracle)
        .expect("blocking multi-client live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_replication_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff(&cfg, "core_replication.json", &oracle).expect("replication live diff");
    let dynamic_failures = report
        .failed
        .iter()
        .filter(|failure| dynamic_replication_metadata_case(&failure.name))
        .collect::<Vec<_>>();
    let unexpected_failures = report
        .failed
        .iter()
        .filter(|failure| !dynamic_replication_metadata_case(&failure.name))
        .collect::<Vec<_>>();

    assert!(
        unexpected_failures.is_empty(),
        "unexpected replication mismatches: {:?}",
        unexpected_failures
    );
    assert_replication_dynamic_metadata_matches_contract(&dynamic_failures);
}

#[test]
fn core_connection_cluster_mode_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_connection.json",
        &[
            "readonly_reports_cluster_disabled",
            "readwrite_reports_cluster_disabled",
            "readonly_wrong_arity",
            "readwrite_wrong_arity",
        ],
        &oracle,
    )
    .expect("connection cluster-mode live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_cluster_disabled_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_cluster.json",
        &[
            "cluster_info",
            "cluster_myid",
            "cluster_getkeysinslot_with_key",
            "cluster_countkeysinslot_with_key",
            "cluster_help",
            "cluster_unknown_subcommand",
            "cluster_keyslot_wrong_arity",
            "cluster_reset_hard",
        ],
        &oracle,
    )
    .expect("cluster disabled-surface live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_config_rewrite_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start_with_config_file(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_config.json",
        &["config_rewrite", "config_rewrite_ok"],
        &oracle,
    )
    .expect("config rewrite live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_server_config_rewrite_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start_with_config_file(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_server.json",
        &["config_rewrite_returns_ok"],
        &oracle,
    )
    .expect("server config rewrite live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn fr_p2c_007_cluster_disabled_surface_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "fr_p2c_007_cluster_journey.json",
        &[
            "cluster_wrong_arity_is_rejected",
            "cluster_unknown_subcommand_is_rejected",
            "cluster_info_is_reachable_from_runtime",
            "cluster_keyslot_is_reachable_from_runtime",
            "asking_after_readonly_is_ok",
            "readonly_wrong_arity_is_rejected",
        ],
        &oracle,
    )
    .expect("packet-007 cluster disabled-surface live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_debug_conformance() {
    let cfg = HarnessConfig::default_paths();
    let diff = run_fixture(&cfg, "core_debug.json").expect("debug fixture");
    assert_eq!(diff.total, diff.passed, "failed: {:?}", diff.failed);
    assert!(diff.failed.is_empty());
}

#[test]
fn core_transaction_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report =
        run_live_redis_diff(&cfg, "core_transaction.json", &oracle).expect("transaction live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_scripting_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_scripting.json",
        CORE_SCRIPTING_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("scripting live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}

#[test]
fn core_stream_live_redis_matches_runtime() {
    let cfg = HarnessConfig::default_paths();
    let oracle_server = VendoredRedisOracle::start(&cfg);
    let oracle = LiveOracleConfig {
        host: "127.0.0.1".to_string(),
        port: oracle_server.port,
        ..LiveOracleConfig::default()
    };
    let report = run_live_redis_diff_for_cases(
        &cfg,
        "core_stream.json",
        CORE_STREAM_LIVE_STABLE_CASES,
        &oracle,
    )
    .expect("stream live diff");
    assert_eq!(
        report.total, report.passed,
        "mismatches: {:?}",
        report.failed
    );
    assert!(report.failed.is_empty());
}
