//! Integration tests for AOF persistence round-trip: execute commands, save AOF,
//! create fresh runtime, load AOF, verify data survived.

use std::path::{Path, PathBuf};

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

fn temp_aof_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("frankenredis_test");
    std::fs::create_dir_all(&dir).ok();
    dir.join(format!("{name}.aof"))
}

fn setup_runtime_with_aof(name: &str) -> (Runtime, PathBuf) {
    let path = temp_aof_path(name);
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.clone());
    (rt, path)
}

fn load_fresh_runtime(path: &Path) -> Runtime {
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.to_path_buf());
    let count = rt.load_aof(0);
    assert!(count.is_ok(), "AOF load should succeed");
    rt
}

#[test]
fn aof_roundtrip_strings() {
    let (mut rt, path) = setup_runtime_with_aof("strings");

    rt.execute_frame(command(&[b"SET", b"key1", b"hello"]), 0);
    rt.execute_frame(command(&[b"SET", b"key2", b"world"]), 0);
    rt.execute_frame(command(&[b"MSET", b"k3", b"v3", b"k4", b"v4"]), 0);
    rt.execute_frame(command(&[b"INCR", b"counter"]), 0);
    rt.execute_frame(command(&[b"INCR", b"counter"]), 0);
    rt.execute_frame(command(&[b"APPEND", b"key1", b" world"]), 0);

    // Save
    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    // Load into fresh runtime
    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"key1"]), 2),
        RespFrame::BulkString(Some(b"hello world".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"key2"]), 2),
        RespFrame::BulkString(Some(b"world".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"k3"]), 2),
        RespFrame::BulkString(Some(b"v3".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"counter"]), 2),
        RespFrame::BulkString(Some(b"2".to_vec()))
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_list() {
    let (mut rt, path) = setup_runtime_with_aof("list");

    rt.execute_frame(command(&[b"RPUSH", b"mylist", b"a", b"b", b"c"]), 0);
    rt.execute_frame(command(&[b"LPUSH", b"mylist", b"z"]), 0);

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"LLEN", b"mylist"]), 2),
        RespFrame::Integer(4)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"LRANGE", b"mylist", b"0", b"-1"]), 2),
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"z".to_vec())),
            RespFrame::BulkString(Some(b"a".to_vec())),
            RespFrame::BulkString(Some(b"b".to_vec())),
            RespFrame::BulkString(Some(b"c".to_vec())),
        ]))
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_hash() {
    let (mut rt, path) = setup_runtime_with_aof("hash");

    rt.execute_frame(
        command(&[b"HSET", b"myhash", b"f1", b"v1", b"f2", b"v2"]),
        0,
    );
    rt.execute_frame(command(&[b"HINCRBY", b"myhash", b"count", b"5"]), 0);

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"HGET", b"myhash", b"f1"]), 2),
        RespFrame::BulkString(Some(b"v1".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"HGET", b"myhash", b"count"]), 2),
        RespFrame::BulkString(Some(b"5".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"HLEN", b"myhash"]), 2),
        RespFrame::Integer(3)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_set_and_sorted_set() {
    let (mut rt, path) = setup_runtime_with_aof("sets");

    rt.execute_frame(command(&[b"SADD", b"myset", b"a", b"b", b"c"]), 0);
    rt.execute_frame(
        command(&[b"ZADD", b"myzset", b"1.5", b"x", b"2.5", b"y"]),
        0,
    );

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"SCARD", b"myset"]), 2),
        RespFrame::Integer(3)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"SISMEMBER", b"myset", b"b"]), 2),
        RespFrame::Integer(1)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"ZCARD", b"myzset"]), 2),
        RespFrame::Integer(2)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"ZSCORE", b"myzset", b"x"]), 2),
        RespFrame::BulkString(Some(b"1.5".to_vec()))
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_stream() {
    let (mut rt, path) = setup_runtime_with_aof("stream");

    rt.execute_frame(
        command(&[b"XADD", b"mystream", b"1-0", b"name", b"Alice"]),
        0,
    );
    rt.execute_frame(command(&[b"XADD", b"mystream", b"2-0", b"name", b"Bob"]), 0);

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"XLEN", b"mystream"]), 2),
        RespFrame::Integer(2)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_delete_and_overwrite() {
    let (mut rt, path) = setup_runtime_with_aof("delete");

    rt.execute_frame(command(&[b"SET", b"temp", b"will_be_deleted"]), 0);
    rt.execute_frame(command(&[b"SET", b"kept", b"original"]), 0);
    rt.execute_frame(command(&[b"DEL", b"temp"]), 0);
    rt.execute_frame(command(&[b"SET", b"kept", b"overwritten"]), 0);

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"EXISTS", b"temp"]), 2),
        RespFrame::Integer(0)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"kept"]), 2),
        RespFrame::BulkString(Some(b"overwritten".to_vec()))
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_roundtrip_mixed_types() {
    let (mut rt, path) = setup_runtime_with_aof("mixed");

    // Create keys of all major types
    rt.execute_frame(command(&[b"SET", b"str", b"hello"]), 0);
    rt.execute_frame(command(&[b"RPUSH", b"list", b"1", b"2", b"3"]), 0);
    rt.execute_frame(command(&[b"SADD", b"set", b"a", b"b"]), 0);
    rt.execute_frame(command(&[b"HSET", b"hash", b"k", b"v"]), 0);
    rt.execute_frame(command(&[b"ZADD", b"zset", b"1", b"m"]), 0);

    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    let mut rt2 = load_fresh_runtime(&path);

    // Verify all types survived
    assert_eq!(
        rt2.execute_frame(command(&[b"TYPE", b"str"]), 2),
        RespFrame::SimpleString("string".to_string())
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"TYPE", b"list"]), 2),
        RespFrame::SimpleString("list".to_string())
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"TYPE", b"set"]), 2),
        RespFrame::SimpleString("set".to_string())
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"TYPE", b"hash"]), 2),
        RespFrame::SimpleString("hash".to_string())
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"TYPE", b"zset"]), 2),
        RespFrame::SimpleString("zset".to_string())
    );

    // Verify values
    assert_eq!(
        rt2.execute_frame(command(&[b"GET", b"str"]), 2),
        RespFrame::BulkString(Some(b"hello".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"LLEN", b"list"]), 2),
        RespFrame::Integer(3)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"SCARD", b"set"]), 2),
        RespFrame::Integer(2)
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn aof_empty_save_and_load() {
    let (mut rt, path) = setup_runtime_with_aof("empty");

    // Save empty database
    let save = rt.execute_frame(command(&[b"SAVE"]), 1);
    assert_eq!(save, RespFrame::SimpleString("OK".to_string()));

    // Load should succeed with 0 records
    let mut rt2 = Runtime::default_strict();
    rt2.set_aof_path(path.clone());
    let count = rt2.load_aof(0).unwrap();
    assert_eq!(count, 0);

    // Verify store is empty
    assert_eq!(
        rt2.execute_frame(command(&[b"DBSIZE"]), 1),
        RespFrame::Integer(0)
    );

    std::fs::remove_file(&path).ok();
}

// ── Hash field TTL (Redis 7.4 HEXPIRE family) (br-frankenredis-4bao) ─

/// Issue HEXPIRE / HPEXPIRE / HPERSIST on a live runtime, persist via
/// SAVE, boot a fresh runtime off the same AOF file, and verify every
/// per-field TTL state survived the replay.
#[test]
fn aof_roundtrip_hexpire_family_reconstructs_per_field_ttls() {
    let (mut rt, path) = setup_runtime_with_aof("hexpire_family");

    rt.execute_frame(
        command(&[
            b"HSET", b"h", b"f_s", b"v_s", b"f_ms", b"v_ms", b"f_at", b"v_at", b"f_pers", b"v_pers",
        ]),
        0,
    );
    rt.execute_frame(
        command(&[b"HEXPIRE", b"h", b"600", b"FIELDS", b"1", b"f_s"]),
        0,
    );
    rt.execute_frame(
        command(&[b"HPEXPIRE", b"h", b"30000", b"FIELDS", b"1", b"f_ms"]),
        0,
    );
    rt.execute_frame(
        command(&[
            b"HEXPIREAT",
            b"h",
            b"9999999999",
            b"FIELDS",
            b"1",
            b"f_at",
        ]),
        0,
    );
    rt.execute_frame(
        command(&[b"HPEXPIRE", b"h", b"5000", b"FIELDS", b"1", b"f_pers"]),
        0,
    );
    rt.execute_frame(
        command(&[b"HPERSIST", b"h", b"FIELDS", b"1", b"f_pers"]),
        0,
    );

    assert_eq!(
        rt.execute_frame(command(&[b"SAVE"]), 1),
        RespFrame::SimpleString("OK".to_string())
    );
    let mut rt2 = load_fresh_runtime(&path);

    // Use now_ms=0 for all reads so the source-runtime remaining-ms is
    // preserved byte-for-byte in the assertions (no drift from saturating
    // arithmetic in the time-remaining calcs).
    assert_eq!(
        rt2.execute_frame(command(&[b"HLEN", b"h"]), 0),
        RespFrame::Integer(4)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"HGET", b"h", b"f_s"]), 0),
        RespFrame::BulkString(Some(b"v_s".to_vec()))
    );
    assert_eq!(
        rt2.execute_frame(
            command(&[b"HTTL", b"h", b"FIELDS", b"1", b"f_s"]),
            0,
        ),
        RespFrame::Array(Some(vec![RespFrame::Integer(600)]))
    );
    assert_eq!(
        rt2.execute_frame(
            command(&[b"HPTTL", b"h", b"FIELDS", b"1", b"f_ms"]),
            0,
        ),
        RespFrame::Array(Some(vec![RespFrame::Integer(30_000)]))
    );
    assert_eq!(
        rt2.execute_frame(
            command(&[b"HEXPIRETIME", b"h", b"FIELDS", b"1", b"f_at"]),
            0,
        ),
        RespFrame::Array(Some(vec![RespFrame::Integer(9_999_999_999)]))
    );
    assert_eq!(
        rt2.execute_frame(
            command(&[b"HTTL", b"h", b"FIELDS", b"1", b"f_pers"]),
            0,
        ),
        RespFrame::Array(Some(vec![RespFrame::Integer(-1)]))
    );
    assert_eq!(
        rt2.execute_frame(
            command(&[b"HTTL", b"h", b"FIELDS", b"1", b"nope"]),
            0,
        ),
        RespFrame::Array(Some(vec![RespFrame::Integer(-2)]))
    );

    std::fs::remove_file(&path).ok();
}

/// Regression guard: HPEXPIREAT with a past deadline reaps the field
/// on the source runtime; the AOF should carry the HPEXPIREAT record
/// and the replay should re-reap the field so it's invisible on the
/// fresh runtime as well.
#[test]
fn aof_roundtrip_hpexpireat_past_deadline_replays_reaped_state() {
    let (mut rt, path) = setup_runtime_with_aof("hpexpireat_past");
    rt.execute_frame(
        command(&[b"HSET", b"h", b"alive", b"a", b"doomed", b"d"]),
        0,
    );
    rt.execute_frame(
        command(&[b"HPEXPIREAT", b"h", b"1", b"FIELDS", b"1", b"doomed"]),
        100,
    );
    assert_eq!(
        rt.execute_frame(command(&[b"HGET", b"h", b"doomed"]), 200),
        RespFrame::BulkString(None)
    );

    assert_eq!(
        rt.execute_frame(command(&[b"SAVE"]), 201),
        RespFrame::SimpleString("OK".to_string())
    );
    let mut rt2 = load_fresh_runtime(&path);

    assert_eq!(
        rt2.execute_frame(command(&[b"HLEN", b"h"]), 300),
        RespFrame::Integer(1)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"HGET", b"h", b"doomed"]), 301),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        rt2.execute_frame(command(&[b"HGET", b"h", b"alive"]), 301),
        RespFrame::BulkString(Some(b"a".to_vec()))
    );

    std::fs::remove_file(&path).ok();
}
