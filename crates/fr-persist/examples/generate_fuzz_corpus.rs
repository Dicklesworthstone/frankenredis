//! Generate seed corpus files for fuzz targets.
//! Run: cargo run -p fr-persist --example generate_fuzz_corpus

use fr_persist::{AofRecord, RdbEntry, RdbValue, encode_aof_stream, encode_rdb};
use std::fs;
use std::path::Path;

fn main() {
    let base = Path::new("fuzz/corpus");

    generate_rdb_corpus(&base.join("fuzz_rdb_decoder"));
    generate_aof_corpus(&base.join("fuzz_aof_decoder"));
    generate_dump_corpus(&base.join("fuzz_dump_restore"));

    println!("Corpus generation complete!");
}

fn generate_rdb_corpus(dir: &Path) {
    fs::create_dir_all(dir).ok();

    // String type
    let string_entry = vec![RdbEntry {
        db: 0,
        key: b"k".to_vec(),
        value: RdbValue::String(b"v".to_vec()),
        expire_ms: None,
    }];
    fs::write(dir.join("string_type"), encode_rdb(&string_entry, &[])).ok();

    // List type
    let list_entry = vec![RdbEntry {
        db: 0,
        key: b"mylist".to_vec(),
        value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]),
        expire_ms: None,
    }];
    fs::write(dir.join("list_type"), encode_rdb(&list_entry, &[])).ok();

    // Set type
    let set_entry = vec![RdbEntry {
        db: 0,
        key: b"myset".to_vec(),
        value: RdbValue::Set(vec![b"x".to_vec(), b"y".to_vec()]),
        expire_ms: None,
    }];
    fs::write(dir.join("set_type"), encode_rdb(&set_entry, &[])).ok();

    // Hash type
    let hash_entry = vec![RdbEntry {
        db: 0,
        key: b"myhash".to_vec(),
        value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
        expire_ms: None,
    }];
    fs::write(dir.join("hash_type"), encode_rdb(&hash_entry, &[])).ok();

    // Sorted set type
    let zset_entry = vec![RdbEntry {
        db: 0,
        key: b"myzset".to_vec(),
        value: RdbValue::SortedSet(vec![(b"m".to_vec(), 1.5)]),
        expire_ms: None,
    }];
    fs::write(dir.join("zset_type"), encode_rdb(&zset_entry, &[])).ok();

    // Stream type
    let stream_entry = vec![RdbEntry {
        db: 0,
        key: b"mystream".to_vec(),
        value: RdbValue::Stream(
            vec![(1000, 0, vec![(b"f".to_vec(), b"v".to_vec())])],
            Some((1000, 0)),
            Vec::new(),
            None,
        ),
        expire_ms: None,
    }];
    fs::write(dir.join("stream_type"), encode_rdb(&stream_entry, &[])).ok();

    // With expiry
    let expiry_entry = vec![RdbEntry {
        db: 0,
        key: b"temp".to_vec(),
        value: RdbValue::String(b"expires".to_vec()),
        expire_ms: Some(1_000_000_000),
    }];
    fs::write(dir.join("with_expiry"), encode_rdb(&expiry_entry, &[])).ok();

    // Multiple DBs
    let multi_db = vec![
        RdbEntry {
            db: 0,
            key: b"k0".to_vec(),
            value: RdbValue::String(b"v0".to_vec()),
            expire_ms: None,
        },
        RdbEntry {
            db: 5,
            key: b"k5".to_vec(),
            value: RdbValue::String(b"v5".to_vec()),
            expire_ms: None,
        },
    ];
    fs::write(dir.join("multi_db"), encode_rdb(&multi_db, &[])).ok();

    // With aux fields
    let aux_entry = vec![RdbEntry {
        db: 0,
        key: b"k".to_vec(),
        value: RdbValue::String(b"v".to_vec()),
        expire_ms: None,
    }];
    fs::write(
        dir.join("with_aux"),
        encode_rdb(&aux_entry, &[("redis-ver", "7.0.0")]),
    )
    .ok();

    // Binary data
    let binary_entry = vec![RdbEntry {
        db: 0,
        key: vec![0x00, 0xFF],
        value: RdbValue::String(vec![0x0D, 0x0A]),
        expire_ms: None,
    }];
    fs::write(dir.join("binary_data"), encode_rdb(&binary_entry, &[])).ok();

    println!("Generated 10 RDB corpus files in {:?}", dir);
}

fn generate_aof_corpus(dir: &Path) {
    fs::create_dir_all(dir).ok();

    // INCR command
    let incr = vec![AofRecord {
        argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
    }];
    fs::write(dir.join("incr_cmd"), encode_aof_stream(&incr)).ok();

    // LPUSH command
    let lpush = vec![AofRecord {
        argv: vec![b"LPUSH".to_vec(), b"mylist".to_vec(), b"a".to_vec()],
    }];
    fs::write(dir.join("lpush_cmd"), encode_aof_stream(&lpush)).ok();

    // SADD command
    let sadd = vec![AofRecord {
        argv: vec![b"SADD".to_vec(), b"myset".to_vec(), b"x".to_vec()],
    }];
    fs::write(dir.join("sadd_cmd"), encode_aof_stream(&sadd)).ok();

    // ZADD command
    let zadd = vec![AofRecord {
        argv: vec![
            b"ZADD".to_vec(),
            b"myzset".to_vec(),
            b"1.5".to_vec(),
            b"m".to_vec(),
        ],
    }];
    fs::write(dir.join("zadd_cmd"), encode_aof_stream(&zadd)).ok();

    // EXPIRE command
    let expire = vec![AofRecord {
        argv: vec![b"EXPIRE".to_vec(), b"key".to_vec(), b"3600".to_vec()],
    }];
    fs::write(dir.join("expire_cmd"), encode_aof_stream(&expire)).ok();

    // DEL command
    let del = vec![AofRecord {
        argv: vec![b"DEL".to_vec(), b"key1".to_vec(), b"key2".to_vec()],
    }];
    fs::write(dir.join("del_cmd"), encode_aof_stream(&del)).ok();

    // Multi-command sequence
    let multi = vec![
        AofRecord {
            argv: vec![b"SET".to_vec(), b"k1".to_vec(), b"v1".to_vec()],
        },
        AofRecord {
            argv: vec![b"SET".to_vec(), b"k2".to_vec(), b"v2".to_vec()],
        },
        AofRecord {
            argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
        },
    ];
    fs::write(dir.join("multi_sequence"), encode_aof_stream(&multi)).ok();

    // Binary args
    let binary = vec![AofRecord {
        argv: vec![b"SET".to_vec(), vec![0x00, 0xFF], vec![0x0D, 0x0A]],
    }];
    fs::write(dir.join("binary_args"), encode_aof_stream(&binary)).ok();

    // XADD command
    let xadd = vec![AofRecord {
        argv: vec![
            b"XADD".to_vec(),
            b"mystream".to_vec(),
            b"*".to_vec(),
            b"f".to_vec(),
            b"v".to_vec(),
        ],
    }];
    fs::write(dir.join("xadd_cmd"), encode_aof_stream(&xadd)).ok();

    println!("Generated 9 AOF corpus files in {:?}", dir);
}

fn generate_dump_corpus(dir: &Path) {
    fs::create_dir_all(dir).ok();

    // String payload structure for DUMP/RESTORE
    let mut string_dump = Vec::new();
    string_dump.push(0x00); // version
    string_dump.push(0x00); // type: string
    string_dump.push(0x05); // length
    string_dump.extend_from_slice(b"hello");
    string_dump.extend_from_slice(&11u16.to_le_bytes()); // RDB version
    string_dump.extend_from_slice(&[0; 8]); // CRC placeholder
    fs::write(dir.join("string_payload"), &string_dump).ok();

    // List payload
    let mut list_dump = vec![
        0x00, 0x01, // type: list
        0x02, // 2 elements
        0x01, b'a', 0x01, b'b',
    ];
    list_dump.extend_from_slice(&11u16.to_le_bytes());
    list_dump.extend_from_slice(&[0; 8]);
    fs::write(dir.join("list_payload"), &list_dump).ok();

    // Hash payload
    let mut hash_dump = vec![
        0x00, 0x04, // type: hash
        0x01, // 1 field
        0x01, b'f', 0x01, b'v',
    ];
    hash_dump.extend_from_slice(&11u16.to_le_bytes());
    hash_dump.extend_from_slice(&[0; 8]);
    fs::write(dir.join("hash_payload"), &hash_dump).ok();

    println!("Generated 3 DUMP corpus files in {:?}", dir);
}
