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
    ManifestParseViolation { line: usize, reason: &'static str },
    ManifestPathViolation { line: usize, file_name: String },
}

impl PartialEq for PersistError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidFrame, Self::InvalidFrame) => true,
            (Self::Parse(a), Self::Parse(b)) => a == b,
            (Self::Io(_), Self::Io(_)) => false, // I/O errors are not structurally comparable
            (
                Self::ManifestParseViolation {
                    line: left_line,
                    reason: left_reason,
                },
                Self::ManifestParseViolation {
                    line: right_line,
                    reason: right_reason,
                },
            ) => left_line == right_line && left_reason == right_reason,
            (
                Self::ManifestPathViolation {
                    line: left_line,
                    file_name: left_file_name,
                },
                Self::ManifestPathViolation {
                    line: right_line,
                    file_name: right_file_name,
                },
            ) => left_line == right_line && left_file_name == right_file_name,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofManifestFileType {
    Base,
    History,
    Incremental,
}

impl AofManifestFileType {
    fn from_manifest_token(token: &str, line: usize) -> Result<Self, PersistError> {
        let bytes = token.as_bytes();
        if bytes.len() != 1 {
            return Err(manifest_parse_error(line, "invalid file type"));
        }

        match bytes[0] {
            b'b' => Ok(Self::Base),
            b'h' => Ok(Self::History),
            b'i' => Ok(Self::Incremental),
            _ => Err(manifest_parse_error(line, "unknown file type")),
        }
    }

    const fn as_manifest_char(self) -> char {
        match self {
            Self::Base => 'b',
            Self::History => 'h',
            Self::Incremental => 'i',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofManifestEntry {
    pub file_name: String,
    pub file_seq: u64,
    pub file_type: AofManifestFileType,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AofManifest {
    pub base: Option<AofManifestEntry>,
    pub history: Vec<AofManifestEntry>,
    pub incremental: Vec<AofManifestEntry>,
    pub curr_base_file_seq: u64,
    pub curr_incr_file_seq: u64,
}

impl AofManifest {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.base.is_none() && self.history.is_empty() && self.incremental.is_empty()
    }
}

const AOF_MANIFEST_MAX_LINE: usize = 1024;

fn manifest_parse_error(line: usize, reason: &'static str) -> PersistError {
    PersistError::ManifestParseViolation { line, reason }
}

#[must_use]
pub fn is_aof_manifest_basename(file_name: &str) -> bool {
    !file_name.is_empty() && !file_name.contains('/') && !file_name.contains('\\')
}

pub fn parse_aof_manifest(input: &str) -> Result<AofManifest, PersistError> {
    let mut manifest = AofManifest::default();
    let mut max_incr_seq = 0_u64;
    let mut saw_physical_line = false;

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        saw_physical_line = true;

        if raw_line.as_bytes().first() == Some(&b'#') {
            continue;
        }
        if raw_line.len() > AOF_MANIFEST_MAX_LINE {
            return Err(manifest_parse_error(line_number, "line too long"));
        }

        let line = raw_line.trim_matches(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'));
        if line.is_empty() {
            return Err(manifest_parse_error(line_number, "empty manifest row"));
        }

        let argv = split_manifest_args(line)
            .ok_or_else(|| manifest_parse_error(line_number, "invalid manifest quoting"))?;
        if argv.len() < 6 || !argv.len().is_multiple_of(2) {
            return Err(manifest_parse_error(line_number, "invalid field count"));
        }

        let entry = parse_aof_manifest_entry(&argv, line_number)?;
        match entry.file_type {
            AofManifestFileType::Base => {
                if manifest.base.is_some() {
                    return Err(manifest_parse_error(line_number, "duplicate base file"));
                }
                manifest.curr_base_file_seq = entry.file_seq;
                manifest.base = Some(entry);
            }
            AofManifestFileType::History => {
                manifest.history.push(entry);
            }
            AofManifestFileType::Incremental => {
                if entry.file_seq <= max_incr_seq {
                    return Err(manifest_parse_error(
                        line_number,
                        "non-monotonic incremental sequence",
                    ));
                }
                max_incr_seq = entry.file_seq;
                manifest.curr_incr_file_seq = entry.file_seq;
                manifest.incremental.push(entry);
            }
        }
    }

    if !saw_physical_line {
        return Err(manifest_parse_error(0, "empty manifest"));
    }

    Ok(manifest)
}

pub fn read_aof_manifest_file(path: &Path) -> Result<AofManifest, PersistError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_aof_manifest(&contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AofManifest::default()),
        Err(error) => Err(PersistError::Io(error)),
    }
}

#[must_use]
pub fn format_aof_manifest(manifest: &AofManifest) -> String {
    let mut out = String::new();
    if let Some(base) = &manifest.base {
        push_manifest_entry(&mut out, base);
    }
    for entry in &manifest.history {
        push_manifest_entry(&mut out, entry);
    }
    for entry in &manifest.incremental {
        push_manifest_entry(&mut out, entry);
    }
    out
}

fn parse_aof_manifest_entry(
    argv: &[String],
    line: usize,
) -> Result<AofManifestEntry, PersistError> {
    let mut file_name = None;
    let mut file_seq = None;
    let mut file_type = None;

    for pair in argv.chunks_exact(2) {
        let key = pair[0].as_str();
        let value = pair[1].as_str();
        if key.eq_ignore_ascii_case("file") {
            if file_name.replace(value.to_string()).is_some() {
                return Err(manifest_parse_error(line, "duplicate file field"));
            }
        } else if key.eq_ignore_ascii_case("seq") {
            if file_seq
                .replace(parse_manifest_sequence(value, line)?)
                .is_some()
            {
                return Err(manifest_parse_error(line, "duplicate seq field"));
            }
        } else if key.eq_ignore_ascii_case("type")
            && file_type
                .replace(AofManifestFileType::from_manifest_token(value, line)?)
                .is_some()
        {
            return Err(manifest_parse_error(line, "duplicate type field"));
        }
    }

    let file_name = file_name.ok_or_else(|| manifest_parse_error(line, "missing file field"))?;
    if !is_aof_manifest_basename(&file_name) {
        return Err(PersistError::ManifestPathViolation { line, file_name });
    }

    Ok(AofManifestEntry {
        file_name,
        file_seq: file_seq.ok_or_else(|| manifest_parse_error(line, "missing seq field"))?,
        file_type: file_type.ok_or_else(|| manifest_parse_error(line, "missing type field"))?,
    })
}

fn parse_manifest_sequence(value: &str, line: usize) -> Result<u64, PersistError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(manifest_parse_error(line, "invalid seq field"));
    }
    let seq = value
        .parse::<u64>()
        .map_err(|_| manifest_parse_error(line, "invalid seq field"))?;
    if seq == 0 {
        return Err(manifest_parse_error(line, "invalid seq field"));
    }
    Ok(seq)
}

fn split_manifest_args(line: &str) -> Option<Vec<String>> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }

        let mut arg = String::new();
        let mut quote = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if let Some(quote_byte) = quote {
                if byte == quote_byte {
                    quote = None;
                    cursor += 1;
                    continue;
                }
                if byte == b'\\' {
                    cursor += 1;
                    let escaped = *bytes.get(cursor)?;
                    arg.push(unescape_manifest_byte(escaped));
                    cursor += 1;
                    continue;
                }
                arg.push(char::from(byte));
                cursor += 1;
                continue;
            }

            if byte.is_ascii_whitespace() {
                break;
            }
            if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                cursor += 1;
                continue;
            }
            if byte == b'\\' {
                cursor += 1;
                let escaped = *bytes.get(cursor)?;
                arg.push(unescape_manifest_byte(escaped));
                cursor += 1;
                continue;
            }
            arg.push(char::from(byte));
            cursor += 1;
        }

        if quote.is_some() {
            return None;
        }
        args.push(arg);
    }

    Some(args)
}

fn unescape_manifest_byte(byte: u8) -> char {
    match byte {
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        other => char::from(other),
    }
}

fn push_manifest_entry(out: &mut String, entry: &AofManifestEntry) {
    out.push_str("file ");
    out.push_str(&format_manifest_file_name(&entry.file_name));
    out.push_str(" seq ");
    out.push_str(&entry.file_seq.to_string());
    out.push_str(" type ");
    out.push(entry.file_type.as_manifest_char());
    out.push('\n');
}

fn format_manifest_file_name(file_name: &str) -> String {
    if file_name
        .bytes()
        .all(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'"' | b'\'' | b'\\'))
    {
        return file_name.to_string();
    }

    let mut out = String::from("\"");
    for byte in file_name.bytes() {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            other => out.push(char::from(other)),
        }
    }
    out.push('"');
    out
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
const RDB_TYPE_ZSET_2: u8 = 5; // Binary LE double scores (our encoding)
const RDB_TYPE_HASH: u8 = 4;
const RDB_TYPE_STREAM: u8 = 15; // FrankenRedis stream encoding
const RDB_CHECKSUM_LEN: usize = 8;
const CRC64_REDIS_POLY: u64 = 0xAD93_D235_94C9_35A9;

/// A key-value entry for RDB serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbEntry {
    pub db: usize,
    pub key: Vec<u8>,
    pub value: RdbValue,
    pub expire_ms: Option<u64>,
}

/// Stream entry: (ms, seq, fields).
pub type StreamEntry = (u64, u64, Vec<(Vec<u8>, Vec<u8>)>);

/// A pending entry in a consumer group (PEL entry).
#[derive(Debug, Clone, PartialEq)]
pub struct RdbStreamPendingEntry {
    pub entry_id_ms: u64,
    pub entry_id_seq: u64,
    pub consumer: Vec<u8>,
    pub deliveries: u64,
    pub last_delivered_ms: u64,
}

/// A consumer group persisted in an RDB snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbStreamConsumerGroup {
    pub name: Vec<u8>,
    pub last_delivered_id_ms: u64,
    pub last_delivered_id_seq: u64,
    pub consumers: Vec<Vec<u8>>,
    pub pending: Vec<RdbStreamPendingEntry>,
}

/// Value types supported in our RDB format.
#[derive(Debug, Clone, PartialEq)]
pub enum RdbValue {
    String(Vec<u8>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    SortedSet(Vec<(Vec<u8>, f64)>),
    /// Stream: entries + optional watermark + consumer groups.
    Stream(
        Vec<StreamEntry>,
        Option<(u64, u64)>,
        Vec<RdbStreamConsumerGroup>,
    ),
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

pub fn crc64_redis(data: &[u8]) -> u64 {
    fn reflect(mut data: u64, bit_len: usize) -> u64 {
        let mut reflected = data & 1;
        for _ in 1..bit_len {
            data >>= 1;
            reflected = (reflected << 1) | (data & 1);
        }
        reflected
    }

    let mut crc = 0_u64;
    for &byte in data {
        let mut mask = 0x01_u8;
        while mask != 0 {
            let mut bit_set = (crc & 0x8000_0000_0000_0000) != 0;
            if (byte & mask) != 0 {
                bit_set = !bit_set;
            }
            crc <<= 1;
            if bit_set {
                crc ^= CRC64_REDIS_POLY;
            }
            mask = mask.wrapping_shl(1);
        }
        crc &= u64::MAX;
    }
    reflect(crc, 64)
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

    let mut sorted_entries: Vec<&RdbEntry> = entries.iter().collect();
    sorted_entries.sort_by(|left, right| {
        left.db
            .cmp(&right.db)
            .then_with(|| left.key.cmp(&right.key))
    });

    let mut current_db: Option<usize> = None;
    for (index, entry) in sorted_entries.iter().enumerate() {
        let needs_db_header = current_db != Some(entry.db);
        if needs_db_header {
            current_db = Some(entry.db);
            buf.push(RDB_OPCODE_SELECTDB);
            rdb_encode_length(&mut buf, entry.db);
            let db_entries = sorted_entries
                .iter()
                .filter(|candidate| candidate.db == entry.db)
                .count();
            let db_expires = sorted_entries
                .iter()
                .filter(|candidate| candidate.db == entry.db && candidate.expire_ms.is_some())
                .count();
            buf.push(RDB_OPCODE_RESIZEDB);
            rdb_encode_length(&mut buf, db_entries);
            rdb_encode_length(&mut buf, db_expires);
        }

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
                buf.push(RDB_TYPE_ZSET_2);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, members.len());
                for (member, score) in members {
                    rdb_encode_string(&mut buf, member);
                    // ZSET2 encoding: 8-byte LE double
                    buf.extend_from_slice(&score.to_le_bytes());
                }
            }
            RdbValue::Stream(stream_entries, watermark, groups) => {
                buf.push(RDB_TYPE_STREAM);
                rdb_encode_string(&mut buf, &entry.key);
                let (wm_ms, wm_seq) = watermark.unwrap_or((0, 0));
                buf.extend_from_slice(&wm_ms.to_le_bytes());
                buf.extend_from_slice(&wm_seq.to_le_bytes());
                rdb_encode_length(&mut buf, stream_entries.len());
                for (ms, seq, fields) in stream_entries {
                    buf.extend_from_slice(&ms.to_le_bytes());
                    buf.extend_from_slice(&seq.to_le_bytes());
                    rdb_encode_length(&mut buf, fields.len());
                    for (fname, fval) in fields {
                        rdb_encode_string(&mut buf, fname);
                        rdb_encode_string(&mut buf, fval);
                    }
                }
                // Consumer groups
                rdb_encode_length(&mut buf, groups.len());
                for group in groups {
                    rdb_encode_string(&mut buf, &group.name);
                    buf.extend_from_slice(&group.last_delivered_id_ms.to_le_bytes());
                    buf.extend_from_slice(&group.last_delivered_id_seq.to_le_bytes());
                    rdb_encode_length(&mut buf, group.consumers.len());
                    for consumer in &group.consumers {
                        rdb_encode_string(&mut buf, consumer);
                    }
                    rdb_encode_length(&mut buf, group.pending.len());
                    for pe in &group.pending {
                        buf.extend_from_slice(&pe.entry_id_ms.to_le_bytes());
                        buf.extend_from_slice(&pe.entry_id_seq.to_le_bytes());
                        rdb_encode_string(&mut buf, &pe.consumer);
                        buf.extend_from_slice(&pe.deliveries.to_le_bytes());
                        buf.extend_from_slice(&pe.last_delivered_ms.to_le_bytes());
                    }
                }
            }
        }
        debug_assert!(index < sorted_entries.len());
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
fn lzf_decompress(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    // Redis max string size is 512MB (536_870_912 bytes).
    // Reject anything larger to prevent OOM via malicious RDB headers.
    if expected_len > 536_870_912 {
        return None;
    }
    // Cap initial allocation to avoid OOM from malicious RDB payloads.
    let mut output = Vec::with_capacity(expected_len.min(8192));
    let mut cursor = 0usize;

    while cursor < input.len() && output.len() < expected_len {
        let ctrl = usize::from(*input.get(cursor)?);
        cursor += 1;

        if ctrl < 32 {
            let literal_len = ctrl + 1;
            let end = cursor.checked_add(literal_len)?;
            let literal = input.get(cursor..end)?;
            output.extend_from_slice(literal);
            cursor = end;
            continue;
        }

        let mut copy_len = (ctrl >> 5) + 2;
        if copy_len == 9 {
            copy_len = copy_len.checked_add(usize::from(*input.get(cursor)?))?;
            cursor += 1;
        }

        let backref_low = usize::from(*input.get(cursor)?);
        cursor += 1;
        let backref = (((ctrl & 0x1F) << 8) | backref_low) + 1;
        if backref > output.len() {
            return None;
        }

        let copy_start = output.len() - backref;
        for idx in 0..copy_len {
            let byte = *output.get(copy_start + idx)?;
            output.push(byte);
        }
    }

    if cursor == input.len() && output.len() == expected_len {
        Some(output)
    } else {
        None
    }
}

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
            3 => {
                let (compressed_len, compressed_hdr) = rdb_decode_length(&data[1..])?;
                let (uncompressed_len, uncompressed_hdr) =
                    rdb_decode_length(&data[1 + compressed_hdr..])?;
                let payload_start = 1 + compressed_hdr + uncompressed_hdr;
                let payload_end = payload_start.checked_add(compressed_len)?;
                let compressed = data.get(payload_start..payload_end)?;
                let decompressed = lzf_decompress(compressed, uncompressed_len)?;
                Some((decompressed, payload_end))
            }
            _ => None,
        }
    } else {
        let (len, hdr) = rdb_decode_length(data)?;
        let end = hdr.checked_add(len)?;
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
    let mut current_db = 0usize;
    let mut saw_eof = false;

    while cursor < data.len() {
        let opcode = data[cursor];
        cursor += 1;

        // Reset expiry if this opcode is not a type byte and not a known expiry opcode.
        // This prevents 'leaking' an expiry to the next key if something unexpected happens.
        let is_type_byte = matches!(
            opcode,
            RDB_TYPE_STRING
                | RDB_TYPE_LIST
                | RDB_TYPE_SET
                | RDB_TYPE_HASH
                | RDB_TYPE_ZSET_2
                | RDB_TYPE_STREAM
        );
        let is_expiry_opcode = matches!(opcode, RDB_OPCODE_EXPIRETIME_MS | 0xFD);
        let is_eviction_opcode = matches!(opcode, 0xF8 | 0xF9);

        if !is_type_byte && !is_expiry_opcode && !is_eviction_opcode && pending_expire_ms.is_some()
        {
            // In a well-formed RDB, expiry/eviction data must be followed by a type byte.
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
                // Use lossy conversion to preserve AUX metadata even with
                // non-UTF8 bytes rather than silently discarding fields.
                let k = String::from_utf8_lossy(&key).into_owned();
                let v = String::from_utf8_lossy(&value).into_owned();
                aux.insert(k, v);
            }
            RDB_OPCODE_SELECTDB => {
                let (db, consumed) =
                    rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
                current_db = db;
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
            0xF8 => {
                // RDB_OPCODE_IDLE
                let (_, consumed) =
                    rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
            }
            0xF9 => {
                // RDB_OPCODE_FREQ
                if cursor >= data.len() {
                    return Err(PersistError::InvalidFrame);
                }
                cursor += 1;
            }
            type_byte @ (RDB_TYPE_STRING | RDB_TYPE_LIST | RDB_TYPE_SET | RDB_TYPE_HASH
            | RDB_TYPE_ZSET_2 | RDB_TYPE_STREAM) => {
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
                        let mut items = Vec::with_capacity(count.min(1024));
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
                        let mut members = Vec::with_capacity(count.min(1024));
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
                        let mut fields = Vec::with_capacity(count.min(1024));
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
                    RDB_TYPE_ZSET_2 => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut members = Vec::with_capacity(count.min(1024));
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
                    RDB_TYPE_STREAM => {
                        // Decode watermark (16 bytes)
                        if cursor + 16 > data.len() {
                            return Err(PersistError::InvalidFrame);
                        }
                        let wm_ms = u64::from_le_bytes(
                            data[cursor..cursor + 8]
                                .try_into()
                                .map_err(|_| PersistError::InvalidFrame)?,
                        );
                        cursor += 8;
                        let wm_seq = u64::from_le_bytes(
                            data[cursor..cursor + 8]
                                .try_into()
                                .map_err(|_| PersistError::InvalidFrame)?,
                        );
                        cursor += 8;
                        let watermark = if wm_ms == 0 && wm_seq == 0 {
                            None
                        } else {
                            Some((wm_ms, wm_seq))
                        };
                        let (count, consumed) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let mut stream_entries = Vec::with_capacity(count.min(1024));
                        for _ in 0..count {
                            if cursor + 16 > data.len() {
                                return Err(PersistError::InvalidFrame);
                            }
                            let ms = u64::from_le_bytes(
                                data[cursor..cursor + 8]
                                    .try_into()
                                    .map_err(|_| PersistError::InvalidFrame)?,
                            );
                            cursor += 8;
                            let seq = u64::from_le_bytes(
                                data[cursor..cursor + 8]
                                    .try_into()
                                    .map_err(|_| PersistError::InvalidFrame)?,
                            );
                            cursor += 8;
                            let (field_count, fc) = rdb_decode_length(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += fc;
                            let mut fields = Vec::with_capacity(field_count.min(1024));
                            for _ in 0..field_count {
                                let (fname, c1) = rdb_decode_string(&data[cursor..])
                                    .ok_or(PersistError::InvalidFrame)?;
                                cursor += c1;
                                let (fval, c2) = rdb_decode_string(&data[cursor..])
                                    .ok_or(PersistError::InvalidFrame)?;
                                cursor += c2;
                                fields.push((fname, fval));
                            }
                            stream_entries.push((ms, seq, fields));
                        }
                        // Decode consumer groups (always present in stream encoding).
                        let (group_count, gc) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += gc;
                        let mut groups = Vec::with_capacity(group_count.min(256));
                        for _ in 0..group_count {
                            let (name, nc) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += nc;
                            if cursor + 16 > data.len() {
                                return Err(PersistError::InvalidFrame);
                            }
                            let ld_ms = u64::from_le_bytes(
                                data[cursor..cursor + 8]
                                    .try_into()
                                    .map_err(|_| PersistError::InvalidFrame)?,
                            );
                            cursor += 8;
                            let ld_seq = u64::from_le_bytes(
                                data[cursor..cursor + 8]
                                    .try_into()
                                    .map_err(|_| PersistError::InvalidFrame)?,
                            );
                            cursor += 8;
                            // Consumers list
                            let (consumer_count, cc) = rdb_decode_length(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += cc;
                            let mut consumers = Vec::with_capacity(consumer_count.min(256));
                            for _ in 0..consumer_count {
                                let (cname, cnc) = rdb_decode_string(&data[cursor..])
                                    .ok_or(PersistError::InvalidFrame)?;
                                cursor += cnc;
                                consumers.push(cname);
                            }
                            // Pending entries
                            let (pel_count, pc) = rdb_decode_length(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += pc;
                            let mut pending = Vec::with_capacity(pel_count.min(4096));
                            for _ in 0..pel_count {
                                if cursor + 16 > data.len() {
                                    return Err(PersistError::InvalidFrame);
                                }
                                let eid_ms = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                let eid_seq = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                let (pe_consumer, pec) = rdb_decode_string(&data[cursor..])
                                    .ok_or(PersistError::InvalidFrame)?;
                                cursor += pec;
                                if cursor + 16 > data.len() {
                                    return Err(PersistError::InvalidFrame);
                                }
                                let deliveries = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                let last_del_ms = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                pending.push(RdbStreamPendingEntry {
                                    entry_id_ms: eid_ms,
                                    entry_id_seq: eid_seq,
                                    consumer: pe_consumer,
                                    deliveries,
                                    last_delivered_ms: last_del_ms,
                                });
                            }
                            groups.push(RdbStreamConsumerGroup {
                                name,
                                last_delivered_id_ms: ld_ms,
                                last_delivered_id_seq: ld_seq,
                                consumers,
                                pending,
                            });
                        }
                        RdbValue::Stream(stream_entries, watermark, groups)
                    }
                    _ => return Err(PersistError::InvalidFrame),
                };

                entries.push(RdbEntry {
                    db: current_db,
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

    use super::{
        AofManifest, AofManifestFileType, AofRecord, PersistError, decode_aof_stream,
        encode_aof_stream, format_aof_manifest, parse_aof_manifest,
    };

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

    #[test]
    fn aof_manifest_parses_base_history_and_incremental_rows() {
        let manifest = parse_aof_manifest(
            "# generated by Redis\n\
             file appendonly.aof.1.base.rdb seq 1 type b\n\
             file appendonly.aof.2.incr.aof seq 2 type h\n\
             file appendonly.aof.3.incr.aof seq 3 type i\n\
             file appendonly.aof.4.incr.aof seq 4 type i\n",
        )
        .expect("parse manifest");

        let base = manifest.base.as_ref().expect("base entry");
        assert_eq!(base.file_name, "appendonly.aof.1.base.rdb");
        assert_eq!(base.file_seq, 1);
        assert_eq!(base.file_type, AofManifestFileType::Base);
        assert_eq!(manifest.history.len(), 1);
        assert_eq!(manifest.incremental.len(), 2);
        assert_eq!(manifest.curr_base_file_seq, 1);
        assert_eq!(manifest.curr_incr_file_seq, 4);
    }

    #[test]
    fn aof_manifest_format_preserves_redis_ordering() {
        let parsed = parse_aof_manifest(
            "file base.aof seq 1 type b\n\
             file old.aof seq 2 type h\n\
             file \"incr 3.aof\" seq 3 type i\n",
        )
        .expect("parse manifest");

        let formatted = format_aof_manifest(&parsed);
        assert_eq!(
            formatted,
            "file base.aof seq 1 type b\n\
             file old.aof seq 2 type h\n\
             file \"incr 3.aof\" seq 3 type i\n"
        );
        assert_eq!(parse_aof_manifest(&formatted).expect("reparse"), parsed);
    }

    #[test]
    fn aof_manifest_empty_input_is_rejected_but_missing_file_is_empty() {
        let err = parse_aof_manifest("").expect_err("empty manifest must fail");
        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 0,
                reason: "empty manifest",
            }
        );

        let missing = super::read_aof_manifest_file(std::path::Path::new(
            "/tmp/fr_persist_missing_manifest_for_test.manifest",
        ))
        .expect("missing manifest");
        assert_eq!(missing, AofManifest::default());
        assert!(missing.is_empty());
    }

    #[test]
    fn aof_manifest_rejects_duplicate_base() {
        let err = parse_aof_manifest(
            "file base-1.aof seq 1 type b\n\
             file base-2.aof seq 2 type b\n",
        )
        .expect_err("duplicate base must fail");

        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 2,
                reason: "duplicate base file",
            }
        );
    }

    #[test]
    fn aof_manifest_rejects_path_style_filename() {
        let err = parse_aof_manifest("file ../appendonly.aof seq 1 type b\n")
            .expect_err("path filename must fail");

        assert_eq!(
            err,
            PersistError::ManifestPathViolation {
                line: 1,
                file_name: "../appendonly.aof".to_string(),
            }
        );
    }

    #[test]
    fn aof_manifest_rejects_non_monotonic_incremental_sequences() {
        let err = parse_aof_manifest(
            "file appendonly.aof.3.incr.aof seq 3 type i\n\
             file appendonly.aof.2.incr.aof seq 2 type i\n",
        )
        .expect_err("non-monotonic incr must fail");

        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 2,
                reason: "non-monotonic incremental sequence",
            }
        );
    }

    #[test]
    fn aof_manifest_rejects_malformed_rows() {
        let err =
            parse_aof_manifest("file appendonly.aof seq 1\n").expect_err("missing type must fail");
        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 1,
                reason: "invalid field count",
            }
        );

        let err = parse_aof_manifest("file appendonly.aof seq 01 type i\n")
            .expect_err("leading-zero seq must fail");
        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 1,
                reason: "invalid seq field",
            }
        );

        let err = parse_aof_manifest("file appendonly.aof seq x type i\n")
            .expect_err("nonnumeric seq must fail");
        assert_eq!(
            err,
            PersistError::ManifestParseViolation {
                line: 1,
                reason: "invalid seq field",
            }
        );
    }

    // ── RDB tests ────────────────────────────────────────────────────

    use super::{
        RDB_CHECKSUM_LEN, RDB_OPCODE_EOF, RDB_TYPE_STRING, RdbEntry, RdbStreamConsumerGroup,
        RdbStreamPendingEntry, RdbValue, crc64_redis, decode_rdb, encode_rdb, lzf_decompress,
        rdb_encode_length, rdb_encode_string,
    };

    #[test]
    fn lzf_decompresses_literal_runs() {
        let compressed = [4, b'h', b'e', b'l', b'l', b'o'];
        let decompressed = lzf_decompress(&compressed, 5).expect("literal decode");
        assert_eq!(decompressed, b"hello");
    }

    #[test]
    fn lzf_decompresses_back_references() {
        let compressed = [2, b'a', b'b', b'c', 0x20, 0x02];
        let decompressed = lzf_decompress(&compressed, 6).expect("backref decode");
        assert_eq!(decompressed, b"abcabc");
    }

    #[test]
    fn rdb_round_trip_string() {
        let entries = vec![RdbEntry {
            db: 0,
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
            db: 0,
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
            db: 0,
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
            db: 0,
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
            db: 0,
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
            db: 0,
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
            db: 0,
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
                db: 0,
                key: b"str".to_vec(),
                value: RdbValue::String(b"hello".to_vec()),
                expire_ms: Some(9_999_999),
            },
            RdbEntry {
                db: 2,
                key: b"hsh".to_vec(),
                value: RdbValue::Hash(vec![(b"a".to_vec(), b"b".to_vec())]),
                expire_ms: None,
            },
            RdbEntry {
                db: 2,
                key: b"lst".to_vec(),
                value: RdbValue::List(vec![b"1".to_vec(), b"2".to_vec()]),
                expire_ms: None,
            },
        ];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_decodes_lzf_encoded_string_values() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"REDIS0011");
        encoded.push(RDB_TYPE_STRING);
        rdb_encode_string(&mut encoded, b"msg");
        encoded.push(0xC3);
        rdb_encode_length(&mut encoded, 6);
        rdb_encode_length(&mut encoded, 6);
        encoded.extend_from_slice(&[2, b'a', b'b', b'c', 0x20, 0x02]);
        encoded.push(RDB_OPCODE_EOF);
        let checksum = crc64_redis(&encoded);
        encoded.extend_from_slice(&checksum.to_le_bytes());

        let (decoded, aux) = decode_rdb(&encoded).expect("decode lzf rdb");
        assert!(aux.is_empty());
        assert_eq!(
            decoded,
            vec![RdbEntry {
                db: 0,
                key: b"msg".to_vec(),
                value: RdbValue::String(b"abcabc".to_vec()),
                expire_ms: None,
            }]
        );
    }

    #[test]
    fn crc64_matches_redis_reference_vector() {
        assert_eq!(crc64_redis(b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }

    #[test]
    fn rdb_rejects_invalid_magic() {
        assert!(decode_rdb(b"NOTREDIS").is_err());
    }

    #[test]
    fn rdb_rejects_checksum_mismatch() {
        let entries = vec![RdbEntry {
            db: 0,
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
            db: 0,
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
            db: 0,
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
    fn rdb_round_trip_stream() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"mystream".to_vec(),
            value: RdbValue::Stream(
                vec![
                    (
                        1000,
                        0,
                        vec![
                            (b"name".to_vec(), b"Alice".to_vec()),
                            (b"age".to_vec(), b"30".to_vec()),
                        ],
                    ),
                    (1001, 0, vec![(b"name".to_vec(), b"Bob".to_vec())]),
                ],
                Some((1001, 0)),
                Vec::new(),
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_stream_no_watermark() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"emptystream".to_vec(),
            value: RdbValue::Stream(vec![], None, Vec::new()),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_stream_with_expiry() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"tempstream".to_vec(),
            value: RdbValue::Stream(
                vec![(5000, 1, vec![(b"field".to_vec(), b"value".to_vec())])],
                Some((5000, 1)),
                Vec::new(),
            ),
            expire_ms: Some(9_999_999),
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_round_trip_stream_with_consumer_groups() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"cg_stream".to_vec(),
            value: RdbValue::Stream(
                vec![
                    (1000, 0, vec![(b"msg".to_vec(), b"hello".to_vec())]),
                    (1001, 0, vec![(b"msg".to_vec(), b"world".to_vec())]),
                ],
                Some((1001, 0)),
                vec![RdbStreamConsumerGroup {
                    name: b"mygroup".to_vec(),
                    last_delivered_id_ms: 1001,
                    last_delivered_id_seq: 0,
                    consumers: vec![b"alice".to_vec(), b"bob".to_vec()],
                    pending: vec![
                        RdbStreamPendingEntry {
                            entry_id_ms: 1000,
                            entry_id_seq: 0,
                            consumer: b"alice".to_vec(),
                            deliveries: 2,
                            last_delivered_ms: 5000,
                        },
                        RdbStreamPendingEntry {
                            entry_id_ms: 1001,
                            entry_id_seq: 0,
                            consumer: b"bob".to_vec(),
                            deliveries: 1,
                            last_delivered_ms: 6000,
                        },
                    ],
                }],
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_stream_decode_rejects_missing_group_count() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"cg_stream".to_vec(),
            value: RdbValue::Stream(
                vec![(1000, 0, vec![(b"msg".to_vec(), b"hello".to_vec())])],
                Some((1000, 0)),
                Vec::new(),
            ),
            expire_ms: None,
        }];
        let mut encoded = encode_rdb(&entries, &[]);
        // Remove the final consumer group length byte (single 0x00 for empty groups)
        // to simulate a truncated stream payload.
        encoded.pop();
        assert!(decode_rdb(&encoded).is_err());
    }

    #[test]
    fn rdb_round_trip_all_types_together() {
        // Entries sorted by key alphabetically (encode_rdb sorts within each db).
        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"hsh".to_vec(),
                value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"lst".to_vec(),
                value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"st".to_vec(),
                value: RdbValue::Set(vec![b"x".to_vec(), b"y".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"str".to_vec(),
                value: RdbValue::String(b"hello".to_vec()),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"strm".to_vec(),
                value: RdbValue::Stream(
                    vec![(100, 0, vec![(b"k".to_vec(), b"v".to_vec())])],
                    Some((100, 0)),
                    Vec::new(),
                ),
                expire_ms: Some(1_000_000),
            },
            RdbEntry {
                db: 0,
                key: b"zst".to_vec(),
                value: RdbValue::SortedSet(vec![(b"m".to_vec(), 2.5)]),
                expire_ms: None,
            },
        ];
        let encoded = encode_rdb(&entries, &[("redis-ver", "7.2.0")]);
        let (decoded, aux) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
        assert_eq!(aux.get("redis-ver").map(String::as_str), Some("7.2.0"));
    }

    #[test]
    fn rdb_write_and_read_round_trip() {
        let dir = std::env::temp_dir().join("fr_persist_rdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.rdb");

        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"key1".to_vec(),
                value: RdbValue::String(b"val1".to_vec()),
                expire_ms: None,
            },
            RdbEntry {
                db: 3,
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

    // ── Golden artifact tests ──────────────────────────────────────────
    // These freeze exact byte sequences to catch accidental format changes.

    mod golden {
        use super::*;

        /// Golden test: AOF SET command encoding must produce exact RESP bytes.
        #[test]
        fn golden_aof_set_command() {
            let record = AofRecord {
                argv: vec![b"SET".to_vec(), b"key".to_vec(), b"value".to_vec()],
            };
            let encoded = encode_aof_stream(&[record]);
            let golden = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
            assert_eq!(
                encoded,
                golden.as_slice(),
                "AOF SET command encoding changed"
            );
        }

        /// Golden test: AOF multi-command stream encoding.
        #[test]
        fn golden_aof_multi_command() {
            let records = vec![
                AofRecord {
                    argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
                },
                AofRecord {
                    argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
                },
            ];
            let encoded = encode_aof_stream(&records);
            let golden =
                b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n*2\r\n$4\r\nINCR\r\n$7\r\ncounter\r\n";
            assert_eq!(
                encoded,
                golden.as_slice(),
                "AOF multi-command encoding changed"
            );
        }

        /// Golden test: RDB magic header must be exactly "REDIS" + version.
        #[test]
        fn golden_rdb_magic_header() {
            let encoded = encode_rdb(&[], &[]);
            assert!(
                encoded.starts_with(b"REDIS0011"),
                "RDB magic header must start with REDIS0011"
            );
        }

        /// Golden test: empty RDB must have magic + EOF + checksum.
        #[test]
        fn golden_rdb_empty() {
            let encoded = encode_rdb(&[], &[]);
            // REDIS0011 (9 bytes) + EOF opcode (1 byte) + CRC64 checksum (8 bytes)
            assert_eq!(encoded.len(), 18, "Empty RDB should be 18 bytes");
            assert_eq!(&encoded[..9], b"REDIS0011", "RDB header must be REDIS0011");
            assert_eq!(encoded[9], 0xFF, "RDB EOF opcode must be 0xFF");
        }

        /// Golden test: RDB with aux field must encode aux opcode correctly.
        #[test]
        fn golden_rdb_aux_field() {
            let encoded = encode_rdb(&[], &[("redis-ver", "7.0.0")]);
            // Header + AUX opcode (0xFA) + length-prefixed key + length-prefixed value
            assert!(
                encoded.starts_with(b"REDIS0011"),
                "RDB header must be REDIS0011"
            );
            // Aux opcode is 0xFA
            assert_eq!(encoded[9], 0xFA, "AUX opcode must be 0xFA");
        }

        /// Golden test: RDB string type encoding.
        #[test]
        fn golden_rdb_string_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"k".to_vec(),
                value: RdbValue::String(b"v".to_vec()),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // After header, expect:
            // - SELECTDB opcode (0xFE)
            // - db number (0x00)
            // - RESIZEDB opcode (0xFB)
            // - entries count, expires count
            // - TYPE_STRING (0x00)
            // - key length + key
            // - value length + value
            // - EOF + checksum

            // Type 0 = string
            let pos = encoded.iter().position(|&b| b == 0x00).unwrap();
            assert!(
                pos < encoded.len() - 8,
                "String type opcode should appear before EOF"
            );
        }

        /// Golden test: RDB with expiry must include EXPIRETIME_MS opcode.
        #[test]
        fn golden_rdb_expiry_opcode() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"k".to_vec(),
                value: RdbValue::String(b"v".to_vec()),
                expire_ms: Some(1_000_000),
            }];
            let encoded = encode_rdb(&entries, &[]);

            // EXPIRETIME_MS opcode is 0xFC
            assert!(
                encoded.contains(&0xFC),
                "RDB with expiry must contain EXPIRETIME_MS opcode (0xFC)"
            );
        }

        /// Golden test: RDB SELECTDB opcode appears for non-zero db.
        #[test]
        fn golden_rdb_selectdb_opcode() {
            let entries = vec![RdbEntry {
                db: 3,
                key: b"k".to_vec(),
                value: RdbValue::String(b"v".to_vec()),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // SELECTDB opcode is 0xFE
            assert!(
                encoded.contains(&0xFE),
                "RDB must contain SELECTDB opcode (0xFE)"
            );
        }

        /// Golden test: RDB list type encoding uses correct type byte.
        #[test]
        fn golden_rdb_list_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"mylist".to_vec(),
                value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec()]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // TYPE_LIST = 1
            // After SELECTDB+RESIZEDB, should see type byte 0x01
            let type_byte_found = encoded.windows(2).any(|w| w[0] == 0x01 && w[1] == 0x06);
            assert!(type_byte_found, "RDB list must have TYPE_LIST (0x01)");
        }

        /// Golden test: RDB set type encoding uses correct type byte.
        #[test]
        fn golden_rdb_set_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myset".to_vec(),
                value: RdbValue::Set(vec![b"x".to_vec()]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // TYPE_SET = 2
            let type_byte_found = encoded.windows(2).any(|w| w[0] == 0x02 && w[1] == 0x05);
            assert!(type_byte_found, "RDB set must have TYPE_SET (0x02)");
        }

        /// Golden test: RDB hash type encoding uses correct type byte.
        #[test]
        fn golden_rdb_hash_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myhash".to_vec(),
                value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // TYPE_HASH = 4
            let type_byte_found = encoded.windows(2).any(|w| w[0] == 0x04 && w[1] == 0x06);
            assert!(type_byte_found, "RDB hash must have TYPE_HASH (0x04)");
        }

        /// Golden test: RDB sorted set type encoding uses ZSET2 type byte.
        #[test]
        fn golden_rdb_zset_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myzset".to_vec(),
                value: RdbValue::SortedSet(vec![(b"member".to_vec(), 1.5)]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // TYPE_ZSET_2 = 5
            let type_byte_found = encoded.windows(2).any(|w| w[0] == 0x05 && w[1] == 0x06);
            assert!(
                type_byte_found,
                "RDB sorted set must have TYPE_ZSET_2 (0x05)"
            );
        }

        /// Golden test: RDB stream type encoding uses correct type byte.
        #[test]
        fn golden_rdb_stream_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"mystream".to_vec(),
                value: RdbValue::Stream(vec![], None, Vec::new()),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // TYPE_STREAM = 15 (0x0F)
            assert!(
                encoded.contains(&0x0F),
                "RDB stream must have TYPE_STREAM (0x0F)"
            );
        }

        /// Golden test: RDB EOF marker is always 0xFF.
        #[test]
        fn golden_rdb_eof_marker() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"k".to_vec(),
                value: RdbValue::String(b"v".to_vec()),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // EOF is 9 bytes from end (1 EOF + 8 checksum)
            let eof_pos = encoded.len() - 9;
            assert_eq!(
                encoded[eof_pos], 0xFF,
                "RDB EOF marker must be 0xFF at position {}",
                eof_pos
            );
        }

        /// Golden test: RDB checksum is 8 bytes at the end.
        #[test]
        fn golden_rdb_checksum_length() {
            let encoded = encode_rdb(&[], &[]);

            // Last 8 bytes are the CRC64 checksum
            let checksum_bytes = &encoded[encoded.len() - 8..];
            assert_eq!(checksum_bytes.len(), 8, "RDB checksum must be 8 bytes");
        }
    }

    // ── Proptest fuzz tests ──────────────────────────────────────────

    mod fuzz {
        use super::*;
        use proptest::prelude::*;
        use proptest::string::string_regex;
        use std::collections::BTreeMap;

        fn byte_vec_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 0..=max_len)
        }

        fn non_empty_byte_vec_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 1..=max_len)
        }

        fn finite_score_strategy() -> impl Strategy<Value = f64> {
            prop_oneof![
                (-1_000_000_i32..=1_000_000_i32).prop_map(|value| f64::from(value) / 1000.0),
                Just(0.0),
                Just(-0.0),
            ]
        }

        fn aof_record_strategy() -> impl Strategy<Value = AofRecord> {
            prop::collection::vec(byte_vec_strategy(16), 1..=6).prop_map(|argv| AofRecord { argv })
        }

        fn stream_entry_strategy() -> impl Strategy<Value = crate::StreamEntry> {
            (
                0_u64..=10_000,
                0_u64..=64,
                prop::collection::vec(
                    (non_empty_byte_vec_strategy(8), byte_vec_strategy(16)),
                    0..=4,
                ),
            )
        }

        fn stream_pending_entry_strategy() -> impl Strategy<Value = RdbStreamPendingEntry> {
            (
                0_u64..=10_000,
                0_u64..=64,
                non_empty_byte_vec_strategy(8),
                0_u64..=32,
                0_u64..=10_000,
            )
                .prop_map(
                    |(entry_id_ms, entry_id_seq, consumer, deliveries, last_delivered_ms)| {
                        RdbStreamPendingEntry {
                            entry_id_ms,
                            entry_id_seq,
                            consumer,
                            deliveries,
                            last_delivered_ms,
                        }
                    },
                )
        }

        fn stream_consumer_group_strategy() -> impl Strategy<Value = RdbStreamConsumerGroup> {
            (
                non_empty_byte_vec_strategy(8),
                0_u64..=10_000,
                0_u64..=64,
                prop::collection::vec(non_empty_byte_vec_strategy(8), 0..=3),
                prop::collection::vec(stream_pending_entry_strategy(), 0..=3),
            )
                .prop_map(
                    |(name, last_delivered_id_ms, last_delivered_id_seq, consumers, pending)| {
                        RdbStreamConsumerGroup {
                            name,
                            last_delivered_id_ms,
                            last_delivered_id_seq,
                            consumers,
                            pending,
                        }
                    },
                )
        }

        fn rdb_value_strategy() -> impl Strategy<Value = RdbValue> {
            prop_oneof![
                byte_vec_strategy(24).prop_map(RdbValue::String),
                prop::collection::vec(byte_vec_strategy(12), 0..=4).prop_map(RdbValue::List),
                prop::collection::vec(byte_vec_strategy(12), 0..=4).prop_map(RdbValue::Set),
                prop::collection::vec((byte_vec_strategy(8), byte_vec_strategy(12)), 0..=4,)
                    .prop_map(RdbValue::Hash),
                prop::collection::vec((byte_vec_strategy(8), finite_score_strategy()), 0..=4,)
                    .prop_map(RdbValue::SortedSet),
                (
                    prop::collection::vec(stream_entry_strategy(), 0..=3),
                    prop::option::of((0_u64..=10_000, 0_u64..=64)),
                    prop::collection::vec(stream_consumer_group_strategy(), 0..=2),
                )
                    .prop_map(|(entries, watermark, groups)| {
                        RdbValue::Stream(entries, watermark, groups)
                    }),
            ]
        }

        fn rdb_entry_strategy() -> impl Strategy<Value = Vec<RdbEntry>> {
            prop::collection::btree_map(
                (0_usize..=3, non_empty_byte_vec_strategy(8)),
                (rdb_value_strategy(), prop::option::of(0_u64..=1_000_000)),
                0..=8,
            )
            .prop_map(|entries| {
                entries
                    .into_iter()
                    .map(|((db, key), (value, expire_ms))| RdbEntry {
                        db,
                        key,
                        value,
                        expire_ms,
                    })
                    .collect()
            })
        }

        fn aux_fields_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
            prop::collection::btree_map(
                string_regex("[a-z][a-z0-9_-]{0,7}").expect("valid aux key regex"),
                string_regex("[A-Za-z0-9._:-]{0,12}").expect("valid aux value regex"),
                0..=4,
            )
            .prop_map(|fields| fields.into_iter().collect())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(10_000))]

            #[test]
            fn decode_rdb_never_panics(data: Vec<u8>) {
                let _ = decode_rdb(&data);
            }

            #[test]
            fn decode_aof_stream_never_panics(data: Vec<u8>) {
                let _ = decode_aof_stream(&data);
            }

            #[test]
            fn decode_rdb_with_valid_header_never_panics(payload: Vec<u8>) {
                // Start with valid RDB magic + version, then random payload.
                let mut data = b"REDIS0011".to_vec();
                data.extend_from_slice(&payload);
                let _ = decode_rdb(&data);
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn encode_decode_aof_stream_round_trips(records in prop::collection::vec(aof_record_strategy(), 0..=8)) {
                let encoded = encode_aof_stream(&records);
                let decoded = decode_aof_stream(&encoded).expect("generated AOF stream should decode");
                prop_assert_eq!(decoded, records);
            }

            #[test]
            fn encode_decode_rdb_round_trips(
                entries in rdb_entry_strategy(),
                aux_fields in aux_fields_strategy(),
            ) {
                let aux_refs: Vec<(&str, &str)> = aux_fields
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                let encoded = encode_rdb(&entries, &aux_refs);
                let (decoded_entries, decoded_aux) =
                    decode_rdb(&encoded).expect("generated RDB payload should decode");
                let expected_aux: BTreeMap<String, String> = aux_fields.into_iter().collect();
                prop_assert_eq!(decoded_entries, entries);
                prop_assert_eq!(decoded_aux, expected_aux);
            }
        }
    }

    mod metamorphic {
        use super::*;
        use proptest::prelude::*;
        use proptest::string::string_regex;

        fn byte_vec_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 0..=max_len)
        }

        fn non_empty_byte_vec_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 1..=max_len)
        }

        fn finite_score_strategy() -> impl Strategy<Value = f64> {
            prop_oneof![
                (-1_000_000_i32..=1_000_000_i32).prop_map(|value| f64::from(value) / 1000.0),
                Just(0.0),
            ]
        }

        fn aof_record_strategy() -> impl Strategy<Value = AofRecord> {
            prop::collection::vec(byte_vec_strategy(16), 1..=6).prop_map(|argv| AofRecord { argv })
        }

        fn stream_entry_strategy() -> impl Strategy<Value = crate::StreamEntry> {
            (
                0_u64..=10_000,
                0_u64..=64,
                prop::collection::vec(
                    (non_empty_byte_vec_strategy(8), byte_vec_strategy(16)),
                    0..=4,
                ),
            )
        }

        fn stream_pending_entry_strategy() -> impl Strategy<Value = RdbStreamPendingEntry> {
            (
                0_u64..=10_000,
                0_u64..=64,
                non_empty_byte_vec_strategy(8),
                0_u64..=32,
                0_u64..=10_000,
            )
                .prop_map(
                    |(entry_id_ms, entry_id_seq, consumer, deliveries, last_delivered_ms)| {
                        RdbStreamPendingEntry {
                            entry_id_ms,
                            entry_id_seq,
                            consumer,
                            deliveries,
                            last_delivered_ms,
                        }
                    },
                )
        }

        fn stream_consumer_group_strategy() -> impl Strategy<Value = RdbStreamConsumerGroup> {
            (
                non_empty_byte_vec_strategy(8),
                0_u64..=10_000,
                0_u64..=64,
                prop::collection::vec(non_empty_byte_vec_strategy(8), 0..=3),
                prop::collection::vec(stream_pending_entry_strategy(), 0..=3),
            )
                .prop_map(
                    |(name, last_delivered_id_ms, last_delivered_id_seq, consumers, pending)| {
                        RdbStreamConsumerGroup {
                            name,
                            last_delivered_id_ms,
                            last_delivered_id_seq,
                            consumers,
                            pending,
                        }
                    },
                )
        }

        fn rdb_value_strategy() -> impl Strategy<Value = RdbValue> {
            prop_oneof![
                byte_vec_strategy(24).prop_map(RdbValue::String),
                prop::collection::vec(byte_vec_strategy(12), 0..=4).prop_map(RdbValue::List),
                prop::collection::vec(byte_vec_strategy(12), 0..=4).prop_map(RdbValue::Set),
                prop::collection::vec((byte_vec_strategy(8), byte_vec_strategy(12)), 0..=4,)
                    .prop_map(RdbValue::Hash),
                prop::collection::vec((byte_vec_strategy(8), finite_score_strategy()), 0..=4,)
                    .prop_map(RdbValue::SortedSet),
                (
                    prop::collection::vec(stream_entry_strategy(), 0..=3),
                    prop::option::of((0_u64..=10_000, 0_u64..=64)),
                    prop::collection::vec(stream_consumer_group_strategy(), 0..=2),
                )
                    .prop_map(|(entries, watermark, groups)| {
                        RdbValue::Stream(entries, watermark, groups)
                    }),
            ]
        }

        fn rdb_entry_strategy() -> impl Strategy<Value = Vec<RdbEntry>> {
            prop::collection::btree_map(
                (0_usize..=3, non_empty_byte_vec_strategy(8)),
                (rdb_value_strategy(), prop::option::of(0_u64..=1_000_000)),
                0..=8,
            )
            .prop_map(|entries| {
                entries
                    .into_iter()
                    .map(|((db, key), (value, expire_ms))| RdbEntry {
                        db,
                        key,
                        value,
                        expire_ms,
                    })
                    .collect()
            })
        }

        fn aux_fields_strategy() -> impl Strategy<Value = Vec<(String, String)>> {
            prop::collection::btree_map(
                string_regex("[a-z][a-z0-9_-]{0,7}").expect("valid aux key regex"),
                string_regex("[A-Za-z0-9._:-]{0,12}").expect("valid aux value regex"),
                0..=4,
            )
            .prop_map(|fields| fields.into_iter().collect())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            /// MR: Encoding determinism - encoding the same data twice produces identical bytes.
            #[test]
            fn mr_aof_encoding_determinism(records in prop::collection::vec(aof_record_strategy(), 0..=8)) {
                let encoded1 = encode_aof_stream(&records);
                let encoded2 = encode_aof_stream(&records);
                prop_assert_eq!(encoded1, encoded2, "AOF encoding must be deterministic");
            }

            /// MR: Encoding determinism - encoding the same RDB twice produces identical bytes.
            #[test]
            fn mr_rdb_encoding_determinism(
                entries in rdb_entry_strategy(),
                aux_fields in aux_fields_strategy(),
            ) {
                let aux_refs: Vec<(&str, &str)> = aux_fields
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let encoded1 = encode_rdb(&entries, &aux_refs);
                let encoded2 = encode_rdb(&entries, &aux_refs);
                prop_assert_eq!(encoded1, encoded2, "RDB encoding must be deterministic");
            }

            /// MR: AOF concatenation equivalence - encode(a ++ b) == encode(a) ++ encode(b).
            #[test]
            fn mr_aof_concatenation_equivalence(
                records_a in prop::collection::vec(aof_record_strategy(), 0..=4),
                records_b in prop::collection::vec(aof_record_strategy(), 0..=4),
            ) {
                let mut combined = records_a.clone();
                combined.extend(records_b.clone());

                let encoded_combined = encode_aof_stream(&combined);
                let mut encoded_separate = encode_aof_stream(&records_a);
                encoded_separate.extend(encode_aof_stream(&records_b));

                prop_assert_eq!(
                    encoded_combined, encoded_separate,
                    "AOF: encode(a ++ b) must equal encode(a) ++ encode(b)"
                );
            }

            /// MR: RDB entry order invariance - shuffling entries produces same encoded output.
            /// encode_rdb internally sorts by (db, key), so input order shouldn't matter.
            #[test]
            fn mr_rdb_entry_order_invariance(
                entries in rdb_entry_strategy(),
                seed in any::<u64>(),
            ) {
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};

                if entries.len() < 2 {
                    return Ok(());
                }

                let encoded_original = encode_rdb(&entries, &[]);

                // Shuffle entries using deterministic permutation based on seed
                let mut shuffled = entries.clone();
                shuffled.sort_by(|a, b| {
                    let mut ha = DefaultHasher::new();
                    let mut hb = DefaultHasher::new();
                    seed.hash(&mut ha);
                    a.key.hash(&mut ha);
                    seed.hash(&mut hb);
                    b.key.hash(&mut hb);
                    ha.finish().cmp(&hb.finish())
                });

                let encoded_shuffled = encode_rdb(&shuffled, &[]);
                prop_assert_eq!(
                    encoded_original, encoded_shuffled,
                    "RDB encoding must be independent of input entry order"
                );
            }

            /// MR: RDB checksum consistency - checksum stored in encoded data matches computed.
            #[test]
            fn mr_rdb_checksum_consistency(
                entries in rdb_entry_strategy(),
                aux_fields in aux_fields_strategy(),
            ) {
                let aux_refs: Vec<(&str, &str)> = aux_fields
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let encoded = encode_rdb(&entries, &aux_refs);

                // Checksum is last 8 bytes
                let stored_checksum = u64::from_le_bytes(
                    encoded[encoded.len() - 8..].try_into().unwrap()
                );
                // Compute checksum over everything except the checksum itself
                let computed_checksum = crc64_redis(&encoded[..encoded.len() - 8]);

                prop_assert_eq!(
                    stored_checksum, computed_checksum,
                    "RDB stored checksum must match computed checksum"
                );
            }

            /// MR: AOF subset preservation - decoding a prefix of encoded records yields that prefix.
            #[test]
            fn mr_aof_subset_preservation(
                records in prop::collection::vec(aof_record_strategy(), 1..=8),
                prefix_len in 1_usize..=8,
            ) {
                let actual_prefix_len = prefix_len.min(records.len());
                let prefix_records: Vec<AofRecord> = records[..actual_prefix_len].to_vec();

                let encoded_prefix = encode_aof_stream(&prefix_records);
                let decoded = decode_aof_stream(&encoded_prefix).expect("prefix should decode");

                prop_assert_eq!(decoded, prefix_records, "AOF prefix decode must match prefix");
            }

            /// MR: RDB aux field independence - aux fields don't affect entry encoding.
            #[test]
            fn mr_rdb_aux_independence(
                entries in rdb_entry_strategy(),
                aux1 in aux_fields_strategy(),
                aux2 in aux_fields_strategy(),
            ) {
                let aux1_refs: Vec<(&str, &str)> = aux1
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                let aux2_refs: Vec<(&str, &str)> = aux2
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();

                let (decoded1, _) = decode_rdb(&encode_rdb(&entries, &aux1_refs))
                    .expect("should decode");
                let (decoded2, _) = decode_rdb(&encode_rdb(&entries, &aux2_refs))
                    .expect("should decode");

                prop_assert_eq!(
                    decoded1, decoded2,
                    "RDB entries must be independent of aux fields"
                );
            }

            /// MR: Empty input identity - encoding empty produces minimal valid output.
            #[test]
            fn mr_empty_aof_identity(_dummy in Just(())) {
                let encoded = encode_aof_stream(&[]);
                prop_assert!(encoded.is_empty(), "Empty AOF should encode to empty bytes");
                let decoded = decode_aof_stream(&encoded).expect("empty should decode");
                prop_assert!(decoded.is_empty(), "Empty AOF should decode to empty");
            }

            /// MR: Single record isolation - single record encodes/decodes independently.
            #[test]
            fn mr_aof_single_record_isolation(record in aof_record_strategy()) {
                let encoded = encode_aof_stream(std::slice::from_ref(&record));
                let decoded = decode_aof_stream(&encoded).expect("single record should decode");
                prop_assert_eq!(decoded.len(), 1);
                prop_assert_eq!(&decoded[0], &record);
            }
        }
    }
}
