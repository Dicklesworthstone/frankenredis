#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use fr_persist::{
    RdbStreamConsumerGroup, RdbStreamPendingEntry, StreamEntry, crc64_redis, decode_rdb,
    encode_upstream_stream_listpacks3_payload,
};

const RDB_MAGIC: &[u8] = b"REDIS";
const RDB_VERSION: &[u8] = b"0011";
const RDB_OPCODE_AUX: u8 = 0xFA;
const RDB_OPCODE_SELECTDB: u8 = 0xFE;
const RDB_OPCODE_RESIZEDB: u8 = 0xFB;
const RDB_OPCODE_EXPIRETIME_MS: u8 = 0xFC;
const RDB_OPCODE_EOF: u8 = 0xFF;

const RDB_TYPE_STRING: u8 = 0;
const RDB_TYPE_LIST: u8 = 1;
const RDB_TYPE_SET: u8 = 2;
const RDB_TYPE_HASH: u8 = 4;
const RDB_TYPE_ZSET_2: u8 = 5;
const RDB_TYPE_STREAM_LISTPACKS_3: u8 = 21;

#[derive(Debug, Arbitrary)]
struct FuzzRdb {
    aux_fields: Vec<AuxField>,
    db_index: u8,
    entries: Vec<RdbEntryFuzz>,
}

#[derive(Debug, Arbitrary)]
struct AuxField {
    key: SmallString,
    value: SmallString,
}

#[derive(Debug, Arbitrary)]
struct SmallString {
    data: Vec<u8>,
}

impl SmallString {
    fn encode(&self) -> Vec<u8> {
        let len = self.data.len().min(255);
        let mut out = Vec::new();
        out.push(len as u8);
        out.extend_from_slice(&self.data[..len]);
        out
    }
}

#[derive(Debug, Arbitrary)]
enum RdbEntryFuzz {
    String {
        key: SmallString,
        value: SmallString,
    },
    List {
        key: SmallString,
        items: Vec<SmallString>,
    },
    Set {
        key: SmallString,
        members: Vec<SmallString>,
    },
    Hash {
        key: SmallString,
        fields: Vec<(SmallString, SmallString)>,
    },
    ZSet {
        key: SmallString,
        members: Vec<(SmallString, f64)>,
    },
    Stream {
        key: SmallString,
        entries: Vec<StreamEntryFuzz>,
        group: Option<StreamGroupFuzz>,
    },
    WithExpiry {
        expiry_ms: u64,
        entry: Box<RdbEntryFuzz>,
    },
}

#[derive(Debug, Arbitrary)]
struct StreamEntryFuzz {
    ms: u16,
    seq: u8,
    fields: Vec<(SmallString, SmallString)>,
}

#[derive(Debug, Arbitrary)]
struct StreamGroupFuzz {
    name: SmallString,
    consumer: SmallString,
    pending_index: u8,
    deliveries: u8,
    last_delivered_ms: u32,
}

impl FuzzRdb {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Magic and version
        buf.extend_from_slice(RDB_MAGIC);
        buf.extend_from_slice(RDB_VERSION);

        // Aux fields
        for aux in &self.aux_fields {
            if aux.key.data.is_empty() || aux.value.data.is_empty() {
                continue;
            }
            buf.push(RDB_OPCODE_AUX);
            buf.extend_from_slice(&aux.key.encode());
            buf.extend_from_slice(&aux.value.encode());
        }

        // Select DB
        buf.push(RDB_OPCODE_SELECTDB);
        buf.push(self.db_index);

        // Resize DB hint (optional)
        buf.push(RDB_OPCODE_RESIZEDB);
        buf.push(self.entries.len().min(255) as u8);
        buf.push(0); // expires count

        // Entries
        for entry in &self.entries {
            entry.encode(&mut buf);
        }

        // EOF
        buf.push(RDB_OPCODE_EOF);

        let crc = crc64_redis(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        buf
    }
}

impl RdbEntryFuzz {
    fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            RdbEntryFuzz::WithExpiry { expiry_ms, entry } => {
                buf.push(RDB_OPCODE_EXPIRETIME_MS);
                buf.extend_from_slice(&expiry_ms.to_le_bytes());
                entry.encode_inner(buf);
            }
            other => other.encode_inner(buf),
        }
    }

    fn encode_inner(&self, buf: &mut Vec<u8>) {
        match self {
            RdbEntryFuzz::String { key, value } => {
                if key.data.is_empty() {
                    return;
                }
                buf.push(RDB_TYPE_STRING);
                buf.extend_from_slice(&key.encode());
                buf.extend_from_slice(&value.encode());
            }
            RdbEntryFuzz::List { key, items } => {
                if key.data.is_empty() {
                    return;
                }
                buf.push(RDB_TYPE_LIST);
                buf.extend_from_slice(&key.encode());
                buf.push(items.len().min(255) as u8);
                for item in items.iter().take(255) {
                    buf.extend_from_slice(&item.encode());
                }
            }
            RdbEntryFuzz::Set { key, members } => {
                if key.data.is_empty() {
                    return;
                }
                buf.push(RDB_TYPE_SET);
                buf.extend_from_slice(&key.encode());
                buf.push(members.len().min(255) as u8);
                for member in members.iter().take(255) {
                    buf.extend_from_slice(&member.encode());
                }
            }
            RdbEntryFuzz::Hash { key, fields } => {
                if key.data.is_empty() {
                    return;
                }
                buf.push(RDB_TYPE_HASH);
                buf.extend_from_slice(&key.encode());
                buf.push(fields.len().min(255) as u8);
                for (field, value) in fields.iter().take(255) {
                    buf.extend_from_slice(&field.encode());
                    buf.extend_from_slice(&value.encode());
                }
            }
            RdbEntryFuzz::ZSet { key, members } => {
                if key.data.is_empty() {
                    return;
                }
                buf.push(RDB_TYPE_ZSET_2);
                buf.extend_from_slice(&key.encode());
                buf.push(members.len().min(255) as u8);
                for (member, score) in members.iter().take(255) {
                    buf.extend_from_slice(&member.encode());
                    buf.extend_from_slice(&score.to_le_bytes());
                }
            }
            RdbEntryFuzz::Stream {
                key,
                entries,
                group,
            } => {
                if key.data.is_empty() {
                    return;
                }
                let stream_entries = build_stream_entries(entries);
                let watermark = stream_entries
                    .last()
                    .map(|entry| (entry.0, entry.1))
                    .or(Some((0, 0)));
                let groups = build_stream_groups(group.as_ref(), &stream_entries);
                let Some(payload) =
                    encode_upstream_stream_listpacks3_payload(&stream_entries, watermark, &groups)
                else {
                    return;
                };
                buf.push(RDB_TYPE_STREAM_LISTPACKS_3);
                let key = capped_non_empty(key, b"stream", 32);
                buf.push(key.len() as u8);
                buf.extend_from_slice(&key);
                buf.extend_from_slice(&payload);
            }
            RdbEntryFuzz::WithExpiry { .. } => {
                // Should not reach here - handled in encode()
            }
        }
    }
}

fn capped_non_empty(input: &SmallString, fallback: &[u8], max_len: usize) -> Vec<u8> {
    let capped = input.data.iter().take(max_len).copied().collect::<Vec<_>>();
    if capped.is_empty() {
        fallback.to_vec()
    } else {
        capped
    }
}

fn build_stream_entries(entries: &[StreamEntryFuzz]) -> Vec<StreamEntry> {
    let mut out = entries
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, entry)| {
            let fields = build_stream_fields(&entry.fields);
            (
                u64::from(entry.ms) + index as u64,
                u64::from(entry.seq),
                fields,
            )
        })
        .collect::<Vec<_>>();
    if out.is_empty() {
        out.push((1, 0, vec![(b"f".to_vec(), b"v".to_vec())]));
    }
    out.sort_by_key(|entry| (entry.0, entry.1));
    out
}

fn build_stream_fields(fields: &[(SmallString, SmallString)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = fields
        .iter()
        .take(4)
        .enumerate()
        .map(|(index, (field, value))| {
            let fallback_field = format!("f{index}");
            (
                capped_non_empty(field, fallback_field.as_bytes(), 32),
                capped_non_empty(value, b"v", 64),
            )
        })
        .collect::<Vec<_>>();
    if out.is_empty() {
        out.push((b"f".to_vec(), b"v".to_vec()));
    }
    out
}

fn build_stream_groups(
    group: Option<&StreamGroupFuzz>,
    entries: &[StreamEntry],
) -> Vec<RdbStreamConsumerGroup> {
    let Some(group) = group else {
        return Vec::new();
    };
    if entries.is_empty() {
        return Vec::new();
    }
    let consumer = capped_non_empty(&group.consumer, b"consumer", 32);
    let pending_entry = &entries[usize::from(group.pending_index) % entries.len()];
    vec![RdbStreamConsumerGroup {
        name: capped_non_empty(&group.name, b"group", 32),
        last_delivered_id_ms: pending_entry.0,
        last_delivered_id_seq: pending_entry.1,
        consumers: vec![consumer.clone()],
        pending: vec![RdbStreamPendingEntry {
            entry_id_ms: pending_entry.0,
            entry_id_seq: pending_entry.1,
            consumer,
            deliveries: u64::from(group.deliveries).saturating_add(1),
            last_delivered_ms: u64::from(group.last_delivered_ms),
        }],
    }]
}

fn upstream_loadback_is_required() -> bool {
    std::env::var_os("REQUIRE_UPSTREAM_RDB_LOAD").is_some()
}

fn redis_server_binary() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("REDIS_SERVER") {
        return path.into();
    }
    let repo_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let direct = repo_root.join("legacy_redis_code/redis/src/redis-server");
    if direct.exists() {
        return direct;
    }
    repo_root.join("../legacy_redis_code/redis/src/redis-server")
}

fn assert_upstream_loads_rdb(encoded: &[u8]) {
    let binary = redis_server_binary();
    assert!(
        binary.exists(),
        "REQUIRE_UPSTREAM_RDB_LOAD=1 but redis-server is missing at {}",
        binary.display()
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind upstream loadback port");
    let port = listener
        .local_addr()
        .expect("read upstream loadback port")
        .port();
    drop(listener);

    let timestamp_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_nanos();
    let data_dir = std::env::temp_dir().join(format!(
        "fr_fuzz_rdb_loadback_{}_{}_{}",
        std::process::id(),
        port,
        timestamp_nanos
    ));
    std::fs::create_dir_all(&data_dir).expect("create upstream loadback dir");
    std::fs::write(data_dir.join("dump.rdb"), encoded).expect("write upstream loadback dump");

    let mut child = std::process::Command::new(&binary)
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg("127.0.0.1")
        .arg("--dir")
        .arg(&data_dir)
        .arg("--dbfilename")
        .arg("dump.rdb")
        .arg("--appendonly")
        .arg("no")
        .arg("--save")
        .arg("")
        .arg("--daemonize")
        .arg("no")
        .arg("--protected-mode")
        .arg("no")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn redis-server for upstream RDB loadback");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll redis-server loadback") {
            panic!("redis-server rejected structured RDB during loadback: {status}");
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("redis-server did not become ready after structured RDB loadback");
}

fuzz_target!(|input: FuzzRdb| {
    // Limit complexity
    if input.aux_fields.len() > 10 || input.entries.len() > 50 {
        return;
    }

    let encoded = input.encode();
    if encoded.len() > 1_000_000 {
        return;
    }

    // Decode the structured input - should not panic
    if decode_rdb(&encoded).is_ok() && upstream_loadback_is_required() {
        assert_upstream_loads_rdb(&encoded);
    }
});
