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
