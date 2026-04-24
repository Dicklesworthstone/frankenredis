//! Integration tests for SCAN/HSCAN/SSCAN/ZSCAN full cursor iteration.
//! These verify that iterating with cursor from 0 back to 0 yields all
//! expected elements exactly once — something static conformance fixtures can't test.

use std::collections::BTreeSet;

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

/// Extract cursor and keys from a SCAN-family response [cursor, [elements...]].
fn parse_scan_response(frame: &RespFrame) -> (u64, Vec<Vec<u8>>) {
    assert!(
        matches!(frame, RespFrame::Array(Some(_))),
        "expected array from SCAN, got: {frame:?}"
    );
    let RespFrame::Array(Some(items)) = frame else {
        return (0, Vec::new());
    };
    assert_eq!(items.len(), 2, "SCAN response must have 2 elements");

    let cursor_frame = &items[0];
    assert!(
        matches!(cursor_frame, RespFrame::BulkString(Some(_))),
        "expected bulk string cursor, got: {cursor_frame:?}"
    );
    let RespFrame::BulkString(Some(c)) = cursor_frame else {
        return (0, Vec::new());
    };
    let parsed = String::from_utf8_lossy(c).parse::<u64>();
    assert!(parsed.is_ok(), "cursor should parse as u64");
    let cursor = parsed.unwrap_or(0);

    let elements_frame = &items[1];
    assert!(
        matches!(elements_frame, RespFrame::Array(Some(_))),
        "expected array of elements, got: {elements_frame:?}"
    );
    let RespFrame::Array(Some(elems)) = elements_frame else {
        return (0, Vec::new());
    };
    let elements = elems
        .iter()
        .filter_map(|e| {
            if let RespFrame::BulkString(Some(b)) = e {
                Some(b.clone())
            } else {
                None
            }
        })
        .collect();

    (cursor, elements)
}

/// Run a full SCAN iteration loop, returning all collected keys.
fn scan_all(rt: &mut Runtime, count: &[u8], now_ms: u64) -> BTreeSet<Vec<u8>> {
    let mut all_keys = BTreeSet::new();
    let mut cursor = 0u64;
    let mut iterations = 0;

    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SCAN".to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
                RespFrame::BulkString(Some(b"COUNT".to_vec())),
                RespFrame::BulkString(Some(count.to_vec())),
            ])),
            now_ms,
        );
        let (next_cursor, keys) = parse_scan_response(&resp);
        for key in keys {
            all_keys.insert(key);
        }
        cursor = next_cursor;
        iterations += 1;

        if cursor == 0 {
            break;
        }
        assert!(iterations < 1000, "SCAN loop exceeded 1000 iterations");
    }

    all_keys
}

/// Run a full HSCAN iteration loop.
fn hscan_all(rt: &mut Runtime, key: &[u8], now_ms: u64) -> BTreeSet<(Vec<u8>, Vec<u8>)> {
    let mut all_fields = BTreeSet::new();
    let mut cursor = 0u64;
    let mut iterations = 0;

    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"HSCAN".to_vec())),
                RespFrame::BulkString(Some(key.to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
            ])),
            now_ms,
        );
        let (next_cursor, elements) = parse_scan_response(&resp);
        // HSCAN returns [field, value, field, value, ...]
        for pair in elements.chunks(2) {
            if pair.len() == 2 {
                all_fields.insert((pair[0].clone(), pair[1].clone()));
            }
        }
        cursor = next_cursor;
        iterations += 1;

        if cursor == 0 {
            break;
        }
        assert!(iterations < 1000, "HSCAN loop exceeded 1000 iterations");
    }

    all_fields
}

/// Run a full SSCAN iteration loop.
fn sscan_all(rt: &mut Runtime, key: &[u8], now_ms: u64) -> BTreeSet<Vec<u8>> {
    let mut all_members = BTreeSet::new();
    let mut cursor = 0u64;
    let mut iterations = 0;

    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SSCAN".to_vec())),
                RespFrame::BulkString(Some(key.to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
            ])),
            now_ms,
        );
        let (next_cursor, members) = parse_scan_response(&resp);
        for member in members {
            all_members.insert(member);
        }
        cursor = next_cursor;
        iterations += 1;

        if cursor == 0 {
            break;
        }
        assert!(iterations < 1000, "SSCAN loop exceeded 1000 iterations");
    }

    all_members
}

/// Run a full ZSCAN iteration loop.
fn zscan_all(rt: &mut Runtime, key: &[u8], now_ms: u64) -> BTreeSet<(Vec<u8>, Vec<u8>)> {
    let mut all_members = BTreeSet::new();
    let mut cursor = 0u64;
    let mut iterations = 0;

    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"ZSCAN".to_vec())),
                RespFrame::BulkString(Some(key.to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
            ])),
            now_ms,
        );
        let (next_cursor, elements) = parse_scan_response(&resp);
        // ZSCAN returns [member, score, member, score, ...]
        for pair in elements.chunks(2) {
            if pair.len() == 2 {
                all_members.insert((pair[0].clone(), pair[1].clone()));
            }
        }
        cursor = next_cursor;
        iterations += 1;

        if cursor == 0 {
            break;
        }
        assert!(iterations < 1000, "ZSCAN loop exceeded 1000 iterations");
    }

    all_members
}

#[test]
fn scan_accepts_redis_cursor_spellings_and_rejects_invalid_cursor() {
    let mut rt = Runtime::default_strict();
    let empty_scan = RespFrame::Array(Some(vec![
        RespFrame::BulkString(Some(b"0".to_vec())),
        RespFrame::Array(Some(Vec::new())),
    ]));

    let resp = rt.execute_frame(command(&[b"SCAN", b"+1"]), 0);
    assert_eq!(resp, empty_scan);

    let resp = rt.execute_frame(command(&[b"SCAN", b"01"]), 0);
    assert_eq!(resp, empty_scan);

    let resp = rt.execute_frame(command(&[b"SCAN", b"abc"]), 0);
    assert!(
        matches!(resp, RespFrame::Error(ref msg) if msg == "ERR invalid cursor"),
        "expected invalid cursor error, got: {resp:?}"
    );
}

#[test]
fn scan_full_iteration_returns_all_keys() {
    let mut rt = Runtime::default_strict();

    // Create 20 keys of various types
    for i in 0..10 {
        rt.execute_frame(command(&[b"SET", format!("str:{i}").as_bytes(), b"val"]), 0);
    }
    for i in 0..5 {
        rt.execute_frame(
            command(&[b"RPUSH", format!("list:{i}").as_bytes(), b"item"]),
            0,
        );
    }
    for i in 0..5 {
        rt.execute_frame(
            command(&[b"SADD", format!("set:{i}").as_bytes(), b"member"]),
            0,
        );
    }

    // Full iteration with small COUNT should still return all 20 keys
    let all_keys = scan_all(&mut rt, b"2", 1);
    assert_eq!(all_keys.len(), 20, "should find all 20 keys via SCAN");

    // Verify specific keys exist
    assert!(all_keys.contains(b"str:0".as_slice()));
    assert!(all_keys.contains(b"str:9".as_slice()));
    assert!(all_keys.contains(b"list:0".as_slice()));
    assert!(all_keys.contains(b"set:4".as_slice()));
}

#[test]
fn scan_with_match_filter_during_iteration() {
    let mut rt = Runtime::default_strict();

    for i in 0..10 {
        rt.execute_frame(command(&[b"SET", format!("alpha:{i}").as_bytes(), b"a"]), 0);
        rt.execute_frame(command(&[b"SET", format!("beta:{i}").as_bytes(), b"b"]), 0);
    }

    // SCAN with MATCH alpha:* should return only alpha keys
    let mut alpha_keys = BTreeSet::new();
    let mut cursor = 0u64;
    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SCAN".to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
                RespFrame::BulkString(Some(b"MATCH".to_vec())),
                RespFrame::BulkString(Some(b"alpha:*".to_vec())),
                RespFrame::BulkString(Some(b"COUNT".to_vec())),
                RespFrame::BulkString(Some(b"3".to_vec())),
            ])),
            1,
        );
        let (next_cursor, keys) = parse_scan_response(&resp);
        for key in keys {
            alpha_keys.insert(key);
        }
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    assert_eq!(alpha_keys.len(), 10, "should find all 10 alpha keys");
    for i in 0..10 {
        assert!(alpha_keys.contains(&format!("alpha:{i}").into_bytes()));
    }
}

#[test]
fn scan_with_type_filter_during_iteration() {
    let mut rt = Runtime::default_strict();

    for i in 0..5 {
        rt.execute_frame(command(&[b"SET", format!("s:{i}").as_bytes(), b"v"]), 0);
        rt.execute_frame(command(&[b"RPUSH", format!("l:{i}").as_bytes(), b"v"]), 0);
    }

    // SCAN with TYPE list
    let mut list_keys = BTreeSet::new();
    let mut cursor = 0u64;
    loop {
        let cursor_bytes = cursor.to_string().into_bytes();
        let resp = rt.execute_frame(
            RespFrame::Array(Some(vec![
                RespFrame::BulkString(Some(b"SCAN".to_vec())),
                RespFrame::BulkString(Some(cursor_bytes)),
                RespFrame::BulkString(Some(b"TYPE".to_vec())),
                RespFrame::BulkString(Some(b"list".to_vec())),
                RespFrame::BulkString(Some(b"COUNT".to_vec())),
                RespFrame::BulkString(Some(b"2".to_vec())),
            ])),
            1,
        );
        let (next_cursor, keys) = parse_scan_response(&resp);
        for key in keys {
            list_keys.insert(key);
        }
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    assert_eq!(list_keys.len(), 5, "should find all 5 list keys");
}

#[test]
fn hscan_full_iteration() {
    let mut rt = Runtime::default_strict();

    // Create hash with 15 fields
    for i in 0..15 {
        rt.execute_frame(
            command(&[
                b"HSET",
                b"myhash",
                format!("field:{i}").as_bytes(),
                format!("val:{i}").as_bytes(),
            ]),
            0,
        );
    }

    let all_fields = hscan_all(&mut rt, b"myhash", 1);
    assert_eq!(all_fields.len(), 15, "HSCAN should return all 15 fields");

    // Verify specific field-value pair
    assert!(all_fields.contains(&(b"field:0".to_vec(), b"val:0".to_vec())));
    assert!(all_fields.contains(&(b"field:14".to_vec(), b"val:14".to_vec())));
}

#[test]
fn sscan_full_iteration() {
    let mut rt = Runtime::default_strict();

    // Create set with 12 members
    for i in 0..12 {
        rt.execute_frame(
            command(&[b"SADD", b"myset", format!("m:{i}").as_bytes()]),
            0,
        );
    }

    let all_members = sscan_all(&mut rt, b"myset", 1);
    assert_eq!(all_members.len(), 12, "SSCAN should return all 12 members");

    assert!(all_members.contains(b"m:0".as_slice()));
    assert!(all_members.contains(b"m:11".as_slice()));
}

#[test]
fn zscan_full_iteration() {
    let mut rt = Runtime::default_strict();

    // Create sorted set with 10 members
    for i in 0..10 {
        let score = format!("{}", i as f64 * 1.5);
        rt.execute_frame(
            command(&[
                b"ZADD",
                b"myzset",
                score.as_bytes(),
                format!("z:{i}").as_bytes(),
            ]),
            0,
        );
    }

    let all_members = zscan_all(&mut rt, b"myzset", 1);
    assert_eq!(all_members.len(), 10, "ZSCAN should return all 10 members");

    assert!(all_members.contains(&(b"z:0".to_vec(), b"0".to_vec())));
    assert!(all_members.contains(&(b"z:9".to_vec(), b"13.5".to_vec())));
}

#[test]
fn scan_empty_store_returns_cursor_zero() {
    let mut rt = Runtime::default_strict();

    let resp = rt.execute_frame(command(&[b"SCAN", b"0"]), 0);
    let (cursor, keys) = parse_scan_response(&resp);
    assert_eq!(cursor, 0);
    assert!(keys.is_empty());
}

#[test]
fn scan_single_key() {
    let mut rt = Runtime::default_strict();
    rt.execute_frame(command(&[b"SET", b"only_key", b"val"]), 0);

    let all_keys = scan_all(&mut rt, b"10", 1);
    assert_eq!(all_keys.len(), 1);
    assert!(all_keys.contains(b"only_key".as_slice()));
}
