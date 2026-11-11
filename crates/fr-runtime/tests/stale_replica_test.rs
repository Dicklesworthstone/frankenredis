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

fn replica_runtime() -> Runtime {
    let mut runtime = Runtime::default_strict();
    assert_eq!(
        runtime.execute_frame(command(&[b"REPLICAOF", b"127.0.0.1", b"6380"]), 0),
        RespFrame::SimpleString("OK".to_string())
    );
    runtime
}

fn apply_primary_commands(runtime: &mut Runtime, commands: &[&[&[u8]]], now_ms: u64) {
    let mut wire = Vec::new();
    for parts in commands {
        command(parts).encode_into(&mut wire);
    }
    runtime
        .apply_replication_sync_payload("CONTINUE", &wire, now_ms)
        .expect("valid primary stream must apply");
}

fn persistence_test_directory(name: &str) -> std::path::PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock must be after the Unix epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "frankenredis-replica-expiry-{name}-{}-{timestamp}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("create isolated persistence directory");
    directory
}

#[test]
fn stale_replica_blocks_reads_when_configured() {
    let mut rt = Runtime::default_strict();

    // Configure as a replica
    rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"REPLICAOF".to_vec())),
            RespFrame::BulkString(Some(b"127.0.0.1".to_vec())),
            RespFrame::BulkString(Some(b"6379".to_vec())),
        ])),
        0,
    );

    // Set replica-serve-stale-data to no
    rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"CONFIG".to_vec())),
            RespFrame::BulkString(Some(b"SET".to_vec())),
            RespFrame::BulkString(Some(b"replica-serve-stale-data".to_vec())),
            RespFrame::BulkString(Some(b"no".to_vec())),
        ])),
        100,
    );

    // GET should be blocked (state = connect)
    let res = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"GET".to_vec())),
            RespFrame::BulkString(Some(b"foo".to_vec())),
        ])),
        200,
    );
    assert_eq!(
        res,
        RespFrame::Error(
            "MASTERDOWN Link with MASTER is down and replica-serve-stale-data is set to 'no'."
                .to_string()
        )
    );

    // INFO should be allowed
    let res = rt.execute_frame(
        RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"INFO".to_vec()))])),
        300,
    );
    assert!(matches!(res, RespFrame::BulkString(Some(_))));
}

#[test]
fn replica_reads_and_active_expiry_preserve_delayed_primary_counter_history() {
    let mut runtime = replica_runtime();
    apply_primary_commands(
        &mut runtime,
        &[
            &[b"SELECT", b"3"],
            &[b"SET", b"counter", b"41"],
            &[b"PEXPIREAT", b"counter", b"1500"],
        ],
        1000,
    );

    // The primary issued its later INCR/PERSIST before the deadline, but the
    // replica has not received them yet. Readers must see a missing key while
    // the original value remains available to that delayed stream.
    let records_before_reads = runtime.aof_records().len();
    let offset_before_reads = runtime.replication_primary_offset();
    for parts in [
        &[b"GET".as_slice(), b"counter"][..],
        &[
            b"EVAL_RO".as_slice(),
            b"return redis.call('GET', KEYS[1])",
            b"1",
            b"counter",
        ][..],
    ] {
        assert_eq!(
            runtime.execute_frame(command(parts), 2000),
            RespFrame::BulkString(None)
        );
    }
    assert_eq!(
        runtime.execute_frame(command(&[b"PTTL", b"counter"]), 2000),
        RespFrame::Integer(-2)
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"EXISTS", b"counter"]), 2000),
        RespFrame::Integer(0)
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"MGET", b"counter"]), 2000),
        RespFrame::Array(Some(vec![RespFrame::BulkString(None)]))
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"MULTI"]), 2000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::SimpleString("QUEUED".to_string())
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"EXEC"]), 2000),
        RespFrame::Array(Some(vec![RespFrame::BulkString(None)]))
    );
    assert_eq!(
        runtime
            .run_server_cron_active_expire_cycle(2000)
            .evicted_keys,
        0
    );
    assert_eq!(runtime.aof_records().len(), records_before_reads);
    assert_eq!(runtime.replication_primary_offset(), offset_before_reads);
    assert_eq!(
        runtime.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1),
        "DBSIZE counts the retained physical key on a replica"
    );

    assert_eq!(
        runtime.execute_replication_frame(&command(&[b"INCR", b"counter"]), 2001),
        RespFrame::Integer(42),
        "the primary stream must increment the retained 41, not an empty key"
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"GET", b"counter"]), 2001),
        RespFrame::BulkString(None),
        "the replay's historical visibility must not leak to ordinary readers"
    );
    apply_primary_commands(&mut runtime, &[&[b"PERSIST", b"counter"]], 2002);
    assert_eq!(
        runtime.execute_frame(command(&[b"GET", b"counter"]), 2002),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"PTTL", b"counter"]), 2002),
        RespFrame::Integer(-1)
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"INCR", b"counter"]), 2002),
        RespFrame::Error("READONLY You can't write against a read only replica.".to_string()),
        "replay must not leave ordinary clients exempt from the write gate"
    );
}

#[test]
fn replica_hides_expired_core_types_without_destroying_their_contents() {
    let mut runtime = replica_runtime();
    apply_primary_commands(
        &mut runtime,
        &[
            &[b"SET", b"string", b"value"],
            &[b"HSET", b"hash", b"field", b"value"],
            &[b"RPUSH", b"list", b"value"],
            &[b"SADD", b"set", b"value"],
            &[b"ZADD", b"zset", b"2", b"value"],
            &[b"XADD", b"stream", b"1-0", b"field", b"value"],
        ],
        1000,
    );
    let keys: &[&[u8]] = &[b"string", b"hash", b"list", b"set", b"zset", b"stream"];
    for key in keys {
        assert_eq!(
            runtime.execute_replication_frame(&command(&[b"PEXPIREAT", key, b"1500"]), 1000),
            RespFrame::Integer(1)
        );
    }

    let empty_array = RespFrame::Array(Some(Vec::new()));
    let cases: &[(&[&[u8]], RespFrame, RespFrame)] = &[
        (
            &[b"GET", b"string"],
            RespFrame::BulkString(None),
            RespFrame::BulkString(Some(b"value".to_vec())),
        ),
        (
            &[b"STRLEN", b"string"],
            RespFrame::Integer(0),
            RespFrame::Integer(5),
        ),
        (
            &[b"HGET", b"hash", b"field"],
            RespFrame::BulkString(None),
            RespFrame::BulkString(Some(b"value".to_vec())),
        ),
        (
            &[b"HGETALL", b"hash"],
            empty_array.clone(),
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"field".to_vec())),
                RespFrame::BulkString(Some(b"value".to_vec())),
            ])),
        ),
        (
            &[b"LLEN", b"list"],
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ),
        (
            &[b"LRANGE", b"list", b"0", b"-1"],
            empty_array.clone(),
            RespFrame::Array(Some(vec![RespFrame::BulkString(Some(b"value".to_vec()))])),
        ),
        (
            &[b"SCARD", b"set"],
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ),
        (
            &[b"SISMEMBER", b"set", b"value"],
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ),
        (
            &[b"ZCARD", b"zset"],
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ),
        (
            &[b"ZSCORE", b"zset", b"value"],
            RespFrame::BulkString(None),
            RespFrame::BulkString(Some(b"2".to_vec())),
        ),
        (
            &[b"XLEN", b"stream"],
            RespFrame::Integer(0),
            RespFrame::Integer(1),
        ),
        (
            &[b"XRANGE", b"stream", b"-", b"+"],
            empty_array.clone(),
            RespFrame::Array(Some(vec![RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"1-0".to_vec())),
                RespFrame::Array(Some(vec![
                    RespFrame::BulkString(Some(b"field".to_vec())),
                    RespFrame::BulkString(Some(b"value".to_vec())),
                ])),
            ]))])),
        ),
    ];
    for (parts, missing, _) in cases {
        assert_eq!(
            &runtime.execute_frame(command(parts), 2000),
            missing,
            "elapsed replica key must be hidden from {parts:?}"
        );
    }
    for key in keys {
        assert_eq!(
            runtime.execute_frame(command(&[b"TYPE", key]), 2000),
            RespFrame::SimpleString("none".to_string())
        );
        assert_eq!(
            runtime.execute_frame(command(&[b"DUMP", key]), 2000),
            RespFrame::BulkString(None)
        );
    }
    assert_eq!(
        runtime.execute_frame(command(&[b"KEYS", b"*"]), 2000),
        empty_array
    );

    // SCAN may return an empty page with a nonzero cursor. Traverse the whole
    // dictionary so hiding the first page alone cannot satisfy this check.
    let mut cursor = b"0".to_vec();
    let mut completed = false;
    for _ in 0..1024 {
        let page = runtime.execute_frame(command(&[b"SCAN", &cursor, b"COUNT", b"100"]), 2000);
        let RespFrame::Array(Some(items)) = page else {
            panic!("SCAN must return a cursor and page");
        };
        let [
            RespFrame::BulkString(Some(next)),
            RespFrame::Array(Some(keys)),
        ] = items.as_slice()
        else {
            panic!("unexpected SCAN reply: {items:?}");
        };
        assert!(
            keys.is_empty(),
            "SCAN exposed elapsed replica keys: {keys:?}"
        );
        cursor.clone_from(next);
        if cursor == b"0" {
            completed = true;
            break;
        }
    }
    assert!(completed, "SCAN must terminate with retained expired keys");
    assert_eq!(
        runtime
            .run_server_cron_active_expire_cycle(2000)
            .evicted_keys,
        0
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(6)
    );

    for key in keys {
        assert_eq!(
            runtime.execute_replication_frame(&command(&[b"PERSIST", key]), 2001),
            RespFrame::Integer(1)
        );
    }
    for (parts, _, restored) in cases {
        assert_eq!(
            &runtime.execute_frame(command(parts), 2001),
            restored,
            "read-side expiry must preserve the original contents for {parts:?}"
        );
    }
}

#[test]
fn replica_randomkey_terminates_when_every_physical_key_is_expired() {
    let mut runtime = replica_runtime();
    apply_primary_commands(
        &mut runtime,
        &[
            &[b"SET", b"only-key", b"41"],
            &[b"PEXPIREAT", b"only-key", b"1500"],
        ],
        1000,
    );
    // Redis 7.2.4 dbRandomKey bounds retries on an all-volatile replica and
    // eventually returns a retained key name, even when that key is expired.
    assert_eq!(
        runtime.execute_frame(command(&[b"RANDOMKEY"]), 2000),
        RespFrame::BulkString(Some(b"only-key".to_vec()))
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"GET", b"only-key"]), 2000),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1)
    );
    assert_eq!(
        runtime.execute_replication_frame(&command(&[b"PERSIST", b"only-key"]), 2001),
        RespFrame::Integer(1)
    );
    assert_eq!(
        runtime.execute_frame(command(&[b"GET", b"only-key"]), 2001),
        RespFrame::BulkString(Some(b"41".to_vec()))
    );
}

#[test]
fn fullresync_retains_elapsed_snapshot_entries_until_the_primary_catches_up() {
    let mut primary = Runtime::default_strict();
    for parts in [
        &[b"SET".as_slice(), b"counter", b"41"][..],
        &[b"PEXPIREAT".as_slice(), b"counter", b"1500"][..],
        &[b"SELECT".as_slice(), b"3"][..],
        &[b"HSET".as_slice(), b"hash", b"counter", b"41"][..],
        &[b"PEXPIREAT".as_slice(), b"hash", b"1500"][..],
    ] {
        let reply = primary.execute_frame(command(parts), 1000);
        assert!(!matches!(reply, RespFrame::Error(_)), "{reply:?}");
    }
    let snapshot = primary.encoded_rdb_snapshot(1001);
    let mut replica = replica_runtime();
    replica
        .apply_replication_sync_payload(
            "FULLRESYNC aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 100",
            &snapshot,
            2000,
        )
        .expect("elapsed keys from a full-sync snapshot must be retained");

    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1)
    );
    assert_eq!(
        replica.execute_frame(command(&[b"SELECT", b"3"]), 2000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        replica.execute_frame(command(&[b"HGET", b"hash", b"counter"]), 2000),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1)
    );
    apply_primary_commands(
        &mut replica,
        &[
            &[b"SELECT", b"0"],
            &[b"INCR", b"counter"],
            &[b"PERSIST", b"counter"],
            &[b"SELECT", b"3"],
            &[b"HINCRBY", b"hash", b"counter", b"1"],
            &[b"PERSIST", b"hash"],
        ],
        2001,
    );
    assert_eq!(
        replica.execute_frame(command(&[b"HGET", b"hash", b"counter"]), 2001),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
    replica.execute_frame(command(&[b"SELECT", b"0"]), 2001);
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"counter"]), 2001),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
}

#[test]
fn replica_snapshot_preserves_hidden_history_for_a_downstream_replica() {
    let mut replica = replica_runtime();
    apply_primary_commands(
        &mut replica,
        &[
            &[b"SET", b"counter", b"41"],
            &[b"PEXPIREAT", b"counter", b"1500"],
        ],
        1000,
    );
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(None)
    );
    let snapshot = replica.encoded_rdb_snapshot(2000);
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1),
        "snapshot generation must not expire primary-owned replica keys"
    );
    let mut downstream = replica_runtime();
    downstream
        .apply_replication_sync_payload(
            "FULLRESYNC aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 100",
            &snapshot,
            2001,
        )
        .expect("apply downstream snapshot");
    assert_eq!(
        downstream.execute_frame(command(&[b"GET", b"counter"]), 2001),
        RespFrame::BulkString(None)
    );
    apply_primary_commands(
        &mut downstream,
        &[
            &[b"MULTI"],
            &[b"INCR", b"counter"],
            &[b"PERSIST", b"counter"],
            &[b"EXEC"],
        ],
        2002,
    );
    assert_eq!(
        downstream.execute_frame(command(&[b"GET", b"counter"]), 2002),
        RespFrame::BulkString(Some(b"42".to_vec())),
        "the retained value must survive snapshotting, full sync, and transaction replay"
    );
}

#[test]
fn primary_deletion_and_promotion_end_replica_expiry_retention() {
    let mut replica = replica_runtime();
    apply_primary_commands(
        &mut replica,
        &[
            &[b"SET", b"deleted", b"41"],
            &[b"PEXPIREAT", b"deleted", b"1500"],
            &[b"SET", b"promoted", b"41"],
            &[b"PEXPIREAT", b"promoted", b"1500"],
        ],
        1000,
    );
    assert_eq!(
        replica
            .run_server_cron_active_expire_cycle(2000)
            .evicted_keys,
        0
    );
    assert_eq!(
        replica.execute_replication_frame(&command(&[b"DEL", b"deleted"]), 2000),
        RespFrame::Integer(1)
    );
    assert_eq!(
        replica.execute_replication_frame(&command(&[b"PERSIST", b"deleted"]), 2000),
        RespFrame::Integer(0),
        "a primary DEL must physically discard retained history"
    );
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(1)
    );

    assert_eq!(
        replica.execute_frame(command(&[b"REPLICAOF", b"NO", b"ONE"]), 2001),
        RespFrame::SimpleString("OK".to_string())
    );
    let _ = replica.run_server_cron_active_expire_cycle(2001);
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"promoted"]), 2001),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2001),
        RespFrame::Integer(0),
        "a promoted node must resume authoritative expiration"
    );
    assert_eq!(
        replica.execute_frame(command(&[b"INCR", b"promoted"]), 2002),
        RespFrame::Integer(1)
    );
}

#[test]
fn writable_replica_expires_local_ttls_and_write_lookups_only() {
    let mut replica = replica_runtime();
    assert_eq!(
        replica.execute_frame(
            command(&[b"CONFIG", b"SET", b"replica-read-only", b"no"]),
            1000,
        ),
        RespFrame::SimpleString("OK".to_string())
    );
    apply_primary_commands(
        &mut replica,
        &[
            &[b"SET", b"remote", b"41"],
            &[b"PEXPIREAT", b"remote", b"1500"],
            &[b"SET", b"overwritten", b"41"],
            &[b"PEXPIREAT", b"overwritten", b"1500"],
        ],
        1000,
    );
    assert_eq!(
        replica.execute_frame(command(&[b"SET", b"local", b"9", b"PX", b"500"]), 1000),
        RespFrame::SimpleString("OK".to_string())
    );
    assert_eq!(
        replica
            .run_server_cron_active_expire_cycle(2000)
            .evicted_keys,
        1
    );
    for key in [b"local".as_slice(), b"remote", b"overwritten"] {
        assert_eq!(
            replica.execute_frame(command(&[b"GET", key]), 2000),
            RespFrame::BulkString(None)
        );
    }
    assert_eq!(
        replica.execute_frame(command(&[b"DBSIZE"]), 2000),
        RespFrame::Integer(2),
        "active expiration may collect the local TTL, but not the primary's values"
    );
    assert_eq!(
        replica.execute_frame(command(&[b"INCR", b"overwritten"]), 2000),
        RespFrame::Integer(1),
        "an ordinary write on a writable replica must collect the elapsed value"
    );
    apply_primary_commands(
        &mut replica,
        &[&[b"INCR", b"remote"], &[b"PERSIST", b"remote"]],
        2001,
    );
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"remote"]), 2001),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"overwritten"]), 2001),
        RespFrame::BulkString(Some(b"1".to_vec()))
    );
}

#[test]
fn replica_rdb_restart_preserves_hidden_history_but_primary_load_discards_it() {
    let directory = persistence_test_directory("rdb-restart");
    let path = directory.join("dump.rdb");
    let mut replica = replica_runtime();
    replica.set_rdb_path(path.clone());
    replica.set_rdb_fec(false);
    apply_primary_commands(
        &mut replica,
        &[
            &[b"SELECT", b"3"],
            &[b"SET", b"counter", b"41"],
            &[b"PEXPIREAT", b"counter", b"1500"],
        ],
        1000,
    );
    assert_eq!(
        replica.execute_frame(command(&[b"GET", b"counter"]), 2000),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        replica.execute_frame(command(&[b"SAVE"]), 2000),
        RespFrame::SimpleString("OK".to_string())
    );
    let snapshot = std::fs::read(&path).expect("SAVE must publish the RDB snapshot");
    let (entries, _) = fr_persist::decode_rdb(&snapshot).expect("saved RDB must decode");
    assert_eq!(entries.len(), 1, "SAVE must retain the hidden physical key");
    assert_eq!(entries[0].db, 3);
    assert_eq!(entries[0].key, b"counter");
    assert_eq!(entries[0].expire_ms, Some(1500));

    let mut restarted_replica = replica_runtime();
    restarted_replica.set_rdb_path(path.clone());
    assert_eq!(
        restarted_replica.load_rdb(2500).expect("load replica RDB"),
        1
    );
    restarted_replica.execute_frame(command(&[b"SELECT", b"3"]), 2500);
    assert_eq!(
        restarted_replica.execute_frame(command(&[b"GET", b"counter"]), 2500),
        RespFrame::BulkString(None)
    );
    assert_eq!(
        restarted_replica.execute_frame(command(&[b"DBSIZE"]), 2500),
        RespFrame::Integer(1)
    );
    assert_eq!(
        restarted_replica
            .run_server_cron_active_expire_cycle(2500)
            .evicted_keys,
        0
    );
    apply_primary_commands(
        &mut restarted_replica,
        &[&[b"INCR", b"counter"], &[b"PERSIST", b"counter"]],
        2501,
    );
    assert_eq!(
        restarted_replica.execute_frame(command(&[b"GET", b"counter"]), 2501),
        RespFrame::BulkString(Some(b"42".to_vec()))
    );

    // A primary loading exactly the same bytes owns expiry decisions itself;
    // retaining replica history must not resurrect expired primary data.
    let mut restarted_primary = Runtime::default_strict();
    restarted_primary.set_rdb_path(path.clone());
    assert_eq!(
        restarted_primary.load_rdb(2500).expect("load primary RDB"),
        0
    );
    restarted_primary.execute_frame(command(&[b"SELECT", b"3"]), 2500);
    assert_eq!(
        restarted_primary.execute_frame(command(&[b"DBSIZE"]), 2500),
        RespFrame::Integer(0)
    );
    assert_eq!(
        restarted_primary.execute_frame(command(&[b"INCR", b"counter"]), 2500),
        RespFrame::Integer(1)
    );
    assert_eq!(
        std::fs::read(path).expect("read original snapshot after both loads"),
        snapshot,
        "loading and catching up must leave the committed snapshot unchanged"
    );
}

#[test]
fn writable_replica_local_expiration_survives_aof_restart_without_stream_injection() {
    for lazyfree in [false, true] {
        let directory = persistence_test_directory(if lazyfree { "aof-unlink" } else { "aof-del" });
        let path = directory.join("appendonly.aof");
        let mut replica = replica_runtime();
        replica.set_aof_path(path.clone());
        assert_eq!(
            replica.execute_frame(
                command(&[b"CONFIG", b"SET", b"replica-read-only", b"no"]),
                1000,
            ),
            RespFrame::SimpleString("OK".to_string())
        );
        if lazyfree {
            assert_eq!(
                replica.execute_frame(
                    command(&[b"CONFIG", b"SET", b"lazyfree-lazy-expire", b"yes"]),
                    1000,
                ),
                RespFrame::SimpleString("OK".to_string())
            );
        }
        apply_primary_commands(
            &mut replica,
            &[
                &[b"SET", b"remote", b"41"],
                &[b"PEXPIREAT", b"remote", b"1500"],
            ],
            1000,
        );
        let primary_offset = replica.replication_primary_offset();
        let inherited_stream = replica.encoded_aof_stream_from_offset(0);
        replica.execute_frame(command(&[b"SELECT", b"3"]), 1000);
        assert_eq!(
            replica.execute_frame(command(&[b"SET", b"local", b"9", b"PX", b"500"]), 1000),
            RespFrame::SimpleString("OK".to_string())
        );
        // Establish a real multipart base containing both keys, before the
        // local deadline elapses. Its deletion must be an incremental effect.
        replica
            .flush_aof_for_stop(1001)
            .expect("publish initial AOF generation");
        let record_count_before_expiry = replica.aof_records().len();
        assert_eq!(
            replica
                .run_server_cron_active_expire_cycle(2000)
                .evicted_keys,
            1
        );
        let local_deletions = &replica.aof_records()[record_count_before_expiry..];
        let expected_command: &[u8] = if lazyfree { b"UNLINK" } else { b"DEL" };
        assert!(
            local_deletions.iter().any(|record| {
                record.argv == vec![expected_command.to_vec(), b"local".to_vec()]
            }),
            "the locally owned expiration must be captured for the AOF: {local_deletions:?}"
        );
        assert_eq!(replica.replication_primary_offset(), primary_offset);
        assert_eq!(
            replica.encoded_aof_stream_from_offset(0),
            inherited_stream,
            "local writes and expirations must not change the inherited stream's bytes or DB"
        );
        assert!(
            replica
                .encoded_aof_stream_from_offset(primary_offset.0)
                .is_empty()
        );
        replica
            .flush_aof_for_stop(2000)
            .expect("persist local expiration");
        drop(replica);

        let mut restarted = replica_runtime();
        restarted.set_aof_path(path);
        restarted
            .load_aof(2500)
            .expect("replay real multipart replica AOF");
        restarted.execute_frame(command(&[b"SELECT", b"3"]), 2500);
        assert_eq!(
            restarted.execute_frame(command(&[b"DBSIZE"]), 2500),
            RespFrame::Integer(0),
            "the AOF's local deletion must survive while expiry is disabled during replay"
        );
        restarted.execute_frame(command(&[b"SELECT", b"0"]), 2500);
        assert_eq!(
            restarted.execute_frame(command(&[b"DBSIZE"]), 2500),
            RespFrame::Integer(1),
            "the expired primary-owned value must remain physically present"
        );
        assert_eq!(
            restarted.execute_frame(command(&[b"GET", b"remote"]), 2500),
            RespFrame::BulkString(None)
        );
        apply_primary_commands(
            &mut restarted,
            &[&[b"INCR", b"remote"], &[b"PERSIST", b"remote"]],
            2501,
        );
        assert_eq!(
            restarted.execute_frame(command(&[b"GET", b"remote"]), 2501),
            RespFrame::BulkString(Some(b"42".to_vec()))
        );
    }
}
