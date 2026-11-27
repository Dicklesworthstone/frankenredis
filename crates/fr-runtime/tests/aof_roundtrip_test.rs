//! Integration tests for AOF persistence round-trip: execute commands, save AOF,
//! create fresh runtime, load AOF, verify data survived.

use std::path::{Path, PathBuf};

use fr_persist::{AofRecord, PersistError, RdbEntry, RdbValue};
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

fn expiry_aof_path(name: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    temp_aof_path(&format!("expiry_{name}_{}_{nonce}", std::process::id()))
}

fn aof_record(parts: &[&[u8]]) -> AofRecord {
    AofRecord::from_resp_frame(&command(parts)).expect("valid command record")
}

#[test]
fn resp_aof_base_preserves_data_functions_and_incremental_database_context() {
    let dir = expiry_aof_path("resp_base_roundtrip").with_extension("dir");
    std::fs::create_dir_all(&dir).expect("create AOF directory");
    let path = dir.join("appendonly.aof");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.clone());
    assert_eq!(
        rt.execute_frame(
            command(&[
                b"CONFIG",
                b"SET",
                b"aof-use-rdb-preamble",
                b"no",
                b"aof-disable-auto-gc",
                b"yes",
            ]),
            1000,
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    let library = b"#!lua name=aofbase\nredis.register_function('aofbase_get', function(keys, args) return redis.call('GET', keys[1]) end)";
    let setup: &[&[&[u8]]] = &[
        &[b"FUNCTION", b"LOAD", library],
        &[b"SET", b"counter", b"41"],
        &[b"SET", b"binary\0key", b"value\r\n\0\xff"],
        &[b"SET", b"ttl", b"retained", b"PXAT", b"90000"],
        &[b"RPUSH", b"list", b"first", b"second"],
        &[b"HSET", b"hash", b"field", b"value"],
        &[b"SADD", b"set", b"one", b"two"],
        &[b"ZADD", b"zset", b"1.25", b"one", b"2.5", b"two"],
        &[b"PFADD", b"hll", b"one", b"two"],
        &[b"XADD", b"stream", b"1-0", b"field", b"first"],
        &[b"XADD", b"stream", b"2-0", b"field", b"second"],
        &[b"XGROUP", b"CREATE", b"stream", b"group", b"0-0"],
        &[
            b"XREADGROUP",
            b"GROUP",
            b"group",
            b"consumer",
            b"COUNT",
            b"1",
            b"STREAMS",
            b"stream",
            b">",
        ],
        &[
            b"XGROUP",
            b"CREATE",
            b"empty-stream",
            b"group",
            b"0-0",
            b"MKSTREAM",
        ],
        &[b"SELECT", b"3"],
        &[b"SET", b"counter", b"other-database"],
        &[b"RPUSH", b"list", b"db-three"],
    ];
    for argv in setup {
        let reply = rt.execute_frame(command(argv), 1000);
        assert!(!matches!(reply, RespFrame::Error(_)), "{argv:?}: {reply:?}");
    }
    assert_eq!(
        rt.execute_frame(command(&[b"BGREWRITEAOF"]), 1000),
        RespFrame::SimpleString("Background append only file rewriting started".to_string())
    );
    let manifest = fr_persist::read_aof_manifest_file(&dir.join("appendonly.aof.manifest"))
        .expect("read RESP base manifest");
    let base = manifest.base.expect("published base");
    assert!(base.file_name.ends_with(".base.aof"));
    let bytes = std::fs::read(dir.join(&base.file_name)).expect("read RESP base");
    let records = fr_persist::decode_aof_stream(&bytes).expect("base is RESP commands");
    for expected in [b"FUNCTION".as_slice(), b"XCLAIM", b"PEXPIREAT"] {
        assert!(
            records
                .iter()
                .any(|record| record.argv.first().map(Vec::as_slice) == Some(expected)),
            "missing {expected:?} in RESP base"
        );
    }

    // The base ends in database 3. The first incremental write must explicitly
    // select database 0 even if that was the old append cursor's database.
    assert_eq!(
        rt.execute_frame(command(&[b"SELECT", b"0"]), 1100),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        rt.execute_frame(command(&[b"INCR", b"counter"]), 1100),
        RespFrame::Integer(42)
    );
    rt.flush_aof_to_disk(1100);

    let mut restored = Runtime::default_strict();
    restored.set_aof_path(path);
    restored.load_aof(2000).expect("reload RESP base and tail");
    let checks: &[(u32, &[&[u8]])] = &[
        (0, &[b"GET", b"counter"]),
        (0, &[b"GET", b"binary\0key"]),
        (0, &[b"PEXPIRETIME", b"ttl"]),
        (0, &[b"LRANGE", b"list", b"0", b"-1"]),
        (0, &[b"HGET", b"hash", b"field"]),
        (0, &[b"SCARD", b"set"]),
        (0, &[b"SISMEMBER", b"set", b"two"]),
        (0, &[b"ZRANGE", b"zset", b"0", b"-1", b"WITHSCORES"]),
        (0, &[b"PFCOUNT", b"hll"]),
        (0, &[b"XRANGE", b"stream", b"-", b"+"]),
        (0, &[b"XPENDING", b"stream", b"group"]),
        (0, &[b"XINFO", b"GROUPS", b"stream"]),
        (0, &[b"XINFO", b"STREAM", b"empty-stream"]),
        (0, &[b"FCALL", b"aofbase_get", b"1", b"counter"]),
        (3, &[b"GET", b"counter"]),
        (3, &[b"LRANGE", b"list", b"0", b"-1"]),
    ];
    for &(db, argv) in checks {
        let db = db.to_string();
        for runtime in [&mut rt, &mut restored] {
            assert_eq!(
                runtime.execute_frame(command(&[b"SELECT", db.as_bytes()]), 2000),
                RespFrame::SimpleString("OK".to_string())
            );
        }
        let expected = rt.execute_frame(command(argv), 2000);
        assert!(
            !matches!(expected, RespFrame::Error(_)),
            "db {db}, {argv:?}: {expected:?}"
        );
        assert_eq!(
            restored.execute_frame(command(argv), 2000),
            expected,
            "db {db}, {argv:?}"
        );
    }
    restored.execute_frame(command(&[b"SELECT", b"0"]), 2000);
    assert_eq!(
        restored.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
}

#[test]
fn aof_base_format_can_change_between_generations_without_losing_tail_writes() {
    let dir = expiry_aof_path("base_format_switch").with_extension("dir");
    std::fs::create_dir_all(&dir).expect("create AOF directory");
    let path = dir.join("appendonly.aof");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.clone());
    assert_eq!(
        rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"aof-disable-auto-gc", b"yes"]),
            1000,
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    rt.execute_frame(command(&[b"SET", b"counter", b"41"]), 1000);
    let mut previous_seq = 0;
    for (step, preamble) in [b"no".as_slice(), b"yes", b"no"].into_iter().enumerate() {
        assert_eq!(
            rt.execute_frame(
                command(&[b"CONFIG", b"SET", b"aof-use-rdb-preamble", preamble]),
                1000,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        assert_eq!(
            rt.execute_frame(command(&[b"BGREWRITEAOF"]), 1000),
            RespFrame::SimpleString("Background append only file rewriting started".to_string())
        );
        let manifest = fr_persist::read_aof_manifest_file(&dir.join("appendonly.aof.manifest"))
            .expect("read format-switch manifest");
        let base = manifest.base.expect("published base");
        assert!(base.file_seq > previous_seq);
        previous_seq = base.file_seq;
        let bytes = std::fs::read(dir.join(&base.file_name)).expect("read generation base");
        if preamble == b"yes" {
            assert!(base.file_name.ends_with(".base.rdb"));
            assert!(bytes.starts_with(b"REDIS"));
        } else {
            assert!(base.file_name.ends_with(".base.aof"));
            fr_persist::decode_aof_stream(&bytes).expect("RESP generation base");
        }
        let expected = 42 + i64::try_from(step).expect("small test step");
        assert_eq!(
            rt.execute_frame(command(&[b"INCR", b"counter"]), 1100),
            RespFrame::Integer(expected)
        );
        rt.flush_aof_to_disk(1100);
        let mut restored = Runtime::default_strict();
        restored.set_aof_path(path.clone());
        restored.load_aof(2000).expect("load switched generation");
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"counter"]), 2000),
            RespFrame::BulkString(Some(expected.to_string().into_bytes()))
        );
    }
}

#[test]
fn empty_resp_aof_base_is_a_valid_generation() {
    let dir = expiry_aof_path("empty_resp_base").with_extension("dir");
    std::fs::create_dir_all(&dir).expect("create AOF directory");
    let path = dir.join("appendonly.aof");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.clone());
    assert_eq!(
        rt.execute_frame(
            command(&[b"CONFIG", b"SET", b"aof-use-rdb-preamble", b"no"]),
            1000,
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    rt.flush_aof_to_disk(1000);
    let manifest = fr_persist::read_aof_manifest_file(&dir.join("appendonly.aof.manifest"))
        .expect("read empty RESP manifest");
    let base = manifest.base.expect("published empty base");
    assert!(base.file_name.ends_with(".base.aof"));
    assert!(
        std::fs::read(dir.join(base.file_name))
            .expect("read empty RESP base")
            .is_empty()
    );
    let mut restored = Runtime::default_strict();
    restored.set_aof_path(path);
    assert_eq!(restored.load_aof(2000).expect("load empty RESP base"), 0);
    assert_eq!(
        restored.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(0)
    );
}

#[test]
fn aof_replay_does_not_revive_an_elapsed_counter_as_an_immortal_one() {
    // These writes happened before 1500. Restarting at 2000 must replay the
    // original counter and its TTL, not delete it before INCR recreates it.
    let records = vec![
        aof_record(&[b"SET", b"counter", b"41"]),
        aof_record(&[b"PEXPIREAT", b"counter", b"1500"]),
        aof_record(&[b"INCR", b"counter"]),
    ];
    let path = expiry_aof_path("elapsed_counter");
    fr_persist::write_aof_file(&path, &records).expect("write history");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path);

    assert_eq!(rt.load_aof(2000), Ok(records.len()));
    assert_eq!(rt.aof_records(), records.as_slice());
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(None),
        "the original elapsed TTL must survive the replayed INCR"
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"counter"]), 2000),
        RespFrame::Integer(-2)
    );
}

#[test]
fn aof_replay_persist_and_keepttl_preserve_historical_values() {
    let records = vec![
        aof_record(&[b"SET", b"counter", b"41"]),
        aof_record(&[b"PEXPIREAT", b"counter", b"1500"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"PERSIST", b"counter"]),
        aof_record(&[b"SET", b"kept", b"old", b"PXAT", b"1500"]),
        aof_record(&[b"SET", b"kept", b"new", b"KEEPTTL"]),
        aof_record(&[b"PERSIST", b"kept"]),
        aof_record(&[b"SET", b"elapsed", b"old", b"PXAT", b"1500"]),
        aof_record(&[b"SET", b"elapsed", b"new", b"KEEPTTL"]),
    ];
    let path = expiry_aof_path("persist_keepttl");
    fr_persist::write_aof_file(&path, &records).expect("write history");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path);

    assert_eq!(rt.load_aof(2000), Ok(records.len()));
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(Some(b"42".to_vec())),
        "PERSIST must rescue the original incremented value"
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"counter"]), 2000),
        RespFrame::Integer(-1)
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"kept"]), 2000),
        RespFrame::BulkString(Some(b"new".to_vec()))
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"kept"]), 2000),
        RespFrame::Integer(-1)
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"elapsed"]), 2000),
        RespFrame::BulkString(None),
        "KEEPTTL must not silently make an elapsed key persistent"
    );
}

fn elapsed_base_and_tail() -> (Vec<RdbEntry>, Vec<AofRecord>) {
    let entries = vec![
        RdbEntry {
            db: 2,
            key: b"counter".to_vec(),
            value: RdbValue::String(b"41".to_vec()),
            expire_ms: Some(1500),
        },
        RdbEntry {
            db: 2,
            key: b"list".to_vec(),
            value: RdbValue::List(vec![b"first".to_vec()]),
            expire_ms: Some(1500),
        },
        RdbEntry {
            db: 2,
            key: b"elapsed".to_vec(),
            value: RdbValue::String(b"41".to_vec()),
            expire_ms: Some(1500),
        },
    ];
    let records = vec![
        aof_record(&[b"SELECT", b"2"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"RPUSH", b"list", b"second"]),
        aof_record(&[b"PERSIST", b"counter"]),
        aof_record(&[b"PERSIST", b"list"]),
        aof_record(&[b"INCR", b"elapsed"]),
    ];
    (entries, records)
}

fn assert_elapsed_base_was_replayed(rt: &mut Runtime) {
    assert_eq!(
        rt.execute_frame(command(&[b"SELECT", b"2"]), 2000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(Some(b"42".to_vec())),
        "the elapsed base value is needed by the incremental INCR"
    );
    assert_eq!(
        rt.execute_frame(command(&[b"LRANGE", b"list", b"0", b"-1"]), 2000),
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"first".to_vec())),
            RespFrame::BulkString(Some(b"second".to_vec())),
        ])),
        "collection reconstruction must also retain the expired base value"
    );
    for key in [b"counter".as_slice(), b"list".as_slice()] {
        assert_eq!(
            rt.execute_frame(command(&[b"PTTL", key]), 2000),
            RespFrame::Integer(-1)
        );
    }
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"elapsed"]), 2000),
        RespFrame::BulkString(None),
        "normal expiry resumes after all incremental records are applied"
    );
}

#[test]
fn aof_replay_retains_elapsed_rdb_preamble_values_for_the_tail() {
    let (entries, records) = elapsed_base_and_tail();
    let mut bytes = fr_persist::encode_rdb(&entries, &[]);
    bytes.extend(fr_persist::encode_aof_stream(&records));
    let path = expiry_aof_path("rdb_preamble");
    std::fs::write(&path, bytes).expect("write RDB preamble and RESP tail");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path);

    assert_eq!(rt.load_aof(2000), Ok(entries.len() + records.len()));
    assert_elapsed_base_was_replayed(&mut rt);
}

#[test]
fn aof_replay_retains_elapsed_multipart_base_across_incremental_files() {
    let (entries, records) = elapsed_base_and_tail();
    let dir = expiry_aof_path("multipart").with_extension("dir");
    std::fs::create_dir_all(&dir).expect("create multipart directory");
    std::fs::write(
        dir.join("appendonly.aof.1.base.rdb"),
        fr_persist::encode_rdb(&entries, &[]),
    )
    .expect("write base");
    // The first incremental mutates the elapsed base; the second persists
    // those mutations. Loading must span the entire committed generation.
    fr_persist::write_aof_file(&dir.join("appendonly.aof.1.incr.aof"), &records[..3])
        .expect("write first incremental");
    let mut second_records = vec![aof_record(&[b"SELECT", b"2"])];
    second_records.extend_from_slice(&records[3..]);
    fr_persist::write_aof_file(&dir.join("appendonly.aof.2.incr.aof"), &second_records)
        .expect("write second incremental");
    let manifest = dir.join("appendonly.aof.manifest");
    std::fs::write(
        &manifest,
        "file appendonly.aof.1.base.rdb seq 1 type b\n\
         file appendonly.aof.1.incr.aof seq 1 type i\n\
         file appendonly.aof.2.incr.aof seq 2 type i\n",
    )
    .expect("write manifest");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(manifest);

    assert_eq!(rt.load_aof(2000), Ok(entries.len() + records.len() + 1));
    assert_elapsed_base_was_replayed(&mut rt);
}

#[test]
fn aof_replay_transaction_mutations_see_the_original_elapsed_value() {
    let records = vec![
        aof_record(&[b"SELECT", b"3"]),
        aof_record(&[b"SET", b"counter", b"40", b"PXAT", b"1500"]),
        aof_record(&[b"MULTI"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"PERSIST", b"counter"]),
        aof_record(&[b"EXEC"]),
    ];
    let path = expiry_aof_path("transaction");
    fr_persist::write_aof_file(&path, &records).expect("write transaction");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path);

    assert_eq!(rt.load_aof(2000), Ok(records.len()));
    assert_eq!(
        rt.execute_frame(command(&[b"SELECT", b"3"]), 2000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"counter"]), 2000),
        RespFrame::Integer(-1)
    );
}

#[test]
fn aof_replay_public_apis_preserve_elapsed_effects_and_recaptured_history() {
    for use_stream in [false, true] {
        let records = vec![
            aof_record(&[b"SET", b"counter", b"41"]),
            aof_record(&[b"PEXPIREAT", b"counter", b"1500"]),
            aof_record(&[b"INCR", b"counter"]),
            aof_record(&[b"PERSIST", b"counter"]),
        ];
        let mut rt = Runtime::default_strict();
        rt.set_aof_path(expiry_aof_path("public_replay"));
        let replies = if use_stream {
            rt.replay_aof_stream(&fr_persist::encode_aof_stream(&records), 2000)
                .expect("decode replay stream")
        } else {
            rt.replay_aof_records(&records, 2000)
        };
        assert_eq!(
            replies,
            vec![
                RespFrame::SimpleString("OK".to_string()),
                RespFrame::Integer(1),
                RespFrame::Integer(42),
                RespFrame::Integer(1),
            ]
        );
        let captured = rt.aof_records().to_vec();
        assert!(
            captured.iter().any(|record| record == &records[1]),
            "the elapsed PEXPIREAT must not be recaptured as DEL"
        );
        let mut restored = Runtime::default_strict();
        let replies = restored.replay_aof_records(&captured, 3000);
        assert!(
            replies
                .iter()
                .all(|reply| !matches!(reply, RespFrame::Error(_))),
            "recaptured history must remain executable: {replies:?}"
        );
        assert_eq!(
            restored.execute_frame(command(&[b"GET", b"counter"]), 3000),
            RespFrame::BulkString(Some(b"42".to_vec()))
        );
    }
}

#[test]
fn aof_replay_getex_retains_elapsed_deadlines_until_later_persist() {
    let records = vec![
        aof_record(&[b"SET", b"counter", b"41"]),
        aof_record(&[b"GETEX", b"counter", b"PXAT", b"1500"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"GETEX", b"counter", b"PERSIST"]),
    ];
    let mut rt = Runtime::default_strict();
    assert_eq!(
        rt.replay_aof_records(&records, 2000),
        vec![
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::BulkString(Some(b"41".to_vec())),
            RespFrame::Integer(42),
            RespFrame::BulkString(Some(b"42".to_vec())),
        ]
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"counter"]), 2000),
        RespFrame::Integer(-1)
    );
}

#[test]
fn aof_replay_expiry_conditions_compare_original_absolute_deadlines() {
    let records = vec![
        aof_record(&[b"SET", b"counter", b"41", b"PXAT", b"1500"]),
        aof_record(&[b"PEXPIREAT", b"counter", b"1750", b"GT"]),
        aof_record(&[b"PEXPIRETIME", b"counter"]),
        aof_record(&[b"PEXPIREAT", b"counter", b"1600", b"LT"]),
        aof_record(&[b"PEXPIRETIME", b"counter"]),
        aof_record(&[b"PTTL", b"counter"]),
        aof_record(&[b"PERSIST", b"counter"]),
    ];
    let mut rt = Runtime::default_strict();
    assert_eq!(
        rt.replay_aof_records(&records, 2000),
        vec![
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(1),
            RespFrame::Integer(1750),
            RespFrame::Integer(1),
            RespFrame::Integer(1600),
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ],
        "GT/LT must compare with the stored deadline, not now plus clamped PTTL"
    );
}

#[test]
fn aof_replay_does_not_advance_the_supplied_clock_per_record() {
    let records = vec![
        aof_record(&[b"SET", b"counter", b"0"]),
        aof_record(&[b"INCR", b"counter"]),
        aof_record(&[b"SET", b"relative", b"value", b"PX", b"10"]),
    ];
    for mode in 0..3 {
        let mut rt = Runtime::default_strict();
        match mode {
            0 => {
                let path = expiry_aof_path("relative_clock");
                fr_persist::write_aof_file(&path, &records).expect("write history");
                rt.set_aof_path(path);
                assert_eq!(rt.load_aof(2000), Ok(records.len()));
            }
            1 => {
                let replies = rt.replay_aof_records(&records, 2000);
                assert!(replies.iter().all(|r| !matches!(r, RespFrame::Error(_))));
            }
            _ => {
                let replies = rt
                    .replay_aof_stream(&fr_persist::encode_aof_stream(&records), 2000)
                    .expect("decode replay stream");
                assert!(replies.iter().all(|r| !matches!(r, RespFrame::Error(_))));
            }
        }
        assert_eq!(
            rt.execute_frame(command(&[b"PEXPIRETIME", b"relative"]), 2000),
            RespFrame::Integer(2010),
            "record indexes are not elapsed milliseconds (replay mode {mode})"
        );
    }
}

#[test]
fn aof_replay_failure_restores_live_state_and_normal_expiration() {
    let records = vec![
        aof_record(&[b"SET", b"imported", b"41", b"PXAT", b"1500"]),
        aof_record(&[b"INCR", b"imported"]),
        aof_record(&[b"NOT-A-REDIS-COMMAND"]),
    ];
    let path = expiry_aof_path("failed_replay");
    fr_persist::write_aof_file(&path, &records).expect("write invalid history");
    let source_bytes = std::fs::read(&path).expect("read source history");
    let mut rt = Runtime::default_strict();
    rt.set_aof_path(path.clone());
    assert_eq!(
        rt.execute_frame(command(&[b"SET", b"keep", b"safe", b"PXAT", b"3000"]), 1000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        rt.execute_frame(
            command(&[b"SET", b"elapsed", b"old", b"PXAT", b"1500"]),
            1000
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    let original_records = rt.aof_records().to_vec();

    assert_eq!(rt.load_aof(2000), Err(PersistError::InvalidFrame));
    assert_eq!(rt.aof_records(), original_records.as_slice());
    assert_eq!(
        std::fs::read(path).expect("unchanged history"),
        source_bytes
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"keep"]), 2000),
        RespFrame::BulkString(Some(b"safe".to_vec()))
    );
    assert_eq!(
        rt.execute_frame(command(&[b"PTTL", b"keep"]), 2000),
        RespFrame::Integer(1000)
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"elapsed"]), 2000),
        RespFrame::BulkString(None),
        "a failed load must not leave ordinary client expiration disabled"
    );
    assert_eq!(
        rt.execute_frame(command(&[b"GET", b"imported"]), 2000),
        RespFrame::BulkString(None)
    );
}

#[test]
fn repeated_expiry_options_preserve_final_deadlines_in_aof_and_replication() {
    const NOW: u64 = 1_000_000;
    // The command grammar validates only the last occurrence of an expiry
    // kind. Earlier values can be valid, already elapsed, or even invalid.
    let cases: &[(&[&[u8]], i64)] = &[
        (
            &[
                b"SET", b"set-ex", b"value", b"EX", b"5", b"GET", b"EX", b"10",
            ],
            1_010_000,
        ),
        (
            &[b"SET", b"set-px", b"value", b"PX", b"5000", b"PX", b"10000"],
            1_010_000,
        ),
        (
            &[
                b"SET",
                b"set-exat",
                b"value",
                b"EXAT",
                b"1",
                b"EXAT",
                b"1020",
            ],
            1_020_000,
        ),
        (
            &[
                b"SET",
                b"set-pxat",
                b"value",
                b"PXAT",
                b"1",
                b"PXAT",
                b"1020000",
            ],
            1_020_000,
        ),
        (
            &[b"SET", b"set-invalid", b"value", b"EX", b"EX", b"EX", b"10"],
            1_010_000,
        ),
        (
            &[b"GETEX", b"getex-ex", b"EX", b"5", b"EX", b"10"],
            1_010_000,
        ),
        (
            &[b"GETEX", b"getex-px", b"PX", b"5000", b"PX", b"10000"],
            1_010_000,
        ),
        (
            &[b"GETEX", b"getex-exat", b"EXAT", b"1", b"EXAT", b"1020"],
            1_020_000,
        ),
        (
            &[b"GETEX", b"getex-pxat", b"PXAT", b"1", b"PXAT", b"1020000"],
            1_020_000,
        ),
        (
            &[b"GETEX", b"getex-invalid", b"ex", b"invalid", b"eX", b"10"],
            1_010_000,
        ),
    ];

    for route in ["direct", "transaction", "script"] {
        // Exercise both logical DB 0 backed by a nonzero physical DB and a
        // nonzero logical DB backed by physical DB 0 after SWAPDB.
        for db in [b"0".as_slice(), b"3".as_slice()] {
            let mut source = Runtime::default_strict();
            source.set_aof_path(expiry_aof_path("repeated_options_source"));
            assert_eq!(
                source.execute_frame(command(&[b"SWAPDB", b"0", b"3"]), NOW),
                RespFrame::SimpleString("OK".to_string())
            );
            assert_eq!(
                source.execute_frame(command(&[b"SELECT", db]), NOW),
                RespFrame::SimpleString("OK".to_string())
            );

            for (argv, _) in cases {
                if argv[0] == b"GETEX" {
                    assert_eq!(
                        source.execute_frame(command(&[b"SET", argv[1], b"value"]), NOW),
                        RespFrame::SimpleString("OK".to_string())
                    );
                }
                let reply = match route {
                    "direct" => source.execute_frame(command(argv), NOW),
                    "transaction" => {
                        assert_eq!(
                            source.execute_frame(command(&[b"MULTI"]), NOW),
                            RespFrame::SimpleString("OK".to_string())
                        );
                        assert_eq!(
                            source.execute_frame(
                                command(&[b"SET", b"transaction-marker", b"present"]),
                                NOW,
                            ),
                            RespFrame::SimpleString("QUEUED".to_string())
                        );
                        assert_eq!(
                            source.execute_frame(command(argv), NOW),
                            RespFrame::SimpleString("QUEUED".to_string())
                        );
                        let RespFrame::Array(Some(mut replies)) =
                            source.execute_frame(command(&[b"EXEC"]), NOW)
                        else {
                            panic!("EXEC must return its command replies");
                        };
                        assert_eq!(replies.len(), 2);
                        assert_eq!(replies[0], RespFrame::SimpleString("OK".to_string()));
                        replies.pop().expect("expiry command reply")
                    }
                    "script" => {
                        let mut eval: Vec<&[u8]> = vec![
                            b"EVAL",
                            b"redis.call('SET','script-marker','present'); return redis.call(unpack(ARGV))",
                            b"0",
                        ];
                        eval.extend_from_slice(argv);
                        source.execute_frame(command(&eval), NOW)
                    }
                    _ => unreachable!(),
                };
                assert!(
                    !matches!(reply, RespFrame::Error(_)),
                    "{route}: {argv:?} returned {reply:?}"
                );
            }

            let aof_bytes = source.encoded_aof_stream();
            let replication_bytes = source.encoded_aof_stream_from_offset(0);
            assert_eq!(aof_bytes, replication_bytes);
            let path = expiry_aof_path("repeated_options_replay");
            std::fs::write(&path, aof_bytes).expect("write captured incremental AOF");
            let mut reloaded = Runtime::default_strict();
            reloaded.set_aof_path(path);
            reloaded.load_aof(NOW + 6000).expect("reload captured AOF");

            let mut replica = Runtime::default_strict();
            replica.configure_startup_replica("127.0.0.1", 6380);
            let empty_snapshot = Runtime::default_strict().encoded_rdb_snapshot(NOW);
            replica
                .apply_replication_sync_payload(
                    "FULLRESYNC aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 0",
                    &empty_snapshot,
                    NOW,
                )
                .expect("initialize replica");
            for record in fr_persist::decode_aof_stream(&replication_bytes)
                .expect("decode captured replication stream")
            {
                let reply = replica.execute_replication_frame(&record.to_resp_frame(), NOW + 6000);
                assert!(
                    !matches!(reply, RespFrame::Error(_)),
                    "{route}: replication rejected {:?}: {reply:?}",
                    record.argv
                );
            }

            for (label, runtime) in [
                ("primary", &mut source),
                ("AOF reload", &mut reloaded),
                ("replica", &mut replica),
            ] {
                assert_eq!(
                    runtime.execute_frame(command(&[b"SELECT", db]), NOW + 6000),
                    RespFrame::SimpleString("OK".to_string())
                );
                for (argv, deadline) in cases {
                    assert_eq!(
                        runtime.execute_frame(command(&[b"GET", argv[1]]), NOW + 6000),
                        RespFrame::BulkString(Some(b"value".to_vec())),
                        "{route} {label}: superseded deadline must not expire {:?}",
                        argv[1]
                    );
                    assert_eq!(
                        runtime.execute_frame(command(&[b"PEXPIRETIME", argv[1]]), NOW + 6000),
                        RespFrame::Integer(*deadline),
                        "{route} {label}: final expiry must survive for {:?}",
                        argv[1]
                    );
                }
            }
        }
    }
}
