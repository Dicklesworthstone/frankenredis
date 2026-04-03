//! Integration tests for DUMP/RESTORE round-trip correctness across all data types.

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

/// Extract a bulk string payload from a DUMP response.
fn extract_dump_payload(frame: &RespFrame) -> Vec<u8> {
    match frame {
        RespFrame::BulkString(Some(data)) => data.clone(),
        other => panic!("expected bulk string from DUMP, got: {other:?}"),
    }
}

/// Helper: DUMP a key, DEL it, RESTORE it, verify the value matches.
fn dump_restore_roundtrip(rt: &mut Runtime, key: &[u8], now_ms: u64) {
    let dump = rt.execute_frame(command(&[b"DUMP", key]), now_ms);
    let payload = extract_dump_payload(&dump);

    // Delete the key
    let del = rt.execute_frame(command(&[b"DEL", key]), now_ms);
    assert_eq!(del, RespFrame::Integer(1));

    // Verify it's gone
    let exists = rt.execute_frame(command(&[b"EXISTS", key]), now_ms);
    assert_eq!(exists, RespFrame::Integer(0));

    // Restore
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(key.to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(payload)),
        ])),
        now_ms,
    );
    assert_eq!(
        restore,
        RespFrame::SimpleString("OK".to_string()),
        "RESTORE failed for key {:?}",
        String::from_utf8_lossy(key)
    );
}

#[test]
fn dump_restore_string() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"str_key", b"hello world"]), 0);

    dump_restore_roundtrip(&mut rt, b"str_key", 1);

    let val = rt.execute_frame(command(&[b"GET", b"str_key"]), 2);
    assert_eq!(val, RespFrame::BulkString(Some(b"hello world".to_vec())));
}

#[test]
fn dump_restore_integer_string() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"int_key", b"42"]), 0);

    dump_restore_roundtrip(&mut rt, b"int_key", 1);

    let val = rt.execute_frame(command(&[b"GET", b"int_key"]), 2);
    assert_eq!(val, RespFrame::BulkString(Some(b"42".to_vec())));
}

#[test]
fn dump_restore_empty_string() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"empty_key", b""]), 0);

    dump_restore_roundtrip(&mut rt, b"empty_key", 1);

    let val = rt.execute_frame(command(&[b"GET", b"empty_key"]), 2);
    assert_eq!(val, RespFrame::BulkString(Some(b"".to_vec())));
}

#[test]
fn dump_restore_list() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"RPUSH", b"list_key", b"a", b"b", b"c"]), 0);

    dump_restore_roundtrip(&mut rt, b"list_key", 1);

    let len = rt.execute_frame(command(&[b"LLEN", b"list_key"]), 2);
    assert_eq!(len, RespFrame::Integer(3));

    let range = rt.execute_frame(command(&[b"LRANGE", b"list_key", b"0", b"-1"]), 3);
    assert_eq!(
        range,
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"a".to_vec())),
            RespFrame::BulkString(Some(b"b".to_vec())),
            RespFrame::BulkString(Some(b"c".to_vec())),
        ]))
    );
}

#[test]
fn dump_restore_set() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SADD", b"set_key", b"x", b"y", b"z"]), 0);

    dump_restore_roundtrip(&mut rt, b"set_key", 1);

    let card = rt.execute_frame(command(&[b"SCARD", b"set_key"]), 2);
    assert_eq!(card, RespFrame::Integer(3));

    let is_x = rt.execute_frame(command(&[b"SISMEMBER", b"set_key", b"x"]), 3);
    assert_eq!(is_x, RespFrame::Integer(1));
    let is_y = rt.execute_frame(command(&[b"SISMEMBER", b"set_key", b"y"]), 3);
    assert_eq!(is_y, RespFrame::Integer(1));
    let is_z = rt.execute_frame(command(&[b"SISMEMBER", b"set_key", b"z"]), 3);
    assert_eq!(is_z, RespFrame::Integer(1));
}

#[test]
fn dump_restore_hash() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(
        command(&[b"HSET", b"hash_key", b"f1", b"v1", b"f2", b"v2"]),
        0,
    );

    dump_restore_roundtrip(&mut rt, b"hash_key", 1);

    let hlen = rt.execute_frame(command(&[b"HLEN", b"hash_key"]), 2);
    assert_eq!(hlen, RespFrame::Integer(2));

    let v1 = rt.execute_frame(command(&[b"HGET", b"hash_key", b"f1"]), 3);
    assert_eq!(v1, RespFrame::BulkString(Some(b"v1".to_vec())));
    let v2 = rt.execute_frame(command(&[b"HGET", b"hash_key", b"f2"]), 3);
    assert_eq!(v2, RespFrame::BulkString(Some(b"v2".to_vec())));
}

#[test]
fn dump_restore_sorted_set() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(
        command(&[
            b"ZADD",
            b"zset_key",
            b"1.5",
            b"a",
            b"2.5",
            b"b",
            b"3.5",
            b"c",
        ]),
        0,
    );

    dump_restore_roundtrip(&mut rt, b"zset_key", 1);

    let zcard = rt.execute_frame(command(&[b"ZCARD", b"zset_key"]), 2);
    assert_eq!(zcard, RespFrame::Integer(3));

    let score_a = rt.execute_frame(command(&[b"ZSCORE", b"zset_key", b"a"]), 3);
    assert_eq!(score_a, RespFrame::BulkString(Some(b"1.5".to_vec())));
    let score_c = rt.execute_frame(command(&[b"ZSCORE", b"zset_key", b"c"]), 3);
    assert_eq!(score_c, RespFrame::BulkString(Some(b"3.5".to_vec())));
}

#[test]
fn dump_restore_stream() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(
        command(&[b"XADD", b"stream_key", b"1-0", b"field1", b"value1"]),
        0,
    );
    rt.execute_frame(
        command(&[b"XADD", b"stream_key", b"2-0", b"field2", b"value2"]),
        0,
    );

    dump_restore_roundtrip(&mut rt, b"stream_key", 1);

    let xlen = rt.execute_frame(command(&[b"XLEN", b"stream_key"]), 2);
    assert_eq!(xlen, RespFrame::Integer(2));
}

#[test]
fn dump_missing_key_returns_null() {
    let mut rt = Runtime::default_strict();
    let dump = rt.execute_frame(command(&[b"DUMP", b"nosuchkey"]), 0);
    assert_eq!(dump, RespFrame::BulkString(None));
}

#[test]
fn restore_busykey_without_replace() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"key1", b"original"]), 0);

    // Dump key1
    let dump = rt.execute_frame(command(&[b"DUMP", b"key1"]), 1);
    let payload = extract_dump_payload(&dump);

    // Try to restore to existing key without REPLACE
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"key1".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(payload)),
        ])),
        2,
    );
    assert_eq!(
        restore,
        RespFrame::Error("BUSYKEY Target key name already exists.".to_string())
    );
}

#[test]
fn restore_with_replace_overwrites() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"key1", b"original"]), 0);

    // Dump key1
    let dump = rt.execute_frame(command(&[b"DUMP", b"key1"]), 1);
    let payload = extract_dump_payload(&dump);

    // Change the value
    rt.execute_frame(command(&[b"SET", b"key1", b"changed"]), 2);

    // Restore with REPLACE
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"key1".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(payload)),
            RespFrame::BulkString(Some(b"REPLACE".to_vec())),
        ])),
        3,
    );
    assert_eq!(restore, RespFrame::SimpleString("OK".to_string()));

    // Value should be back to original
    let val = rt.execute_frame(command(&[b"GET", b"key1"]), 4);
    assert_eq!(val, RespFrame::BulkString(Some(b"original".to_vec())));
}

#[test]
fn restore_with_ttl() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"ttl_key", b"value"]), 0);

    let dump = rt.execute_frame(command(&[b"DUMP", b"ttl_key"]), 1);
    let payload = extract_dump_payload(&dump);

    rt.execute_frame(command(&[b"DEL", b"ttl_key"]), 2);

    // Restore with 5000ms TTL
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"ttl_key".to_vec())),
            RespFrame::BulkString(Some(b"5000".to_vec())),
            RespFrame::BulkString(Some(payload)),
        ])),
        1000,
    );
    assert_eq!(restore, RespFrame::SimpleString("OK".to_string()));

    // Key exists at now_ms=2000 (TTL should be ~4000ms remaining)
    let ttl = rt.execute_frame(command(&[b"PTTL", b"ttl_key"]), 2000);
    if let RespFrame::Integer(remaining) = ttl {
        assert!(remaining > 0, "key should still have TTL");
        assert!(remaining <= 5000, "TTL should not exceed original");
    } else {
        panic!("expected integer from PTTL, got: {ttl:?}");
    }

    // Key should be expired after 6000ms
    let val = rt.execute_frame(command(&[b"GET", b"ttl_key"]), 7000);
    assert_eq!(val, RespFrame::BulkString(None));
}

#[test]
fn restore_invalid_payload() {
    let mut rt = Runtime::default_strict();
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"badkey".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(b"invalidpayload".to_vec())),
        ])),
        0,
    );
    assert_eq!(
        restore,
        RespFrame::Error("ERR DUMP payload version or checksum are wrong".to_string())
    );
}

#[test]
fn restore_corrupted_crc() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"crc_key", b"test"]), 0);

    let dump = rt.execute_frame(command(&[b"DUMP", b"crc_key"]), 1);
    let mut payload = extract_dump_payload(&dump);

    // Corrupt the last byte (part of CRC64)
    let last = payload.len() - 1;
    payload[last] ^= 0xFF;

    rt.execute_frame(command(&[b"DEL", b"crc_key"]), 2);

    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"crc_key".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(payload)),
        ])),
        3,
    );
    assert_eq!(
        restore,
        RespFrame::Error("ERR DUMP payload version or checksum are wrong".to_string())
    );
}

#[test]
fn dump_restore_to_different_key() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"src_key", b"data"]), 0);

    let dump = rt.execute_frame(command(&[b"DUMP", b"src_key"]), 1);
    let payload = extract_dump_payload(&dump);

    // Restore to a different key
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"dst_key".to_vec())),
            RespFrame::BulkString(Some(b"0".to_vec())),
            RespFrame::BulkString(Some(payload)),
        ])),
        2,
    );
    assert_eq!(restore, RespFrame::SimpleString("OK".to_string()));

    // Both keys should exist
    let src = rt.execute_frame(command(&[b"GET", b"src_key"]), 3);
    assert_eq!(src, RespFrame::BulkString(Some(b"data".to_vec())));
    let dst = rt.execute_frame(command(&[b"GET", b"dst_key"]), 3);
    assert_eq!(dst, RespFrame::BulkString(Some(b"data".to_vec())));
}

#[test]
fn dump_expired_key_returns_null() {
    let mut rt = Runtime::default_strict();
    // Set key with 1000ms TTL
    rt.execute_frame(command(&[b"SET", b"exp_key", b"val"]), 0);
    rt.execute_frame(command(&[b"PEXPIRE", b"exp_key", b"1000"]), 0);

    // DUMP after expiry
    let dump = rt.execute_frame(command(&[b"DUMP", b"exp_key"]), 2000);
    assert_eq!(dump, RespFrame::BulkString(None));
}

#[test]
fn restore_with_absttl() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"abs_key", b"value"]), 0);

    let dump = rt.execute_frame(command(&[b"DUMP", b"abs_key"]), 1);
    let payload = extract_dump_payload(&dump);

    rt.execute_frame(command(&[b"DEL", b"abs_key"]), 2);

    // Restore with ABSTTL at 10000ms (absolute time)
    let restore = rt.execute_frame(
        RespFrame::Array(Some(vec![
            RespFrame::BulkString(Some(b"RESTORE".to_vec())),
            RespFrame::BulkString(Some(b"abs_key".to_vec())),
            RespFrame::BulkString(Some(b"10000".to_vec())),
            RespFrame::BulkString(Some(payload)),
            RespFrame::BulkString(Some(b"ABSTTL".to_vec())),
        ])),
        5000,
    );
    assert_eq!(restore, RespFrame::SimpleString("OK".to_string()));

    // Should still exist at 8000ms
    let val = rt.execute_frame(command(&[b"GET", b"abs_key"]), 8000);
    assert_eq!(val, RespFrame::BulkString(Some(b"value".to_vec())));

    // Should be expired after 10000ms
    let val_expired = rt.execute_frame(command(&[b"GET", b"abs_key"]), 11000);
    assert_eq!(val_expired, RespFrame::BulkString(None));
}
