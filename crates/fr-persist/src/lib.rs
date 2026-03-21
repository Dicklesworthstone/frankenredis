#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use fr_protocol::{RespFrame, RespParseError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofRecord {
    pub argv: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub enum PersistError {
    InvalidFrame,
    Parse(RespParseError),
    Io(std::io::Error),
}

impl PartialEq for PersistError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidFrame, Self::InvalidFrame) => true,
            (Self::Parse(a), Self::Parse(b)) => a == b,
            (Self::Io(_), Self::Io(_)) => false, // I/O errors are not structurally comparable
            _ => false,
        }
    }
}

impl Eq for PersistError {}

impl From<RespParseError> for PersistError {
    fn from(value: RespParseError) -> Self {
        Self::Parse(value)
    }
}

impl From<std::io::Error> for PersistError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl AofRecord {
    #[must_use]
    pub fn to_resp_frame(&self) -> RespFrame {
        let args = self
            .argv
            .iter()
            .map(|arg| RespFrame::BulkString(Some(arg.clone())))
            .collect();
        RespFrame::Array(Some(args))
    }

    pub fn from_resp_frame(frame: &RespFrame) -> Result<Self, PersistError> {
        let RespFrame::Array(Some(items)) = frame else {
            return Err(PersistError::InvalidFrame);
        };
        if items.is_empty() {
            return Err(PersistError::InvalidFrame);
        }
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            match item {
                RespFrame::BulkString(Some(bytes)) => argv.push(bytes.clone()),
                RespFrame::SimpleString(text) => argv.push(text.as_bytes().to_vec()),
                RespFrame::Integer(n) => argv.push(n.to_string().as_bytes().to_vec()),
                _ => return Err(PersistError::InvalidFrame),
            }
        }
        Ok(Self { argv })
    }
}

#[must_use]
pub fn encode_aof_stream(records: &[AofRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&record.to_resp_frame().to_bytes());
    }
    out
}

pub fn decode_aof_stream(input: &[u8]) -> Result<Vec<AofRecord>, PersistError> {
    let mut cursor = 0usize;
    let mut out = Vec::new();
    let parser_config = fr_protocol::ParserConfig {
        max_bulk_len: 1024 * 1024 * 1024, // 1GiB for AOF
        max_array_len: 10 * 1024 * 1024,  // 10M elements
        max_recursion_depth: 1024,
    };
    while cursor < input.len() {
        let parsed = fr_protocol::parse_frame_with_config(&input[cursor..], &parser_config)?;
        let record = AofRecord::from_resp_frame(&parsed.frame)?;
        out.push(record);
        cursor = cursor.saturating_add(parsed.consumed);
    }
    Ok(out)
}

/// Convert a list of command argv vectors (from `Store::to_aof_commands()`)
/// into `AofRecord` entries suitable for encoding.
#[must_use]
pub fn argv_to_aof_records(commands: Vec<Vec<Vec<u8>>>) -> Vec<AofRecord> {
    commands
        .into_iter()
        .map(|argv| AofRecord { argv })
        .collect()
}

/// Write AOF records to a file at the given path.
///
/// Writes atomically by first writing to a temporary file, then renaming.
/// This prevents corruption if the process crashes mid-write.
pub fn write_aof_file(path: &Path, records: &[AofRecord]) -> Result<(), PersistError> {
    let encoded = encode_aof_stream(records);
    let tmp_path = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, path)?;
    sync_parent_dir(path)?;
    Ok(())
}

/// Read and decode AOF records from a file at the given path.
///
/// Returns an empty vector if the file does not exist.
pub fn read_aof_file(path: &Path) -> Result<Vec<AofRecord>, PersistError> {
    match std::fs::read(path) {
        Ok(data) => {
            if data.is_empty() {
                return Ok(Vec::new());
            }
            decode_aof_stream(&data)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(PersistError::Io(e)),
    }
}

// ── RDB Snapshot Persistence ──────────────────────────────────────────

/// Redis RDB file format version we emit.
const RDB_VERSION: u32 = 11;

/// RDB opcodes.
const RDB_OPCODE_AUX: u8 = 0xFA;
const RDB_OPCODE_SELECTDB: u8 = 0xFE;
const RDB_OPCODE_RESIZEDB: u8 = 0xFB;
const RDB_OPCODE_EXPIRETIME_MS: u8 = 0xFC;
const RDB_OPCODE_EOF: u8 = 0xFF;

/// RDB value type tags.
const RDB_TYPE_STRING: u8 = 0;
const RDB_TYPE_LIST: u8 = 1;
const RDB_TYPE_SET: u8 = 2;
const RDB_TYPE_ZSET: u8 = 5;
const RDB_TYPE_HASH: u8 = 4;
const RDB_CHECKSUM_LEN: usize = 8;
const CRC64_REDIS_POLY: u64 = 0xAD93_D235_94C9_35A9;

/// A key-value entry for RDB serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbEntry {
    pub key: Vec<u8>,
    pub value: RdbValue,
    pub expire_ms: Option<u64>,
}

/// Value types supported in our RDB format.
#[derive(Debug, Clone, PartialEq)]
pub enum RdbValue {
    String(Vec<u8>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    SortedSet(Vec<(Vec<u8>, f64)>),
}

/// Encode an RDB length using Redis's variable-length encoding.
fn rdb_encode_length(buf: &mut Vec<u8>, len: usize) {
    if len < 64 {
        buf.push(len as u8);
    } else if len < 16384 {
        buf.push(0x40 | ((len >> 8) as u8));
        buf.push((len & 0xFF) as u8);
    } else if len <= u32::MAX as usize {
        buf.push(0x80);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        buf.push(0x81);
        buf.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

/// Encode a length-prefixed string (RDB string encoding).
fn rdb_encode_string(buf: &mut Vec<u8>, data: &[u8]) {
    rdb_encode_length(buf, data.len());
    buf.extend_from_slice(data);
}

fn crc64_redis(data: &[u8]) -> u64 {
    let mut crc = 0_u64;
    for &byte in data {
        crc ^= u64::from(byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= CRC64_REDIS_POLY;
            }
        }
    }
    crc
}

fn sync_parent_dir(path: &Path) -> Result<(), PersistError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let dir = std::fs::File::open(parent)?;
    dir.sync_all()?;
    Ok(())
}

/// Encode a complete RDB file from a set of entries.
#[must_use]
pub fn encode_rdb(entries: &[RdbEntry], aux: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Magic + version
    buf.extend_from_slice(b"REDIS");
    let version_str = format!("{RDB_VERSION:04}");
    buf.extend_from_slice(version_str.as_bytes());

    // Auxiliary fields (metadata like redis-ver, ctime, etc.)
    for (key, value) in aux {
        buf.push(RDB_OPCODE_AUX);
        rdb_encode_string(&mut buf, key.as_bytes());
        rdb_encode_string(&mut buf, value.as_bytes());
    }

    // Select DB 0
    buf.push(RDB_OPCODE_SELECTDB);
    rdb_encode_length(&mut buf, 0);

    // Resize DB hint
    let db_size = entries.len();
    let expires_size = entries.iter().filter(|e| e.expire_ms.is_some()).count();
    buf.push(RDB_OPCODE_RESIZEDB);
    rdb_encode_length(&mut buf, db_size);
    rdb_encode_length(&mut buf, expires_size);

    // Key-value pairs
    for entry in entries {
        // Expiry
        if let Some(ms) = entry.expire_ms {
            buf.push(RDB_OPCODE_EXPIRETIME_MS);
            buf.extend_from_slice(&ms.to_le_bytes());
        }

        // Type + key + value
        match &entry.value {
            RdbValue::String(v) => {
                buf.push(RDB_TYPE_STRING);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_string(&mut buf, v);
            }
            RdbValue::List(items) => {
                buf.push(RDB_TYPE_LIST);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, items.len());
                for item in items {
                    rdb_encode_string(&mut buf, item);
                }
            }
            RdbValue::Set(members) => {
                buf.push(RDB_TYPE_SET);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, members.len());
                for member in members {
                    rdb_encode_string(&mut buf, member);
                }
            }
            RdbValue::Hash(fields) => {
                buf.push(RDB_TYPE_HASH);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, fields.len());
                for (field, value) in fields {
                    rdb_encode_string(&mut buf, field);
                    rdb_encode_string(&mut buf, value);
                }
            }
            RdbValue::SortedSet(members) => {
                buf.push(RDB_TYPE_ZSET);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, members.len());
                for (member, score) in members {
                    rdb_encode_string(&mut buf, member);
                    // ZSET2 encoding: 8-byte LE double
                    buf.extend_from_slice(&score.to_le_bytes());
                }
            }
        }
    }

    // EOF
    buf.push(RDB_OPCODE_EOF);
    let checksum = crc64_redis(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());

    buf
}

/// Decode an RDB length. Returns `(length, bytes_consumed)` or `None` on
/// insufficient data. Note: this function returns `None` for special encodings (type 3)
/// since they represent string values, not lengths.
fn rdb_decode_length(data: &[u8]) -> Option<(usize, usize)> {
    let first = *data.first()?;
    let encoding = (first & 0xC0) >> 6;
    match encoding {
        0 => Some(((first & 0x3F) as usize, 1)),
        1 => {
            let second = *data.get(1)?;
            let len = (((first & 0x3F) as usize) << 8) | (second as usize);
            Some((len, 2))
        }
        2 => {
            if first == 0x80 {
                if data.len() < 5 {
                    return None;
                }
                let len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
                Some((len, 5))
            } else if first == 0x81 {
                if data.len() < 9 {
                    return None;
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&data[1..9]);
                let len = u64::from_be_bytes(bytes) as usize;
                Some((len, 9))
            } else {
                None // Unhandled special encodings
            }
        }
        _ => None, // Special encodings (type 3) are handled by rdb_decode_string
    }
}

/// Decode an RDB string. Returns `(bytes, consumed)` or `None`.
fn rdb_decode_string(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    let first = *data.first()?;
    let encoding = (first & 0xC0) >> 6;
    
    if encoding == 3 {
        // Special encoding (integers or LZF)
        match first & 0x3F {
            0 => {
                // 8-bit integer
                let val = *data.get(1)? as i8;
                Some((val.to_string().into_bytes(), 2))
            }
            1 => {
                // 16-bit integer
                if data.len() < 3 {
                    return None;
                }
                let val = i16::from_le_bytes([data[1], data[2]]);
                Some((val.to_string().into_bytes(), 3))
            }
            2 => {
                // 32-bit integer
                if data.len() < 5 {
                    return None;
                }
                let val = i32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                Some((val.to_string().into_bytes(), 5))
            }
            _ => None, // LZF or unknown special encoding not supported yet
        }
    } else {
        let (len, hdr) = rdb_decode_length(data)?;
        let end = hdr + len;
        if data.len() < end {
            return None;
        }
        Some((data[hdr..end].to_vec(), end))
    }
}

/// Decode an RDB file into entries. Returns entries and auxiliary metadata.
pub fn decode_rdb(data: &[u8]) -> Result<(Vec<RdbEntry>, BTreeMap<String, String>), PersistError> {
    if data.len() < 9 + RDB_CHECKSUM_LEN || &data[..5] != b"REDIS" {
        return Err(PersistError::InvalidFrame);
    }

    let version = std::str::from_utf8(&data[5..9]).map_err(|_| PersistError::InvalidFrame)?;
    if version != format!("{RDB_VERSION:04}") {
        return Err(PersistError::InvalidFrame);
    }
    let mut cursor = 9; // Skip "REDIS" + 4-digit version
    let mut entries = Vec::new();
    let mut aux = BTreeMap::new();
    let mut pending_expire_ms: Option<u64> = None;
    let mut saw_eof = false;

    while cursor < data.len() {
        let opcode = data[cursor];
        cursor += 1;

        // Reset expiry if this opcode is not a type byte and not a known expiry opcode.
        // This prevents 'leaking' an expiry to the next key if something unexpected happens.
        let is_type_byte = matches!(
            opcode,
            RDB_TYPE_STRING | RDB_TYPE_LIST | RDB_TYPE_SET | RDB_TYPE_HASH | RDB_TYPE_ZSET
        );
        let is_expiry_opcode = matches!(opcode, RDB_OPCODE_EXPIRETIME_MS | 0xFD);

        if !is_type_byte && !is_expiry_opcode && pending_expire_ms.is_some() {
            // In a well-formed RDB, expiry must be followed by a type byte.
            // If we see SELECTDB or something else here, the file is malformed.
            return Err(PersistError::InvalidFrame);
        }

        match opcode {
            RDB_OPCODE_EOF => {
                if data.len() != cursor + RDB_CHECKSUM_LEN {
                    return Err(PersistError::InvalidFrame);
                }
                let expected_checksum = u64::from_le_bytes(
                    data[cursor..cursor + RDB_CHECKSUM_LEN]
                        .try_into()
                        .map_err(|_| PersistError::InvalidFrame)?,
                );
                let actual_checksum = crc64_redis(&data[..cursor]);
                if expected_checksum != actual_checksum {
                    return Err(PersistError::InvalidFrame);
                }
                if pending_expire_ms.is_some() {
                    return Err(PersistError::InvalidFrame);
                }
                saw_eof = true;
                break;
            }
            RDB_OPCODE_AUX => {
                let (key, consumed) =
                    rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
                let (value, consumed) =
                    rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
                if let (Ok(k), Ok(v)) = (String::from_utf8(key), String::from_utf8(value)) {
                    aux.insert(k, v);
                }
            }
            RDB_OPCODE_SELECTDB => {
                let (_, consumed) =
                    rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
            }
            RDB_OPCODE_RESIZEDB => {
                let (_, consumed) =
                    rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
                let (_, consumed2) =
                    rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed2;
            }
            RDB_OPCODE_EXPIRETIME_MS => {
                if cursor + 8 > data.len() {
                    return Err(PersistError::InvalidFrame);
                }
                let ms = u64::from_le_bytes(
                    data[cursor..cursor + 8]
                        .try_into()
                        .map_err(|_| PersistError::InvalidFrame)?,
                );
                cursor += 8;
                pending_expire_ms = Some(ms);
            }
            0xFD => {
                // EXPIRETIME (seconds) — skip 4 bytes, convert to ms
                if cursor + 4 > data.len() {
                    return Err(PersistError::InvalidFrame);
                }
                let secs = u32::from_le_bytes(
                    data[cursor..cursor + 4]
                        .try_into()
                        .map_err(|_| PersistError::InvalidFrame)?,
                );
                cursor += 4;
                pending_expire_ms = Some(u64::from(secs) * 1000);
            }
            type_byte @ (RDB_TYPE_STRING | RDB_TYPE_LIST | RDB_TYPE_SET | RDB_TYPE_HASH
            | RDB_TYPE_ZSET) => {
                let (key, consumed) =
                    rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;

                let value = match type_byte {
                    RDB_TYPE_STRING => {
                        let (v, c) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        RdbValue::String(v)
                    }
                    RDB_TYPE_LIST => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut items = Vec::with_capacity(count);
                        for _ in 0..count {
                            let (item, c) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            items.push(item);
                        }
                        RdbValue::List(items)
                    }
                    RDB_TYPE_SET => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut members = Vec::with_capacity(count);
                        for _ in 0..count {
                            let (m, c) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            members.push(m);
                        }
                        RdbValue::Set(members)
                    }
                    RDB_TYPE_HASH => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut fields = Vec::with_capacity(count);
                        for _ in 0..count {
                            let (f, c1) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c1;
                            let (v, c2) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c2;
                            fields.push((f, v));
                        }
                        RdbValue::Hash(fields)
                    }
                    RDB_TYPE_ZSET => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut members = Vec::with_capacity(count);
                        for _ in 0..count {
                            let (m, c) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            if cursor + 8 > data.len() {
                                return Err(PersistError::InvalidFrame);
                            }
                            let score = f64::from_le_bytes(
                                data[cursor..cursor + 8]
                                    .try_into()
                                    .map_err(|_| PersistError::InvalidFrame)?,
                            );
                            cursor += 8;
                            members.push((m, score));
                        }
                        RdbValue::SortedSet(members)
                    }
                    _ => return Err(PersistError::InvalidFrame),
                };

                entries.push(RdbEntry {
                    key,
                    value,
                    expire_ms: pending_expire_ms.take(),
                });
            }
            _ => {
                // Unknown type — skip this entry (fail-closed for safety)
                return Err(PersistError::InvalidFrame);
            }
        }
    }

    if !saw_eof {
        return Err(PersistError::InvalidFrame);
    }

    Ok((entries, aux))
}

/// Write an RDB snapshot to a file. Uses atomic rename for crash safety.
pub fn write_rdb_file(
    path: &Path,
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
) -> Result<(), PersistError> {
    let encoded = encode_rdb(entries, aux);
    let tmp_path = path.with_extension("rdb.tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, path)?;
    sync_parent_dir(path)?;
    Ok(())
}

/// Read and decode an RDB file. Returns entries and auxiliary metadata.
pub fn read_rdb_file(
    path: &Path,
) -> Result<(Vec<RdbEntry>, BTreeMap<String, String>), PersistError> {
    match std::fs::read(path) {
        Ok(data) => {
            if data.is_empty() {
                return Err(PersistError::InvalidFrame);
            }
            decode_rdb(&data)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), BTreeMap::new())),
        Err(e) => Err(PersistError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use fr_protocol::{RespFrame, RespParseError};

    use super::{AofRecord, PersistError, decode_aof_stream, encode_aof_stream};

    #[test]
    fn round_trip_aof_record() {
        let record = AofRecord {
            argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
        };
        let frame = record.to_resp_frame();
        let decoded = AofRecord::from_resp_frame(&frame).expect("decode");
        assert_eq!(decoded, record);
    }

    #[test]
    fn invalid_frame_rejected() {
        let frame = RespFrame::BulkString(Some(b"x".to_vec()));
        assert!(AofRecord::from_resp_frame(&frame).is_err());
    }

    #[test]
    fn empty_array_record_rejected() {
        let frame = RespFrame::Array(Some(Vec::new()));
        let err = AofRecord::from_resp_frame(&frame).expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn round_trip_multi_record_stream() {
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
            },
        ];
        let encoded = encode_aof_stream(&records);
        let decoded = decode_aof_stream(&encoded).expect("decode stream");
        assert_eq!(decoded, records);
    }

    #[test]
    fn decode_rejects_invalid_stream_frame() {
        let err = decode_aof_stream(b"$3\r\nbad\r\n").expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn decode_rejects_empty_command_array_record() {
        let err = decode_aof_stream(b"*0\r\n").expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn decode_rejects_incomplete_stream() {
        let err = decode_aof_stream(b"*2\r\n$3\r\nGET\r\n$1\r\nk").expect_err("must fail");
        assert_eq!(err, PersistError::Parse(RespParseError::Incomplete));
    }

    #[test]
    fn argv_to_aof_records_converts() {
        let commands = vec![
            vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            vec![
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"v".to_vec(),
            ],
        ];
        let records = super::argv_to_aof_records(commands);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].argv[0], b"SET");
        assert_eq!(records[1].argv[0], b"HSET");
    }

    #[test]
    fn write_and_read_aof_file_round_trip() {
        let dir = std::env::temp_dir().join("fr_persist_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.aof");

        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"key1".to_vec(), b"val1".to_vec()],
            },
            AofRecord {
                argv: vec![
                    b"RPUSH".to_vec(),
                    b"list1".to_vec(),
                    b"a".to_vec(),
                    b"b".to_vec(),
                ],
            },
        ];

        super::write_aof_file(&path, &records).expect("write");
        let loaded = super::read_aof_file(&path).expect("read");
        assert_eq!(loaded, records);

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_aof_file_missing_returns_empty() {
        let path = std::path::Path::new("/tmp/fr_persist_nonexistent_test_file.aof");
        let loaded = super::read_aof_file(path).expect("read missing");
        assert!(loaded.is_empty());
    }

    #[test]
    fn sync_parent_dir_accepts_relative_paths() {
        super::sync_parent_dir(std::path::Path::new("relative-test.aof"))
            .expect("sync relative parent");
    }

    // ── RDB tests ────────────────────────────────────────────────────

    use super::{RDB_CHECKSUM_LEN, RdbEntry, RdbValue, decode_rdb, encode_rdb};

    #[test]
    fn rdb_round_trip_string() {
        let entries = vec![RdbEntry {
            key: b"hello".to_vec(),
            value: RdbValue::String(b"world".to_vec()),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _aux) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_with_expiry() {
        let entries = vec![RdbEntry {
            key: b"temp".to_vec(),
            value: RdbValue::String(b"val".to_vec()),
            expire_ms: Some(1_700_000_000_000),
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_list() {
        let entries = vec![RdbEntry {
            key: b"mylist".to_vec(),
            value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_set() {
        let entries = vec![RdbEntry {
            key: b"myset".to_vec(),
            value: RdbValue::Set(vec![b"x".to_vec(), b"y".to_vec()]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_hash() {
        let entries = vec![RdbEntry {
            key: b"myhash".to_vec(),
            value: RdbValue::Hash(vec![
                (b"f1".to_vec(), b"v1".to_vec()),
                (b"f2".to_vec(), b"v2".to_vec()),
            ]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_sorted_set() {
        let entries = vec![RdbEntry {
            key: b"myzset".to_vec(),
            value: RdbValue::SortedSet(vec![(b"alice".to_vec(), 1.5), (b"bob".to_vec(), 2.0)]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_aux_fields() {
        let entries = vec![RdbEntry {
            key: b"k".to_vec(),
            value: RdbValue::String(b"v".to_vec()),
            expire_ms: None,
        }];
        let aux = [("redis-ver", "7.0.0"), ("ctime", "1700000000")];
        let encoded = encode_rdb(&entries, &aux);
        let (decoded, aux_map) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
        assert_eq!(aux_map.get("redis-ver").map(String::as_str), Some("7.0.0"));
        assert_eq!(aux_map.get("ctime").map(String::as_str), Some("1700000000"));
    }

    #[test]
    fn rdb_round_trip_multiple_types() {
        let entries = vec![
            RdbEntry {
                key: b"str".to_vec(),
                value: RdbValue::String(b"hello".to_vec()),
                expire_ms: Some(9_999_999),
            },
            RdbEntry {
                key: b"lst".to_vec(),
                value: RdbValue::List(vec![b"1".to_vec(), b"2".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                key: b"hsh".to_vec(),
                value: RdbValue::Hash(vec![(b"a".to_vec(), b"b".to_vec())]),
                expire_ms: None,
            },
        ];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_rejects_invalid_magic() {
        assert!(decode_rdb(b"NOTREDIS").is_err());
    }

    #[test]
    fn rdb_rejects_checksum_mismatch() {
        let entries = vec![RdbEntry {
            key: b"tamper".to_vec(),
            value: RdbValue::String(b"proof".to_vec()),
            expire_ms: None,
        }];
        let mut encoded = encode_rdb(&entries, &[]);
        let len = encoded.len();
        encoded[len - 1] ^= 0xFF;

        assert!(decode_rdb(&encoded).is_err());
    }

    #[test]
    fn rdb_rejects_missing_eof_trailer() {
        let entries = vec![RdbEntry {
            key: b"missing".to_vec(),
            value: RdbValue::String(b"eof".to_vec()),
            expire_ms: None,
        }];
        let mut encoded = encode_rdb(&entries, &[]);
        encoded.truncate(encoded.len() - (1 + RDB_CHECKSUM_LEN));

        assert!(decode_rdb(&encoded).is_err());
    }

    #[test]
    fn rdb_rejects_unsupported_version() {
        let entries = vec![RdbEntry {
            key: b"version".to_vec(),
            value: RdbValue::String(b"mismatch".to_vec()),
            expire_ms: None,
        }];
        let mut encoded = encode_rdb(&entries, &[]);
        encoded[5..9].copy_from_slice(b"0010");

        assert!(decode_rdb(&encoded).is_err());
    }

    #[test]
    fn rdb_missing_file_returns_empty() {
        let path = std::path::Path::new("/tmp/fr_persist_nonexistent_test_file.rdb");
        let (entries, _) = super::read_rdb_file(path).expect("read missing");
        assert!(entries.is_empty());
    }

    #[test]
    fn rdb_existing_empty_file_is_rejected() {
        let dir = std::env::temp_dir().join("fr_persist_rdb_empty_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.rdb");
        std::fs::write(&path, []).expect("create empty rdb");

        let err = super::read_rdb_file(&path).expect_err("empty rdb must fail");
        assert_eq!(err, PersistError::InvalidFrame);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rdb_write_and_read_round_trip() {
        let dir = std::env::temp_dir().join("fr_persist_rdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.rdb");

        let entries = vec![
            RdbEntry {
                key: b"key1".to_vec(),
                value: RdbValue::String(b"val1".to_vec()),
                expire_ms: None,
            },
            RdbEntry {
                key: b"key2".to_vec(),
                value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec()]),
                expire_ms: Some(5_000_000),
            },
        ];

        super::write_rdb_file(&path, &entries, &[("redis-ver", "7.0.0")]).expect("write");
        let (loaded, aux) = super::read_rdb_file(&path).expect("read");
        assert_eq!(loaded, entries);
        assert_eq!(aux.get("redis-ver").map(String::as_str), Some("7.0.0"));

        let _ = std::fs::remove_file(&path);
    }
}
