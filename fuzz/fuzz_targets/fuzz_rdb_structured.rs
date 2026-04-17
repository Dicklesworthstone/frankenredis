#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use fr_persist::decode_rdb;

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
    WithExpiry {
        expiry_ms: u64,
        entry: Box<RdbEntryFuzz>,
    },
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

        // CRC64 placeholder (8 bytes of zeros - decoder may skip or validate)
        buf.extend_from_slice(&[0u8; 8]);

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
            RdbEntryFuzz::WithExpiry { .. } => {
                // Should not reach here - handled in encode()
            }
        }
    }
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
    let _ = decode_rdb(&encoded);
});
