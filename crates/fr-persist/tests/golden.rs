use fr_persist::parse_aof_manifest;
use fr_persist::{
    RdbValue, StreamEntry, UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, decode_upstream_stream_payload,
    encode_upstream_stream_listpacks3_payload,
};
use std::fs;
use std::path::Path;

fn assert_golden(test_name: &str, actual: &str) {
    let golden_path = Path::new("tests/golden").join(format!("{}.golden", test_name));

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        fs::write(&golden_path, actual).unwrap();
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
        panic!(
            "Golden file missing: {}\n\
             Run with UPDATE_GOLDENS=1 to create it",
            golden_path.display()
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual).unwrap();

        panic!(
            "GOLDEN MISMATCH: {}\n\
             To update: UPDATE_GOLDENS=1 cargo test --test golden\n\
             To review: diff {} {}",
            test_name,
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn parse_and_snapshot(test_name: &str, manifest_data: &str) {
    match parse_aof_manifest(manifest_data) {
        Ok(result) => {
            let actual = format!("{:#?}", result);
            assert_golden(test_name, &actual);
        }
        Err(e) => {
            let actual = format!("Error: {:#?}", e);
            assert_golden(test_name, &actual);
        }
    }
}

#[test]
fn golden_manifest_empty() {
    parse_and_snapshot("manifest_empty", "");
}

#[test]
fn golden_manifest_single_base() {
    parse_and_snapshot(
        "manifest_single_base",
        "file appendonly.aof.1.base.rdb seq 1 type b\n",
    );
}

#[test]
fn golden_manifest_base_and_incremental() {
    parse_and_snapshot(
        "manifest_base_and_incremental",
        "file appendonly.aof.1.base.rdb seq 1 type b\nfile appendonly.aof.1.incr.aof seq 1 type i\n",
    );
}

#[test]
fn golden_manifest_history_entries() {
    parse_and_snapshot(
        "manifest_history_entries",
        "file appendonly.aof.1.base.rdb seq 1 type h\nfile appendonly.aof.1.incr.aof seq 1 type h\nfile appendonly.aof.2.base.rdb seq 2 type b\nfile appendonly.aof.2.incr.aof seq 2 type i\n",
    );
}

#[test]
fn golden_manifest_invalid_type() {
    parse_and_snapshot(
        "manifest_invalid_type",
        "file appendonly.aof.1.base.rdb seq 1 type x\n",
    );
}

#[test]
fn golden_manifest_missing_seq() {
    parse_and_snapshot(
        "manifest_missing_seq",
        "file appendonly.aof.1.base.rdb type b\n",
    );
}

#[test]
fn golden_manifest_missing_file() {
    parse_and_snapshot("manifest_missing_file", "seq 1 type b\n");
}

#[test]
fn golden_manifest_invalid_seq_number() {
    parse_and_snapshot(
        "manifest_invalid_seq_number",
        "file appendonly.aof.1.base.rdb seq 01 type b\n",
    );
}

/// Byte-exact regression vs vendored Redis 7.2.4 (frankenredis-ren6y).
///
/// `stream_type21_vendored_redis_724.dump` is the raw `DUMP` payload Redis
/// 7.2.4 produced for a 250-entry stream (explicit IDs, mixed integer/string
/// values, reused field names, a consumer group with a populated PEL) — large
/// enough to force several `stream-node-max-bytes`/`-entries` macro-node
/// splits. Decoding it and re-encoding through our type-21 synthesizer must
/// reproduce the payload byte-for-byte: this locks in that our node packing,
/// listpack integer encoding, and consumer-group serialization match upstream
/// exactly (so a stream rebuilt from live state DUMPs identically and Redis
/// can RESTORE our RDB).
#[test]
fn golden_stream_type21_byte_exact_vs_vendored_redis_724() {
    let dump = fs::read(Path::new("tests/golden").join("stream_type21_vendored_redis_724.dump"))
        .expect("read vendored redis stream dump fixture");
    // A DUMP payload is `[type byte][value body][2-byte rdbver][8-byte crc64]`.
    let body = &dump[1..dump.len() - 10];
    assert_eq!(
        dump[0], UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3,
        "fixture must be a type-21 stream"
    );

    let (value, consumed) =
        decode_upstream_stream_payload(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, body)
            .expect("decode vendored stream body");
    assert_eq!(consumed, body.len(), "decoder must consume the whole body");

    let RdbValue::Stream(entries, watermark, groups, _metadata, entries_added, max_deleted) = value
    else {
        panic!("expected a stream value");
    };

    let reencoded = encode_upstream_stream_listpacks3_payload(
        &entries,
        watermark,
        &groups,
        entries_added,
        max_deleted,
    )
    .expect("re-encode synthesized type-21 payload");

    assert_eq!(
        reencoded.len(),
        body.len(),
        "re-encoded length must equal the vendored Redis body length"
    );
    assert_eq!(
        reencoded, body,
        "re-encoded type-21 bytes must equal vendored Redis byte-for-byte"
    );
}

/// Persistence round-trip proof for the max-deleted-entry-id watermark
/// (frankenredis-fplrm). A stream synthesized from live state — i.e. with no
/// raw upstream metadata, as `store_to_rdb_entries` produces for an fr-native
/// stream — must carry its max-deleted-entry-id through type-21 encode→decode.
/// Before fplrm the encoder hardcoded `0-0`, so an XDEL'd fr-native stream
/// reloaded with max-deleted-entry-id `0-0`, corrupting XINFO lag/tombstone
/// logic. Here `7-3` must survive; a stream that deleted nothing stays `None`.
#[test]
fn type21_round_trips_max_deleted_entry_id_fplrm() {
    let entries: Vec<StreamEntry> = vec![
        (10, 0, vec![(b"f".to_vec(), b"v1".to_vec())]),
        (20, 0, vec![(b"f".to_vec(), b"v2".to_vec())]),
    ];
    let groups: Vec<fr_persist::RdbStreamConsumerGroup> = Vec::new();

    for max_deleted in [Some((7u64, 3u64)), None] {
        let payload = encode_upstream_stream_listpacks3_payload(
            &entries,
            Some((20, 0)),
            &groups,
            Some(2),
            max_deleted,
        )
        .expect("encode type-21 payload");

        let (value, consumed) =
            decode_upstream_stream_payload(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                .expect("decode type-21 payload");
        assert_eq!(
            consumed,
            payload.len(),
            "decoder consumes the whole payload"
        );

        let RdbValue::Stream(_, _, _, _, _, decoded_max_deleted) = value else {
            panic!("expected a stream value");
        };
        assert_eq!(
            decoded_max_deleted, max_deleted,
            "max-deleted-entry-id must round-trip through type-21 persistence"
        );
    }
}
