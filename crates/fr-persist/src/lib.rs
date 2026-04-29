#![forbid(unsafe_code)]

use std::collections::BTreeMap;
#[cfg(feature = "upstream-stream-rdb")]
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use fr_protocol::{RespFrame, RespParseError};

pub mod listpack;
#[allow(dead_code)]
pub(crate) mod rdb_stream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofRecord {
    pub argv: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofReplayRecord {
    pub record: AofRecord,
    pub start_offset: usize,
    pub end_offset: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AofReplayStream {
    pub rdb_preamble: Option<RdbDecodeResult>,
    pub records: Vec<AofReplayRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofReplayTransactionTrim {
    pub records: Vec<AofReplayRecord>,
    pub truncated_from_offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofReplaySegmentPosition {
    Final,
    NonFinal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AofReplayTailRepairPolicy {
    Disabled,
    BoundedFinalSegment { max_tail_bytes: usize },
    HardenedNonAllowlisted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AofReplayTailFailure {
    Parse(RespParseError),
    InvalidFrame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofReplayTailRepair {
    pub records: Vec<AofReplayRecord>,
    pub truncated_from_offset: usize,
    pub truncated_bytes: usize,
    pub failure: AofReplayTailFailure,
    pub reason_code: &'static str,
    pub policy_reason_code: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AofReplayTailFatal {
    pub records: Vec<AofReplayRecord>,
    pub failure_offset: usize,
    pub trailing_bytes: usize,
    pub failure: AofReplayTailFailure,
    pub reason_code: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AofReplayTailRepairOutcome {
    Clean { records: Vec<AofReplayRecord> },
    Repaired(AofReplayTailRepair),
    Fatal(AofReplayTailFatal),
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

    pub fn replay_entries(&self) -> impl Iterator<Item = &AofManifestEntry> {
        self.base.iter().chain(self.incremental.iter())
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
    Ok(decode_aof_stream_with_offsets(input)?
        .into_iter()
        .map(|entry| entry.record)
        .collect())
}

const AOF_MAX_BULK_LEN: usize = 1024 * 1024 * 1024;
const AOF_MAX_ARRAY_LEN: usize = 10 * 1024 * 1024;
const AOF_MAX_RECURSION_DEPTH: usize = 1024;

fn aof_parser_config() -> fr_protocol::ParserConfig {
    fr_protocol::ParserConfig {
        max_bulk_len: AOF_MAX_BULK_LEN,
        max_array_len: AOF_MAX_ARRAY_LEN,
        max_recursion_depth: AOF_MAX_RECURSION_DEPTH,
        ..fr_protocol::ParserConfig::default()
    }
}

pub fn decode_aof_stream_with_offsets(input: &[u8]) -> Result<Vec<AofReplayRecord>, PersistError> {
    let mut cursor = 0usize;
    let mut out = Vec::new();
    let parser_config = aof_parser_config();
    while cursor < input.len() {
        let parsed = fr_protocol::parse_frame_with_config(&input[cursor..], &parser_config)?;
        let record = AofRecord::from_resp_frame(&parsed.frame)?;
        let start_offset = cursor;
        let end_offset = cursor.saturating_add(parsed.consumed);
        out.push(AofReplayRecord {
            record,
            start_offset,
            end_offset,
        });
        cursor = end_offset;
    }
    Ok(out)
}

/// Decode an AOF segment and classify final-tail repair eligibility.
#[must_use]
pub fn classify_aof_replay_tail_repair(
    input: &[u8],
    segment_position: AofReplaySegmentPosition,
    policy: AofReplayTailRepairPolicy,
) -> AofReplayTailRepairOutcome {
    let mut cursor = 0usize;
    let mut records = Vec::new();
    let parser_config = aof_parser_config();

    while cursor < input.len() {
        let parsed = match fr_protocol::parse_frame_with_config(&input[cursor..], &parser_config) {
            Ok(parsed) => parsed,
            Err(error) => {
                return classify_aof_tail_failure(
                    records,
                    cursor,
                    input.len().saturating_sub(cursor),
                    AofReplayTailFailure::Parse(error),
                    segment_position,
                    policy,
                );
            }
        };

        let record = match AofRecord::from_resp_frame(&parsed.frame) {
            Ok(record) => record,
            Err(error) => {
                return classify_aof_tail_failure(
                    records,
                    cursor,
                    input.len().saturating_sub(cursor),
                    aof_tail_failure_from_persist_error(error),
                    segment_position,
                    policy,
                );
            }
        };

        let start_offset = cursor;
        let end_offset = cursor.saturating_add(parsed.consumed);
        records.push(AofReplayRecord {
            record,
            start_offset,
            end_offset,
        });
        cursor = end_offset;
    }

    AofReplayTailRepairOutcome::Clean { records }
}

fn aof_tail_failure_from_persist_error(error: PersistError) -> AofReplayTailFailure {
    match error {
        PersistError::Parse(error) => AofReplayTailFailure::Parse(error),
        PersistError::InvalidFrame
        | PersistError::Io(_)
        | PersistError::ManifestParseViolation { .. }
        | PersistError::ManifestPathViolation { .. } => AofReplayTailFailure::InvalidFrame,
    }
}

fn classify_aof_tail_failure(
    records: Vec<AofReplayRecord>,
    failure_offset: usize,
    trailing_bytes: usize,
    failure: AofReplayTailFailure,
    segment_position: AofReplaySegmentPosition,
    policy: AofReplayTailRepairPolicy,
) -> AofReplayTailRepairOutcome {
    if segment_position == AofReplaySegmentPosition::NonFinal {
        return AofReplayTailRepairOutcome::Fatal(AofReplayTailFatal {
            records,
            failure_offset,
            trailing_bytes,
            failure,
            reason_code: "persist.replay.nonfinal_truncation_fatal",
        });
    }

    match policy {
        AofReplayTailRepairPolicy::BoundedFinalSegment { max_tail_bytes }
            if trailing_bytes <= max_tail_bytes =>
        {
            AofReplayTailRepairOutcome::Repaired(AofReplayTailRepair {
                records,
                truncated_from_offset: failure_offset,
                truncated_bytes: trailing_bytes,
                failure,
                reason_code: "persist.replay.tail_truncate_recover",
                policy_reason_code: "persist.replay.repair_policy_applied",
            })
        }
        AofReplayTailRepairPolicy::BoundedFinalSegment { .. } => {
            AofReplayTailRepairOutcome::Fatal(AofReplayTailFatal {
                records,
                failure_offset,
                trailing_bytes,
                failure,
                reason_code: "persist.replay.tail_repair_bound_exceeded",
            })
        }
        AofReplayTailRepairPolicy::HardenedNonAllowlisted => {
            AofReplayTailRepairOutcome::Fatal(AofReplayTailFatal {
                records,
                failure_offset,
                trailing_bytes,
                failure,
                reason_code: "persist.hardened_nonallowlisted_rejected",
            })
        }
        AofReplayTailRepairPolicy::Disabled => {
            AofReplayTailRepairOutcome::Fatal(AofReplayTailFatal {
                records,
                failure_offset,
                trailing_bytes,
                failure,
                reason_code: "persist.replay.frame_parse_invalid",
            })
        }
    }
}

/// Decode a Redis replay stream that is either RESP-only or RDB preamble + RESP tail.
pub fn decode_aof_replay_stream(input: &[u8]) -> Result<AofReplayStream, PersistError> {
    if input.starts_with(b"REDIS") {
        let rdb_preamble = decode_rdb_prefix(input)?;
        let mut records = decode_aof_stream_with_offsets(&input[rdb_preamble.consumed..])?;
        for record in &mut records {
            record.start_offset += rdb_preamble.consumed;
            record.end_offset += rdb_preamble.consumed;
        }

        return Ok(AofReplayStream {
            rdb_preamble: Some(rdb_preamble),
            records,
        });
    }

    Ok(AofReplayStream {
        rdb_preamble: None,
        records: decode_aof_stream_with_offsets(input)?,
    })
}

/// Return the replay-safe prefix, trimming a terminal unmatched MULTI block.
#[must_use]
pub fn trim_incomplete_multi_replay(records: &[AofReplayRecord]) -> AofReplayTransactionTrim {
    let mut multi_start_index = None;

    for (index, replay_record) in records.iter().enumerate() {
        let Some(command) = replay_record.record.argv.first() else {
            continue;
        };

        if command.eq_ignore_ascii_case(b"MULTI") {
            if multi_start_index.is_none() {
                multi_start_index = Some(index);
            }
        } else if multi_start_index.is_some()
            && (command.eq_ignore_ascii_case(b"EXEC") || command.eq_ignore_ascii_case(b"DISCARD"))
        {
            multi_start_index = None;
        }
    }

    if let Some(index) = multi_start_index {
        return AofReplayTransactionTrim {
            records: records[..index].to_vec(),
            truncated_from_offset: Some(records[index].start_offset),
        };
    }

    AofReplayTransactionTrim {
        records: records.to_vec(),
        truncated_from_offset: None,
    }
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
/// FrankenRedis-private type tag for hashes that carry at least one
/// per-field TTL. Kept in a private high-numbered range so upstream's
/// RDB_TYPE_STREAM_LISTPACKS_3 (21) can be decoded as a Redis stream.
/// Layout on disk:
///   [u8 type=100][key:string][u32 len][(field:string, value:string,
///                                       expires_ms:u64)]×len
/// The 0-sentinel convention: expires_ms == u64::MAX means "no TTL for
/// this field"; any other value is the absolute ms-since-epoch deadline.
/// (br-frankenredis-th7q)
const RDB_TYPE_HASH_WITH_TTLS: u8 = 100;
const RDB_TYPE_STREAM: u8 = 15; // FrankenRedis stream encoding
/// Upstream Redis compact-encoding type tags. fr-persist decodes these so a
/// dump.rdb produced by `redis-server` (which prefers compact forms for
/// small data structures) can be loaded without truncation. Encoder side
/// for these tags lives in fr-store::dump_key (DUMP/RESTORE).
/// (br-frankenredis-aqgx)
const RDB_TYPE_SET_INTSET: u8 = 11;
const RDB_TYPE_HASH_LISTPACK: u8 = 16;
const RDB_TYPE_ZSET_LISTPACK: u8 = 17;
const RDB_TYPE_LIST_QUICKLIST_2: u8 = 18;
const RDB_TYPE_SET_LISTPACK: u8 = 20;
/// Upstream Redis stream RDB type tags. Numbers overlap with our
/// internal type 15 (FrankenRedis stream encoding). Type 19 and 21 are
/// routed through the upstream stream decoder by the top-level RDB path.
/// (br-frankenredis-hjub, br-frankenredis-qi6z)
#[allow(dead_code)]
pub const UPSTREAM_RDB_TYPE_STREAM_LISTPACKS: u8 = 15;
#[allow(dead_code)]
pub const UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2: u8 = 19;
#[allow(dead_code)]
pub const UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3: u8 = 21;
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

/// Decoded RDB payload plus the byte offset immediately after the checksum.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbDecodeResult {
    pub entries: Vec<RdbEntry>,
    pub aux: BTreeMap<String, String>,
    pub consumed: usize,
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

/// Upstream stream RDB payload retained for exact file-level re-emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdbStreamMetadata {
    pub upstream_type_byte: u8,
    pub upstream_payload: Vec<u8>,
}

/// Value types supported in our RDB format.
#[derive(Debug, Clone, PartialEq)]
pub enum RdbValue {
    String(Vec<u8>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    /// Redis 7.4 hash with per-field TTLs. Each tuple is
    /// (field, value, Some(abs_deadline_ms)) for a TTL'd field or
    /// (field, value, None) for a field without a TTL. Encoded via
    /// RDB_TYPE_HASH_WITH_TTLS (100). (br-frankenredis-th7q)
    HashWithTtls(Vec<(Vec<u8>, Vec<u8>, Option<u64>)>),
    SortedSet(Vec<(Vec<u8>, f64)>),
    /// Stream: entries + optional watermark + consumer groups.
    Stream(
        Vec<StreamEntry>,
        Option<(u64, u64)>,
        Vec<RdbStreamConsumerGroup>,
        Option<RdbStreamMetadata>,
    ),
}

/// Encode a Redis 7.2+ STREAM_LISTPACKS_3 payload for DUMP/RESTORE values.
///
/// The returned bytes start after the type byte and before the DUMP trailer.
#[must_use]
pub fn encode_upstream_stream_listpacks3_payload(
    entries: &[StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
) -> Option<Vec<u8>> {
    rdb_stream::encode_upstream_stream_listpacks3(entries, watermark, groups)
}

/// Decode an upstream stream DUMP payload starting after the type byte.
///
/// Returns `(value, consumed)` so callers can verify the payload boundary.
#[must_use]
pub fn decode_upstream_stream_payload(type_byte: u8, data: &[u8]) -> Option<(RdbValue, usize)> {
    rdb_stream::decode_upstream_stream_skeleton(type_byte, data).ok()
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

/// Number of bytes `rdb_encode_length` would emit for the given length.
/// Used by `rdb_encode_string` to decide whether the LZF wire form is
/// strictly smaller than the raw form. (br-frankenredis-1uin)
fn rdb_length_size(len: usize) -> usize {
    if len < 64 {
        1
    } else if len < 16384 {
        2
    } else if len <= u32::MAX as usize {
        5
    } else {
        9
    }
}

/// Encode a length-prefixed string (RDB string encoding).
///
/// For inputs longer than 20 bytes, attempts LZF compression first and
/// emits the `0xC3 [comp_len:rdb_length] [orig_len:rdb_length] [payload]`
/// special encoding when the compressed wire form is strictly smaller
/// than the raw form. Mirrors upstream's `rdbSaveLzfStringObject` policy
/// (rdbcompression on) so dump.rdb files emitted by fr-persist
/// round-trip through `redis-server --loadrdb` even when long strings
/// are present. (br-frankenredis-1uin)
fn rdb_encode_string(buf: &mut Vec<u8>, data: &[u8]) {
    // Upstream skips LZF below this threshold because even a run of
    // repeated bytes cannot compress enough to beat the wire overhead.
    if data.len() > 20 {
        // Upstream's compressed-fits budget: `out_len = in_len - 4`.
        // lzf_compress returns None if it can't fit within that.
        let budget = data.len() - 4;
        if let Some(compressed) = lzf_compress(data, budget) {
            let raw_size = rdb_length_size(data.len()) + data.len();
            let lzf_size = 1
                + rdb_length_size(compressed.len())
                + rdb_length_size(data.len())
                + compressed.len();
            if lzf_size < raw_size {
                buf.push(0xC3);
                rdb_encode_length(buf, compressed.len());
                rdb_encode_length(buf, data.len());
                buf.extend_from_slice(&compressed);
                return;
            }
        }
    }
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
            RdbValue::HashWithTtls(fields) => {
                buf.push(RDB_TYPE_HASH_WITH_TTLS);
                rdb_encode_string(&mut buf, &entry.key);
                rdb_encode_length(&mut buf, fields.len());
                for (field, value, expires_ms) in fields {
                    rdb_encode_string(&mut buf, field);
                    rdb_encode_string(&mut buf, value);
                    // u64::MAX sentinel = "no TTL". Any other value is
                    // the absolute ms-since-epoch deadline.
                    let encoded = expires_ms.unwrap_or(u64::MAX);
                    buf.extend_from_slice(&encoded.to_le_bytes());
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
            RdbValue::Stream(stream_entries, watermark, groups, metadata) => {
                encode_stream_rdb_value(
                    &mut buf,
                    &entry.key,
                    stream_entries,
                    *watermark,
                    groups,
                    metadata,
                );
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

fn encode_stream_rdb_value(
    buf: &mut Vec<u8>,
    key: &[u8],
    stream_entries: &[StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
    metadata: &Option<RdbStreamMetadata>,
) {
    if let Some(metadata) = metadata {
        buf.push(metadata.upstream_type_byte);
        rdb_encode_string(buf, key);
        buf.extend_from_slice(&metadata.upstream_payload);
        return;
    }

    #[cfg(feature = "upstream-stream-rdb")]
    {
        if can_encode_upstream_stream_losslessly(stream_entries, watermark, groups)
            && let Some(payload) =
                rdb_stream::encode_upstream_stream_listpacks3(stream_entries, watermark, groups)
        {
            buf.push(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3);
            rdb_encode_string(buf, key);
            buf.extend_from_slice(&payload);
            return;
        }
    }

    encode_private_stream_rdb_value(buf, key, stream_entries, watermark, groups);
}

#[cfg(feature = "upstream-stream-rdb")]
fn can_encode_upstream_stream_losslessly(
    stream_entries: &[StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
) -> bool {
    if watermark.is_none() {
        return false;
    }

    if stream_entries.windows(2).any(|pair| {
        let left = (pair[0].0, pair[0].1);
        let right = (pair[1].0, pair[1].1);
        left >= right
    }) {
        return false;
    }

    groups.iter().all(consumer_group_is_lossless_type21)
}

#[cfg(feature = "upstream-stream-rdb")]
fn consumer_group_is_lossless_type21(group: &RdbStreamConsumerGroup) -> bool {
    let mut pending_ids = BTreeSet::new();
    for pending in &group.pending {
        if !group
            .consumers
            .iter()
            .any(|consumer| consumer.as_slice() == pending.consumer.as_slice())
        {
            return false;
        }
        if !pending_ids.insert((pending.entry_id_ms, pending.entry_id_seq)) {
            return false;
        }
    }

    let mut encoded_order = Vec::with_capacity(group.pending.len());
    for consumer in &group.consumers {
        encoded_order.extend(
            group
                .pending
                .iter()
                .filter(|pending| pending.consumer.as_slice() == consumer.as_slice()),
        );
    }

    encoded_order.into_iter().eq(group.pending.iter())
}

fn encode_private_stream_rdb_value(
    buf: &mut Vec<u8>,
    key: &[u8],
    stream_entries: &[StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
) {
    buf.push(RDB_TYPE_STREAM);
    rdb_encode_string(buf, key);
    let (wm_ms, wm_seq) = watermark.unwrap_or((0, 0));
    buf.extend_from_slice(&wm_ms.to_le_bytes());
    buf.extend_from_slice(&wm_seq.to_le_bytes());
    rdb_encode_length(buf, stream_entries.len());
    for (ms, seq, fields) in stream_entries {
        buf.extend_from_slice(&ms.to_le_bytes());
        buf.extend_from_slice(&seq.to_le_bytes());
        rdb_encode_length(buf, fields.len());
        for (fname, fval) in fields {
            rdb_encode_string(buf, fname);
            rdb_encode_string(buf, fval);
        }
    }
    // Consumer groups
    rdb_encode_length(buf, groups.len());
    for group in groups {
        rdb_encode_string(buf, &group.name);
        buf.extend_from_slice(&group.last_delivered_id_ms.to_le_bytes());
        buf.extend_from_slice(&group.last_delivered_id_seq.to_le_bytes());
        rdb_encode_length(buf, group.consumers.len());
        for consumer in &group.consumers {
            rdb_encode_string(buf, consumer);
        }
        rdb_encode_length(buf, group.pending.len());
        for pe in &group.pending {
            buf.extend_from_slice(&pe.entry_id_ms.to_le_bytes());
            buf.extend_from_slice(&pe.entry_id_seq.to_le_bytes());
            rdb_encode_string(buf, &pe.consumer);
            buf.extend_from_slice(&pe.deliveries.to_le_bytes());
            buf.extend_from_slice(&pe.last_delivered_ms.to_le_bytes());
        }
    }
}

/// Decode an RDB length. Returns `(length, bytes_consumed)` or `None` on
/// insufficient data. Note: this function returns `None` for special encodings (type 3)
/// since they represent string values, not lengths.
/// Decode an upstream-encoded Redis intset (as it appears wrapped inside an
/// RDB string for `RDB_TYPE_SET_INTSET`). The wire format is
/// `[encoding:u32 LE][len:u32 LE][element:encoding-bytes × len]` with
/// `encoding ∈ {2, 4, 8}` selecting the per-element width in bytes
/// (mirrors `intset.h`). Returns the elements as their canonical decimal
/// string form so they round-trip through `RdbValue::Set(Vec<Vec<u8>>)`.
/// (br-frankenredis-aqgx)
fn decode_intset_members(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    if data.len() < 8 {
        return None;
    }
    let encoding = u32::from_le_bytes(data[0..4].try_into().ok()?);
    let len = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let width = match encoding {
        2 => 2,
        4 => 4,
        8 => 8,
        _ => return None,
    };
    let expected_len = 8usize.checked_add(len.checked_mul(width)?)?;
    if data.len() != expected_len {
        return None;
    }
    let mut members = Vec::with_capacity(len);
    let mut cursor = 8;
    for _ in 0..len {
        let value = match width {
            2 => {
                let raw = i16::from_le_bytes(data[cursor..cursor + 2].try_into().ok()?);
                cursor += 2;
                i64::from(raw)
            }
            4 => {
                let raw = i32::from_le_bytes(data[cursor..cursor + 4].try_into().ok()?);
                cursor += 4;
                i64::from(raw)
            }
            8 => {
                let raw = i64::from_le_bytes(data[cursor..cursor + 8].try_into().ok()?);
                cursor += 8;
                raw
            }
            _ => unreachable!("width is one of 2, 4, 8"),
        };
        members.push(value.to_string().into_bytes());
    }
    Some(members)
}

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

/// LZF compressor. Pure-Rust port of upstream `lzf_c.c::lzf_compress`
/// (Marc Lehmann's LZF, BSD/GPL dual-licensed). Mirrors the wire
/// format `lzf_decompress` already accepts:
///
/// ```text
/// 000LLLLL <L+1 octets>           ; literal run, L+1 = 1..32 bytes
/// LLLooooo oooooooo              ; backref, copy_len = L+2 (3..8), off = (top5<<8 | low8) + 1
/// 111ooooo LLLLLLLL oooooooo     ; backref, copy_len = L+9 (9..264), off as above
/// ```
///
/// Returns `None` if the encoded stream would not fit in `out_budget`
/// bytes (caller's signal that compression isn't worth it). Upstream
/// uses `outlen = in_len - 4` as the budget — we follow the same
/// convention so a "compressed-fits" check guarantees ≥ 5 bytes saved
/// after the 0xC3 + length prefix overhead is folded in.
///
/// (br-frankenredis-1uin)
fn lzf_compress(input: &[u8], out_budget: usize) -> Option<Vec<u8>> {
    const HLOG: u32 = 14;
    const HSIZE: usize = 1 << HLOG;
    const MAX_LIT: usize = 32;
    const MAX_OFF: usize = 8192;
    const MAX_REF: usize = (1 << 8) + (1 << 3); // 264

    let in_len = input.len();
    if in_len == 0 || out_budget == 0 {
        return None;
    }

    // Knuth multiplicative hash on the (next 3 bytes) trigram.
    fn trigram(buf: &[u8], i: usize) -> u32 {
        ((buf[i] as u32) << 16) | ((buf[i + 1] as u32) << 8) | (buf[i + 2] as u32)
    }
    fn hash(v: u32) -> usize {
        ((v.wrapping_mul(2_654_435_761)) >> (32 - HLOG)) as usize & (HSIZE - 1)
    }

    let mut out: Vec<u8> = Vec::with_capacity(out_budget);
    // 0 means "unset" (we store ip+1 so position 0 is representable).
    let mut htab = vec![0u32; HSIZE];

    let mut ip: usize = 0;
    let mut lit_hdr_pos: usize = out.len();
    out.push(0); // placeholder for literal-run header, filled when run ends
    if out.len() > out_budget {
        return None;
    }
    let mut lit: usize = 0;

    while ip + 2 < in_len {
        let v = trigram(input, ip);
        let h = hash(v);
        let stored = htab[h];
        htab[h] = (ip as u32).wrapping_add(1);
        let ref_idx = if stored == 0 {
            None
        } else {
            Some((stored - 1) as usize)
        };

        let mut emitted_match = false;
        if let Some(r) = ref_idx {
            // off = ip - ref - 1, must be in 0..MAX_OFF.
            if r < ip
                && ip - r - 1 < MAX_OFF
                && r + 2 < in_len
                && input[r] == input[ip]
                && input[r + 1] == input[ip + 1]
                && input[r + 2] == input[ip + 2]
            {
                let off = ip - r - 1;
                let max_len = std::cmp::min(MAX_REF, in_len - ip);
                let mut match_len = 3;
                while match_len < max_len && input[r + match_len] == input[ip + match_len] {
                    match_len += 1;
                }
                let len_minus_2 = match_len - 2; // 1..MAX_REF - 2 = 1..262

                // Close the open literal run.
                if lit > 0 {
                    out[lit_hdr_pos] = (lit - 1) as u8;
                } else {
                    // Empty run — drop the placeholder.
                    out.pop();
                }

                let off_hi = ((off >> 8) & 0x1F) as u8;
                if len_minus_2 < 7 {
                    let header = ((len_minus_2 as u8) << 5) | off_hi;
                    out.push(header);
                } else {
                    out.push((7u8 << 5) | off_hi);
                    out.push((len_minus_2 - 7) as u8);
                }
                out.push((off & 0xFF) as u8);
                if out.len() > out_budget {
                    return None;
                }

                ip += match_len;

                // Open a new literal run.
                lit = 0;
                lit_hdr_pos = out.len();
                out.push(0);
                if out.len() > out_budget {
                    return None;
                }
                emitted_match = true;
            }
        }

        if !emitted_match {
            out.push(input[ip]);
            if out.len() > out_budget {
                return None;
            }
            lit += 1;
            ip += 1;
            if lit == MAX_LIT {
                out[lit_hdr_pos] = (lit - 1) as u8;
                lit = 0;
                lit_hdr_pos = out.len();
                out.push(0);
                if out.len() > out_budget {
                    return None;
                }
            }
        }
    }

    // Tail: drain remaining 0..2 bytes as literals.
    while ip < in_len {
        out.push(input[ip]);
        if out.len() > out_budget {
            return None;
        }
        lit += 1;
        ip += 1;
        if lit == MAX_LIT {
            out[lit_hdr_pos] = (lit - 1) as u8;
            lit = 0;
            lit_hdr_pos = out.len();
            out.push(0);
            if out.len() > out_budget {
                return None;
            }
        }
    }

    // Finalize the trailing literal run.
    if lit > 0 {
        out[lit_hdr_pos] = (lit - 1) as u8;
    } else {
        out.pop();
    }

    if out.is_empty() || out.len() > out_budget {
        return None;
    }
    Some(out)
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

/// Decode an RDB preamble and report the first byte after its checksum.
///
/// Redis AOF replay can begin with an RDB preamble followed by RESP AOF records.
/// This API decodes only the RDB prefix and leaves any tail bytes to the caller.
pub fn decode_rdb_prefix(data: &[u8]) -> Result<RdbDecodeResult, PersistError> {
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
                | RDB_TYPE_HASH_WITH_TTLS
                | RDB_TYPE_ZSET_2
                | RDB_TYPE_STREAM
                | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2
                | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3
                | RDB_TYPE_SET_INTSET
                | RDB_TYPE_HASH_LISTPACK
                | RDB_TYPE_ZSET_LISTPACK
                | RDB_TYPE_LIST_QUICKLIST_2
                | RDB_TYPE_SET_LISTPACK
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
                if cursor + RDB_CHECKSUM_LEN > data.len() {
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
                cursor += RDB_CHECKSUM_LEN;
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
            type_byte @ (RDB_TYPE_STRING
            | RDB_TYPE_LIST
            | RDB_TYPE_SET
            | RDB_TYPE_HASH
            | RDB_TYPE_HASH_WITH_TTLS
            | RDB_TYPE_ZSET_2
            | RDB_TYPE_STREAM
            | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2
            | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3
            | RDB_TYPE_SET_INTSET
            | RDB_TYPE_HASH_LISTPACK
            | RDB_TYPE_ZSET_LISTPACK
            | RDB_TYPE_LIST_QUICKLIST_2
            | RDB_TYPE_SET_LISTPACK) => {
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
                    RDB_TYPE_HASH_WITH_TTLS => {
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
                            if cursor + 8 > data.len() {
                                return Err(PersistError::InvalidFrame);
                            }
                            let mut deadline_buf = [0u8; 8];
                            deadline_buf.copy_from_slice(&data[cursor..cursor + 8]);
                            cursor += 8;
                            let raw = u64::from_le_bytes(deadline_buf);
                            let expires = if raw == u64::MAX { None } else { Some(raw) };
                            fields.push((f, v, expires));
                        }
                        RdbValue::HashWithTtls(fields)
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
                        RdbValue::Stream(stream_entries, watermark, groups, None)
                    }
                    UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2 | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3 => {
                        let (value, consumed) =
                            rdb_stream::decode_upstream_stream_skeleton(type_byte, &data[cursor..])
                                .map_err(|_| PersistError::InvalidFrame)?;
                        cursor += consumed;
                        value
                    }
                    RDB_TYPE_SET_INTSET => {
                        // Payload is a string-wrapped binary intset blob.
                        let (intset, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let members =
                            decode_intset_members(&intset).ok_or(PersistError::InvalidFrame)?;
                        RdbValue::Set(members)
                    }
                    RDB_TYPE_SET_LISTPACK => {
                        let (listpack, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let members = listpack::decode_listpack(&listpack)
                            .map_err(|_| PersistError::InvalidFrame)?
                            .into_iter()
                            .map(|entry| entry.to_bytes())
                            .collect();
                        RdbValue::Set(members)
                    }
                    RDB_TYPE_HASH_LISTPACK => {
                        // Listpack of f1, v1, f2, v2, ... pairs.
                        let (listpack, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let entries: Vec<Vec<u8>> = listpack::decode_listpack(&listpack)
                            .map_err(|_| PersistError::InvalidFrame)?
                            .into_iter()
                            .map(|entry| entry.to_bytes())
                            .collect();
                        if !entries.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let mut fields = Vec::with_capacity(entries.len() / 2);
                        let mut chunks = entries.chunks_exact(2);
                        for pair in &mut chunks {
                            fields.push((pair[0].clone(), pair[1].clone()));
                        }
                        RdbValue::Hash(fields)
                    }
                    RDB_TYPE_ZSET_LISTPACK => {
                        // Listpack of m1, score1, m2, score2, ... where each
                        // score is encoded as a decimal string (upstream
                        // calls listpackAppend with the textual score).
                        let (listpack, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let entries: Vec<Vec<u8>> = listpack::decode_listpack(&listpack)
                            .map_err(|_| PersistError::InvalidFrame)?
                            .into_iter()
                            .map(|entry| entry.to_bytes())
                            .collect();
                        if !entries.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let mut members = Vec::with_capacity(entries.len() / 2);
                        let mut chunks = entries.chunks_exact(2);
                        for pair in &mut chunks {
                            let score = std::str::from_utf8(&pair[1])
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                                .ok_or(PersistError::InvalidFrame)?;
                            members.push((pair[0].clone(), score));
                        }
                        RdbValue::SortedSet(members)
                    }
                    RDB_TYPE_LIST_QUICKLIST_2 => {
                        // node_count nodes, each: (container:length,
                        // listpack:string). Upstream's container is 1 for
                        // PLAIN nodes (raw string elements) and 2 for
                        // PACKED nodes (listpack-of-elements). We accept
                        // both; a PLAIN node carries exactly one element.
                        let (node_count, consumed) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let mut items = Vec::with_capacity(node_count.min(1024));
                        for _ in 0..node_count {
                            let (container, consumed) = rdb_decode_length(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += consumed;
                            let (node_blob, consumed) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += consumed;
                            match container {
                                1 => {
                                    // PLAIN: the blob is the element itself.
                                    items.push(node_blob);
                                }
                                2 => {
                                    // PACKED: the blob is a listpack.
                                    for entry in listpack::decode_listpack(&node_blob)
                                        .map_err(|_| PersistError::InvalidFrame)?
                                    {
                                        items.push(entry.to_bytes());
                                    }
                                }
                                _ => return Err(PersistError::InvalidFrame),
                            }
                        }
                        RdbValue::List(items)
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

    Ok(RdbDecodeResult {
        entries,
        aux,
        consumed: cursor,
    })
}

/// Decode an RDB file into entries. Returns entries and auxiliary metadata.
pub fn decode_rdb(data: &[u8]) -> Result<(Vec<RdbEntry>, BTreeMap<String, String>), PersistError> {
    let decoded = decode_rdb_prefix(data)?;
    if decoded.consumed != data.len() {
        return Err(PersistError::InvalidFrame);
    }

    Ok((decoded.entries, decoded.aux))
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
        AofManifest, AofManifestFileType, AofRecord, AofReplaySegmentPosition,
        AofReplayTailFailure, AofReplayTailRepairOutcome, AofReplayTailRepairPolicy, PersistError,
        classify_aof_replay_tail_repair, decode_aof_replay_stream, decode_aof_stream,
        decode_aof_stream_with_offsets, encode_aof_stream, format_aof_manifest, parse_aof_manifest,
        trim_incomplete_multi_replay,
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
    fn decode_aof_stream_with_offsets_preserves_record_boundaries() {
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
            },
        ];
        let first_len = records[0].to_resp_frame().to_bytes().len();
        let encoded = encode_aof_stream(&records);

        let decoded = decode_aof_stream_with_offsets(&encoded).expect("decode stream");

        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].record, records[0]);
        assert_eq!(decoded[0].start_offset, 0);
        assert_eq!(decoded[0].end_offset, first_len);
        assert_eq!(decoded[1].record, records[1]);
        assert_eq!(decoded[1].start_offset, first_len);
        assert_eq!(decoded[1].end_offset, encoded.len());
    }

    #[test]
    fn decode_aof_stream_with_offsets_accepts_empty_stream() {
        let decoded = decode_aof_stream_with_offsets(b"").expect("empty stream");
        assert!(decoded.is_empty());
    }

    #[test]
    fn decode_aof_stream_with_offsets_rejects_invalid_and_incomplete_frames() {
        let err = decode_aof_stream_with_offsets(b"$3\r\nbad\r\n").expect_err("must fail");
        assert_eq!(err, PersistError::InvalidFrame);

        let err =
            decode_aof_stream_with_offsets(b"*2\r\n$3\r\nGET\r\n$1\r\nk").expect_err("must fail");
        assert_eq!(err, PersistError::Parse(RespParseError::Incomplete));
    }

    #[test]
    fn decode_aof_replay_stream_decodes_resp_only_input_with_offsets() {
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"DEL".to_vec(), b"k".to_vec()],
            },
        ];
        let first_len = records[0].to_resp_frame().to_bytes().len();
        let encoded = encode_aof_stream(&records);

        let replay = decode_aof_replay_stream(&encoded).expect("decode replay stream");

        assert!(replay.rdb_preamble.is_none());
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.records[0].record, records[0]);
        assert_eq!(replay.records[0].start_offset, 0);
        assert_eq!(replay.records[0].end_offset, first_len);
        assert_eq!(replay.records[1].record, records[1]);
        assert_eq!(replay.records[1].start_offset, first_len);
        assert_eq!(replay.records[1].end_offset, encoded.len());
    }

    #[test]
    fn trim_incomplete_multi_replay_returns_valid_prefix_and_truncation_offset() {
        let records = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
            },
            AofRecord {
                argv: vec![b"MULTI".to_vec()],
            },
            AofRecord {
                argv: vec![b"SET".to_vec(), b"inside".to_vec(), b"2".to_vec()],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"inside-counter".to_vec()],
            },
        ];
        let replay_records =
            decode_aof_stream_with_offsets(&encode_aof_stream(&records)).expect("decode records");
        let multi_offset = replay_records[1].start_offset;

        let trimmed = trim_incomplete_multi_replay(&replay_records);

        assert_eq!(trimmed.records, replay_records[..1]);
        assert_eq!(trimmed.truncated_from_offset, Some(multi_offset));
    }

    #[test]
    fn trim_incomplete_multi_replay_preserves_complete_exec_transaction() {
        let records = vec![
            AofRecord {
                argv: vec![b"multi".to_vec()],
            },
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"exec".to_vec()],
            },
        ];
        let replay_records =
            decode_aof_stream_with_offsets(&encode_aof_stream(&records)).expect("decode records");

        let trimmed = trim_incomplete_multi_replay(&replay_records);

        assert_eq!(trimmed.records, replay_records);
        assert_eq!(trimmed.truncated_from_offset, None);
    }

    #[test]
    fn trim_incomplete_multi_replay_preserves_discarded_transaction_boundary() {
        let records = vec![
            AofRecord {
                argv: vec![b"MULTI".to_vec()],
            },
            AofRecord {
                argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"DISCARD".to_vec()],
            },
            AofRecord {
                argv: vec![b"SET".to_vec(), b"after".to_vec(), b"1".to_vec()],
            },
        ];
        let replay_records =
            decode_aof_stream_with_offsets(&encode_aof_stream(&records)).expect("decode records");

        let trimmed = trim_incomplete_multi_replay(&replay_records);

        assert_eq!(trimmed.records, replay_records);
        assert_eq!(trimmed.truncated_from_offset, None);
    }

    #[test]
    fn classify_aof_replay_tail_repair_preserves_clean_segment() -> Result<(), String> {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
        }];
        let encoded = encode_aof_stream(&records);

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::Final,
            AofReplayTailRepairPolicy::Disabled,
        );

        let AofReplayTailRepairOutcome::Clean {
            records: replay_records,
        } = outcome
        else {
            return Err(format!(
                "clean segment should not require repair: {outcome:?}"
            ));
        };
        assert_eq!(replay_records.len(), 1);
        assert_eq!(replay_records[0].record, records[0]);
        assert_eq!(replay_records[0].start_offset, 0);
        assert_eq!(replay_records[0].end_offset, encoded.len());
        Ok(())
    }

    #[test]
    fn classify_aof_replay_tail_repair_truncates_bounded_final_tail() -> Result<(), String> {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
        }];
        let valid_prefix = encode_aof_stream(&records);
        let mut encoded = valid_prefix.clone();
        encoded.extend_from_slice(b"*2\r\n$3\r\nSET\r\n$1\r\nx");
        let truncated_bytes = encoded.len() - valid_prefix.len();

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::Final,
            AofReplayTailRepairPolicy::BoundedFinalSegment {
                max_tail_bytes: truncated_bytes,
            },
        );

        let AofReplayTailRepairOutcome::Repaired(repair) = outcome else {
            return Err(format!("bounded final tail should repair: {outcome:?}"));
        };
        assert_eq!(repair.records.len(), 1);
        assert_eq!(repair.records[0].record, records[0]);
        assert_eq!(repair.truncated_from_offset, valid_prefix.len());
        assert_eq!(repair.truncated_bytes, truncated_bytes);
        assert_eq!(
            repair.failure,
            AofReplayTailFailure::Parse(RespParseError::Incomplete)
        );
        assert_eq!(repair.reason_code, "persist.replay.tail_truncate_recover");
        assert_eq!(
            repair.policy_reason_code,
            "persist.replay.repair_policy_applied"
        );
        Ok(())
    }

    #[test]
    fn classify_aof_replay_tail_repair_handles_corrupt_final_frame() -> Result<(), String> {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
        }];
        let valid_prefix = encode_aof_stream(&records);
        let mut encoded = valid_prefix.clone();
        encoded.extend_from_slice(b"$3\r\nbad\r\n");
        let corrupted_bytes = encoded.len() - valid_prefix.len();

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::Final,
            AofReplayTailRepairPolicy::BoundedFinalSegment {
                max_tail_bytes: corrupted_bytes,
            },
        );

        let AofReplayTailRepairOutcome::Repaired(repair) = outcome else {
            return Err(format!(
                "bounded final corruption should repair: {outcome:?}"
            ));
        };
        assert_eq!(repair.records.len(), 1);
        assert_eq!(repair.truncated_from_offset, valid_prefix.len());
        assert_eq!(repair.truncated_bytes, corrupted_bytes);
        assert_eq!(repair.failure, AofReplayTailFailure::InvalidFrame);
        Ok(())
    }

    #[test]
    fn classify_aof_replay_tail_repair_rejects_nonfinal_tail_corruption() -> Result<(), String> {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
        }];
        let valid_prefix = encode_aof_stream(&records);
        let mut encoded = valid_prefix.clone();
        encoded.extend_from_slice(b"*2\r\n$3\r\nSET\r\n$1\r\nx");

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::NonFinal,
            AofReplayTailRepairPolicy::BoundedFinalSegment {
                max_tail_bytes: encoded.len(),
            },
        );

        let AofReplayTailRepairOutcome::Fatal(fatal) = outcome else {
            return Err(format!("non-final segment must fail closed: {outcome:?}"));
        };
        assert_eq!(fatal.records.len(), 1);
        assert_eq!(fatal.failure_offset, valid_prefix.len());
        assert_eq!(fatal.trailing_bytes, encoded.len() - valid_prefix.len());
        assert_eq!(
            fatal.failure,
            AofReplayTailFailure::Parse(RespParseError::Incomplete)
        );
        assert_eq!(
            fatal.reason_code,
            "persist.replay.nonfinal_truncation_fatal"
        );
        Ok(())
    }

    #[test]
    fn classify_aof_replay_tail_repair_rejects_over_bound_final_tail() -> Result<(), String> {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
        }];
        let valid_prefix = encode_aof_stream(&records);
        let mut encoded = valid_prefix.clone();
        encoded.extend_from_slice(b"*2\r\n$3\r\nSET\r\n$1\r\nx");

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::Final,
            AofReplayTailRepairPolicy::BoundedFinalSegment { max_tail_bytes: 1 },
        );

        let AofReplayTailRepairOutcome::Fatal(fatal) = outcome else {
            return Err(format!(
                "over-bound final tail should stay fatal: {outcome:?}"
            ));
        };
        assert_eq!(fatal.records.len(), 1);
        assert_eq!(fatal.failure_offset, valid_prefix.len());
        assert_eq!(
            fatal.reason_code,
            "persist.replay.tail_repair_bound_exceeded"
        );
        Ok(())
    }

    #[test]
    fn classify_aof_replay_tail_repair_rejects_hardened_nonallowlisted_repair() -> Result<(), String>
    {
        let records = vec![AofRecord {
            argv: vec![b"SET".to_vec(), b"before".to_vec(), b"1".to_vec()],
        }];
        let valid_prefix = encode_aof_stream(&records);
        let mut encoded = valid_prefix.clone();
        encoded.extend_from_slice(b"*2\r\n$3\r\nSET\r\n$1\r\nx");

        let outcome = classify_aof_replay_tail_repair(
            &encoded,
            AofReplaySegmentPosition::Final,
            AofReplayTailRepairPolicy::HardenedNonAllowlisted,
        );

        let AofReplayTailRepairOutcome::Fatal(fatal) = outcome else {
            return Err(format!(
                "non-allowlisted hardened repair must reject: {outcome:?}"
            ));
        };
        assert_eq!(fatal.records.len(), 1);
        assert_eq!(fatal.failure_offset, valid_prefix.len());
        assert_eq!(
            fatal.reason_code,
            "persist.hardened_nonallowlisted_rejected"
        );
        Ok(())
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
    fn aof_manifest_replay_entries_exclude_history_and_preserve_replay_order() {
        let manifest = parse_aof_manifest(
            "file base.aof seq 1 type b\n\
             file old-2.aof seq 2 type h\n\
             file incr-3.aof seq 3 type i\n\
             file old-4.aof seq 4 type h\n\
             file incr-5.aof seq 5 type i\n",
        )
        .expect("parse manifest");

        let replay = manifest
            .replay_entries()
            .map(|entry| (entry.file_name.as_str(), entry.file_type))
            .collect::<Vec<_>>();

        assert_eq!(
            replay,
            vec![
                ("base.aof", AofManifestFileType::Base),
                ("incr-3.aof", AofManifestFileType::Incremental),
                ("incr-5.aof", AofManifestFileType::Incremental),
            ],
        );
    }

    #[test]
    fn aof_manifest_replay_entries_allow_incremental_only_and_empty_manifests() {
        let manifest = parse_aof_manifest(
            "file incr-1.aof seq 1 type i\n\
             file incr-2.aof seq 2 type i\n",
        )
        .expect("parse manifest");

        let replay = manifest
            .replay_entries()
            .map(|entry| (entry.file_name.as_str(), entry.file_type))
            .collect::<Vec<_>>();

        assert_eq!(
            replay,
            vec![
                ("incr-1.aof", AofManifestFileType::Incremental),
                ("incr-2.aof", AofManifestFileType::Incremental),
            ],
        );
        assert!(AofManifest::default().replay_entries().next().is_none());
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
        RDB_CHECKSUM_LEN, RDB_OPCODE_AUX, RDB_OPCODE_EOF, RDB_OPCODE_EXPIRETIME_MS,
        RDB_OPCODE_RESIZEDB, RDB_OPCODE_SELECTDB, RDB_TYPE_HASH_LISTPACK, RDB_TYPE_HASH_WITH_TTLS,
        RDB_TYPE_LIST_QUICKLIST_2, RDB_TYPE_SET_INTSET, RDB_TYPE_SET_LISTPACK, RDB_TYPE_STRING,
        RDB_TYPE_ZSET_LISTPACK, RdbEntry, RdbStreamConsumerGroup, RdbStreamMetadata,
        RdbStreamPendingEntry, RdbValue, UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, crc64_redis,
        decode_intset_members, decode_rdb, decode_rdb_prefix, encode_rdb, lzf_compress,
        lzf_decompress, rdb_decode_string, rdb_encode_length, rdb_encode_string,
    };

    fn append_rdb_checksum(encoded: &mut Vec<u8>) {
        let checksum = crc64_redis(encoded);
        encoded.extend_from_slice(&checksum.to_le_bytes());
    }

    fn rdb_encode_raw_stream_id(buf: &mut Vec<u8>, ms: u64, seq: u64) {
        buf.extend_from_slice(&ms.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
    }

    fn rdb_encode_millisecond_time(buf: &mut Vec<u8>, ms: u64) {
        buf.extend_from_slice(&ms.to_le_bytes());
    }

    fn encode_single_raw_rdb_entry(type_byte: u8, key: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut encoded = b"REDIS0011".to_vec();
        encoded.push(RDB_OPCODE_SELECTDB);
        rdb_encode_length(&mut encoded, 0);
        encoded.push(RDB_OPCODE_RESIZEDB);
        rdb_encode_length(&mut encoded, 1);
        rdb_encode_length(&mut encoded, 0);
        encoded.push(type_byte);
        rdb_encode_string(&mut encoded, key);
        encoded.extend_from_slice(payload);
        encoded.push(RDB_OPCODE_EOF);
        append_rdb_checksum(&mut encoded);
        encoded
    }

    #[cfg(feature = "upstream-stream-rdb")]
    struct ManagedRedis {
        child: std::process::Child,
    }

    #[cfg(feature = "upstream-stream-rdb")]
    impl Drop for ManagedRedis {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(feature = "upstream-stream-rdb")]
    fn project_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("manifest has workspace root")
            .to_path_buf()
    }

    #[cfg(feature = "upstream-stream-rdb")]
    fn pick_free_port() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
        listener.local_addr().expect("local addr").port()
    }

    #[cfg(feature = "upstream-stream-rdb")]
    fn wait_for_redis_cli(redis_cli: &std::path::Path, port: u16) -> bool {
        let port = port.to_string();
        for _ in 0..100 {
            if let Ok(output) = std::process::Command::new(redis_cli)
                .args(["-h", "127.0.0.1", "-p", port.as_str(), "--raw", "PING"])
                .output()
                && output.status.success()
                && output.stdout.starts_with(b"PONG")
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[cfg(feature = "upstream-stream-rdb")]
    fn redis_cli_output(redis_cli: &std::path::Path, port: u16, argv: &[&str]) -> String {
        let port = port.to_string();
        let output = std::process::Command::new(redis_cli)
            .args(["-h", "127.0.0.1", "-p", port.as_str(), "--raw"])
            .args(argv)
            .output()
            .expect("run redis-cli");
        assert!(
            output.status.success(),
            "redis-cli {:?} failed: {}",
            argv,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("redis-cli stdout is utf8")
    }

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
    fn rdb_opcode_contract_decodes_selectdb_resizedb_expiry_and_aux() {
        let entries = vec![RdbEntry {
            db: 7,
            key: b"contract-key".to_vec(),
            value: RdbValue::String(b"contract-value".to_vec()),
            expire_ms: Some(1_700_000_001_234),
        }];
        let encoded = encode_rdb(&entries, &[("future-aux-field", "ignored-safely")]);

        assert!(encoded.contains(&RDB_OPCODE_AUX));
        assert!(encoded.contains(&RDB_OPCODE_SELECTDB));
        assert!(encoded.contains(&RDB_OPCODE_RESIZEDB));
        assert!(encoded.contains(&RDB_OPCODE_EXPIRETIME_MS));

        let decoded = decode_rdb_prefix(&encoded).expect("required RDB opcodes must decode");
        assert_eq!(decoded.consumed, encoded.len());
        assert_eq!(decoded.entries, entries);
        assert_eq!(
            decoded.aux.get("future-aux-field").map(String::as_str),
            Some("ignored-safely")
        );
    }

    #[test]
    fn rdb_aux_contract_preserves_unknown_and_non_utf8_fields() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"REDIS0011");
        encoded.push(RDB_OPCODE_AUX);
        rdb_encode_string(&mut encoded, b"unknown-compatible-aux");
        rdb_encode_string(&mut encoded, b"preserved");
        encoded.push(RDB_OPCODE_AUX);
        rdb_encode_string(&mut encoded, b"\xFFbinary-key");
        rdb_encode_string(&mut encoded, b"\xFFbinary-value");
        encoded.push(RDB_OPCODE_EOF);
        append_rdb_checksum(&mut encoded);

        let decoded = decode_rdb_prefix(&encoded).expect("unknown AUX must be safe");
        let lossy_key = String::from_utf8_lossy(b"\xFFbinary-key").into_owned();
        let lossy_value = String::from_utf8_lossy(b"\xFFbinary-value").into_owned();

        assert_eq!(decoded.entries, Vec::new());
        assert_eq!(
            decoded
                .aux
                .get("unknown-compatible-aux")
                .map(String::as_str),
            Some("preserved")
        );
        assert_eq!(decoded.aux.get(&lossy_key), Some(&lossy_value));
    }

    #[test]
    fn rdb_rejects_unknown_mandatory_opcode_with_valid_checksum() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"REDIS0011");
        encoded.push(0xF4);
        encoded.push(RDB_OPCODE_EOF);
        append_rdb_checksum(&mut encoded);

        let err = decode_rdb_prefix(&encoded).expect_err("unknown mandatory opcode must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn rdb_rejects_expiry_opcode_not_followed_by_value_type() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"REDIS0011");
        encoded.push(RDB_OPCODE_EXPIRETIME_MS);
        encoded.extend_from_slice(&1_700_000_001_234_u64.to_le_bytes());
        encoded.push(RDB_OPCODE_SELECTDB);
        rdb_encode_length(&mut encoded, 1);
        encoded.push(RDB_OPCODE_EOF);
        append_rdb_checksum(&mut encoded);

        let err = decode_rdb_prefix(&encoded).expect_err("dangling expiry must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn rdb_prefix_decode_reports_consumed_length_before_aof_tail() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"preamble-key".to_vec(),
            value: RdbValue::String(b"preamble-value".to_vec()),
            expire_ms: None,
        }];
        let mut combined = encode_rdb(&entries, &[("redis-ver", "7.2.0")]);
        let rdb_len = combined.len();
        let tail_records = vec![AofRecord {
            argv: vec![
                b"SET".to_vec(),
                b"tail-key".to_vec(),
                b"tail-value".to_vec(),
            ],
        }];
        combined.extend_from_slice(&encode_aof_stream(&tail_records));

        let decoded = decode_rdb_prefix(&combined).expect("decode rdb preamble");

        assert_eq!(decoded.consumed, rdb_len);
        assert_eq!(decoded.entries, entries);
        assert_eq!(
            decoded.aux.get("redis-ver").map(String::as_str),
            Some("7.2.0")
        );
        let decoded_tail = decode_aof_stream(&combined[decoded.consumed..]).expect("decode tail");
        assert_eq!(decoded_tail, tail_records);
    }

    #[test]
    fn decode_aof_replay_stream_decodes_rdb_preamble_and_aof_tail() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"snapshot-key".to_vec(),
            value: RdbValue::String(b"snapshot-value".to_vec()),
            expire_ms: None,
        }];
        let mut combined = encode_rdb(&entries, &[("redis-ver", "7.2.4")]);
        let rdb_len = combined.len();
        let tail_records = vec![
            AofRecord {
                argv: vec![
                    b"SET".to_vec(),
                    b"tail-key".to_vec(),
                    b"tail-value".to_vec(),
                ],
            },
            AofRecord {
                argv: vec![b"INCR".to_vec(), b"tail-counter".to_vec()],
            },
        ];
        let first_tail_len = tail_records[0].to_resp_frame().to_bytes().len();
        combined.extend_from_slice(&encode_aof_stream(&tail_records));

        let replay = decode_aof_replay_stream(&combined).expect("decode mixed replay stream");
        let preamble = replay.rdb_preamble.expect("rdb preamble");

        assert_eq!(preamble.consumed, rdb_len);
        assert_eq!(preamble.entries, entries);
        assert_eq!(
            preamble.aux.get("redis-ver").map(String::as_str),
            Some("7.2.4")
        );
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.records[0].record, tail_records[0]);
        assert_eq!(replay.records[0].start_offset, rdb_len);
        assert_eq!(replay.records[0].end_offset, rdb_len + first_tail_len);
        assert_eq!(replay.records[1].record, tail_records[1]);
        assert_eq!(replay.records[1].start_offset, rdb_len + first_tail_len);
        assert_eq!(replay.records[1].end_offset, combined.len());
    }

    #[test]
    fn decode_aof_replay_stream_rejects_corrupt_rdb_preamble_before_tail() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"snapshot-key".to_vec(),
            value: RdbValue::String(b"snapshot-value".to_vec()),
            expire_ms: None,
        }];
        let mut combined = encode_rdb(&entries, &[]);
        let checksum_byte = combined.len() - 1;
        combined[checksum_byte] ^= 0x7F;
        combined.extend_from_slice(&encode_aof_stream(&[AofRecord {
            argv: vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()],
        }]));

        let err = decode_aof_replay_stream(&combined).expect_err("corrupt preamble must fail");
        assert_eq!(err, PersistError::InvalidFrame);
    }

    #[test]
    fn rdb_whole_file_decode_rejects_aof_tail_after_preamble() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"strict-key".to_vec(),
            value: RdbValue::String(b"strict-value".to_vec()),
            expire_ms: None,
        }];
        let mut combined = encode_rdb(&entries, &[]);
        combined.extend_from_slice(&encode_aof_stream(&[AofRecord {
            argv: vec![b"INCR".to_vec(), b"counter".to_vec()],
        }]));

        let err = decode_rdb(&combined).expect_err("strict decode must reject trailing AOF");
        assert_eq!(err, PersistError::InvalidFrame);
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

    // ── LZF encoder tests (br-frankenredis-1uin) ───────────────────────

    #[test]
    fn lzf_compress_round_trips_repetitive_payload() {
        // 256 bytes of repeating pattern compresses well; the round-trip
        // must restore the original byte-for-byte.
        let payload: Vec<u8> = b"ababababcdcdcdcdef"
            .iter()
            .copied()
            .cycle()
            .take(256)
            .collect();
        let compressed =
            lzf_compress(&payload, payload.len() - 4).expect("repetitive payload should compress");
        assert!(
            compressed.len() < payload.len(),
            "compressed size {} should be smaller than raw {}",
            compressed.len(),
            payload.len()
        );
        let restored = lzf_decompress(&compressed, payload.len()).expect("decompress round-trip");
        assert_eq!(restored, payload);
    }

    #[test]
    fn lzf_compress_round_trips_short_input() {
        // 5..32 byte inputs — boundary cases for the literal-run header
        // (MAX_LIT == 32) and the minimum compressible length.
        for &input in &[
            &b"hello"[..],
            &b"aaaaaaaaaa"[..],
            &b"abcdefghijklmnopqrstuvwxyz0123456"[..], // 32 bytes
            &b"abcdefghijklmnopqrstuvwxyz01234567"[..], // 33 bytes
        ] {
            // Use a generous budget so we test compressibility regardless
            // of overhead; round-trip must always restore.
            let budget = input.len() * 2 + 64;
            if let Some(compressed) = lzf_compress(input, budget) {
                let restored = lzf_decompress(&compressed, input.len())
                    .unwrap_or_else(|| panic!("decompress {input:?}"));
                assert_eq!(restored, input, "round-trip mismatch on {input:?}");
            }
        }
    }

    #[test]
    fn lzf_compress_round_trips_long_repetitive_payload() {
        // 4096 bytes of mostly-repeating content — exercises the
        // MAX_REF=264 backref-extension path and the extra-byte encoding
        // (top3 == 7 + extra byte).
        let mut payload = Vec::with_capacity(4096);
        for _ in 0..512 {
            payload.extend_from_slice(b"frankenredis_lzf");
        }
        let compressed =
            lzf_compress(&payload, payload.len() - 4).expect("4096B repetitive should compress");
        assert!(compressed.len() < payload.len() / 4);
        let restored = lzf_decompress(&compressed, payload.len()).expect("decompress");
        assert_eq!(restored, payload);
    }

    #[test]
    fn lzf_compress_returns_none_when_budget_exceeded() {
        // Random-looking bytes don't compress; with budget=in_len-4 we
        // expect None since LZF cannot save 5+ bytes.
        let payload: Vec<u8> = (0..20)
            .map(|i: u8| i.wrapping_mul(73).wrapping_add(31))
            .collect();
        let compressed = lzf_compress(&payload, payload.len() - 4);
        assert!(
            compressed.is_none(),
            "incompressible 20-byte payload should fail to fit in budget {}",
            payload.len() - 4
        );
    }

    #[test]
    fn rdb_encode_string_emits_lzf_for_long_compressible_string() {
        // Build an RDB containing a 256-byte highly compressible string.
        // The encode/decode path must round-trip and the wire form must
        // start with the 0xC3 special-encoding byte after the type tag
        // and key prefix.
        let payload: Vec<u8> = b"abc"[..].iter().copied().cycle().take(256).collect();
        let entries = vec![RdbEntry {
            db: 0,
            key: b"big".to_vec(),
            value: RdbValue::String(payload.clone()),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode lzf-encoded string");
        assert_eq!(decoded, entries);

        // After REDIS0011 + RESIZEDB headers + RDB_TYPE_STRING + key,
        // we expect the value to start with 0xC3 (LZF special encoding).
        // Search for the 0xC3 byte; it must be present somewhere.
        assert!(
            encoded.contains(&0xC3),
            "expected LZF marker (0xC3) in encoded RDB; raw byte search across {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn rdb_encode_string_emits_raw_for_short_string() {
        // Strings at or below upstream's 20-byte gate must skip the LZF
        // path entirely, avoiding compression overhead for short keys
        // and AUX values.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"k".to_vec(),
            value: RdbValue::String(b"short value".to_vec()),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        // LZF marker 0xC3 must NOT appear. (0xC3 is also a possible
        // CRC byte at the end so we restrict the search to the body.)
        let body_end = encoded.len().saturating_sub(8);
        assert!(
            !encoded[..body_end].contains(&0xC3),
            "did not expect LZF marker for short payload; encoded body: {:?}",
            &encoded[..body_end.min(48)]
        );
        let (decoded, _) = decode_rdb(&encoded).expect("decode short string");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_encode_string_emits_raw_when_compression_grows_payload() {
        // 22..30 byte random-looking strings. Many of these will fail
        // the budget check and fall back to raw; the round-trip must
        // still work either way.
        for seed in 0..16u8 {
            let payload: Vec<u8> = (0..28u8)
                .map(|i| seed.wrapping_mul(73).wrapping_add(i.wrapping_mul(101)))
                .collect();
            let entries = vec![RdbEntry {
                db: 0,
                key: format!("k{seed}").into_bytes(),
                value: RdbValue::String(payload.clone()),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);
            let (decoded, _) = decode_rdb(&encoded).expect("decode incompressible-ish");
            assert_eq!(decoded, entries, "round-trip drift for seed {seed}");
        }
    }

    // ── Compact-encoding RDB decoder tests (br-frankenredis-aqgx) ──────

    /// Encode a small listpack of byte-string entries, mirroring
    /// upstream `listpack.c::lpAppend`. Test-only — production callers
    /// live in `fr-store::dump_key`.
    fn build_listpack_for_test(entries: &[&[u8]]) -> Vec<u8> {
        fn push_backlen(buf: &mut Vec<u8>, len: usize) {
            if len <= 127 {
                buf.push(len as u8);
            } else if len < 16_383 {
                buf.push((len >> 7) as u8);
                buf.push(((len & 0x7F) as u8) | 0x80);
            } else {
                // Tests stay small enough that we never need >2-byte backlen.
                unreachable!("test listpack entries should not exceed 2-byte backlen");
            }
        }
        let mut entry_bytes = Vec::new();
        for entry in entries {
            let start = entry_bytes.len();
            // 6-bit literal string: tag 0x80 | len for len < 64.
            assert!(
                entry.len() < 64,
                "test helper only supports literal-string entries < 64 bytes"
            );
            entry_bytes.push(0x80 | entry.len() as u8);
            entry_bytes.extend_from_slice(entry);
            let data_len = entry_bytes.len() - start;
            push_backlen(&mut entry_bytes, data_len);
        }
        let total_bytes = (6 + entry_bytes.len() + 1) as u32;
        let entry_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        let mut out = Vec::with_capacity(total_bytes as usize);
        out.extend_from_slice(&total_bytes.to_le_bytes());
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&entry_bytes);
        out.push(0xFF);
        out
    }

    fn build_intset_for_test(values: &[i64]) -> Vec<u8> {
        // Pick the narrowest encoding that fits everything.
        let needs_64 = values
            .iter()
            .any(|v| !(i32::MIN as i64..=i32::MAX as i64).contains(v));
        let needs_32 = !needs_64
            && values
                .iter()
                .any(|v| !(i16::MIN as i64..=i16::MAX as i64).contains(v));
        let (encoding, width) = if needs_64 {
            (8u32, 8usize)
        } else if needs_32 {
            (4u32, 4usize)
        } else {
            (2u32, 2usize)
        };
        let mut out = Vec::with_capacity(8 + values.len() * width);
        out.extend_from_slice(&encoding.to_le_bytes());
        out.extend_from_slice(&(values.len() as u32).to_le_bytes());
        // Upstream sorts intset members; the decoder doesn't require it,
        // but real upstream-produced blobs always come sorted.
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        for v in sorted {
            match width {
                2 => out.extend_from_slice(&(v as i16).to_le_bytes()),
                4 => out.extend_from_slice(&(v as i32).to_le_bytes()),
                8 => out.extend_from_slice(&v.to_le_bytes()),
                _ => unreachable!(),
            }
        }
        out
    }

    /// Wrap a raw payload as a length-prefixed RDB string and append it
    /// to `buf` (the form used by RDB compact-encoding type tags).
    fn append_rdb_wrapped_string(buf: &mut Vec<u8>, data: &[u8]) {
        rdb_encode_length(buf, data.len());
        buf.extend_from_slice(data);
    }

    fn finalize_rdb_blob(payload: &mut Vec<u8>) -> Vec<u8> {
        payload.push(RDB_OPCODE_EOF);
        let checksum = crc64_redis(payload);
        payload.extend_from_slice(&checksum.to_le_bytes());
        std::mem::take(payload)
    }

    #[test]
    fn rdb_decodes_compact_set_intset() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_SET_INTSET);
        rdb_encode_string(&mut blob, b"si");
        let intset = build_intset_for_test(&[1, 2, 3, 5]);
        append_rdb_wrapped_string(&mut blob, &intset);
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode set_intset");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, b"si");
        match &entries[0].value {
            RdbValue::Set(members) => {
                let mut got: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
                got.sort();
                assert_eq!(got, vec![b"1" as &[u8], b"2", b"3", b"5"]);
            }
            other => panic!("expected RdbValue::Set, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_set_listpack() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_SET_LISTPACK);
        rdb_encode_string(&mut blob, b"slp");
        let lp = build_listpack_for_test(&[b"alpha", b"beta", b"gamma"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode set_listpack");
        match &entries[0].value {
            RdbValue::Set(members) => {
                let mut got: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
                got.sort();
                assert_eq!(got, vec![b"alpha" as &[u8], b"beta", b"gamma"]);
            }
            other => panic!("expected RdbValue::Set, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_hash_listpack() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_HASH_LISTPACK);
        rdb_encode_string(&mut blob, b"hlp");
        let lp = build_listpack_for_test(&[b"f1", b"v1", b"f2", b"v2"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode hash_listpack");
        match &entries[0].value {
            RdbValue::Hash(fields) => {
                assert_eq!(
                    fields,
                    &vec![
                        (b"f1".to_vec(), b"v1".to_vec()),
                        (b"f2".to_vec(), b"v2".to_vec()),
                    ]
                );
            }
            other => panic!("expected RdbValue::Hash, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_hash_listpack_rejects_odd_entry_count() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_HASH_LISTPACK);
        rdb_encode_string(&mut blob, b"bad");
        let lp = build_listpack_for_test(&[b"f1", b"v1", b"orphan"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        assert!(matches!(
            decode_rdb(&bytes),
            Err(PersistError::InvalidFrame)
        ));
    }

    #[test]
    fn rdb_decodes_compact_zset_listpack() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_ZSET_LISTPACK);
        rdb_encode_string(&mut blob, b"zlp");
        // Listpack is (member, score-as-string) pairs. Upstream stores
        // scores via lpAppend with the textual representation, e.g.
        // "1", "2.5", or "7.25".
        let lp = build_listpack_for_test(&[b"a", b"1", b"b", b"2.5", b"c", b"7.25"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode zset_listpack");
        match &entries[0].value {
            RdbValue::SortedSet(members) => {
                assert_eq!(members.len(), 3);
                assert_eq!(members[0].0, b"a");
                assert!((members[0].1 - 1.0).abs() < f64::EPSILON);
                assert_eq!(members[1].0, b"b");
                assert!((members[1].1 - 2.5).abs() < f64::EPSILON);
                assert_eq!(members[2].0, b"c");
                assert!((members[2].1 - 7.25).abs() < f64::EPSILON);
            }
            other => panic!("expected RdbValue::SortedSet, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_zset_listpack_rejects_non_numeric_score() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_ZSET_LISTPACK);
        rdb_encode_string(&mut blob, b"bad");
        let lp = build_listpack_for_test(&[b"a", b"not_a_number"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        assert!(matches!(
            decode_rdb(&bytes),
            Err(PersistError::InvalidFrame)
        ));
    }

    #[test]
    fn rdb_decodes_compact_list_quicklist_2_packed_node() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_LIST_QUICKLIST_2);
        rdb_encode_string(&mut blob, b"lq");
        // 1 node, container=2 (PACKED listpack), payload = listpack of "a","b","c".
        rdb_encode_length(&mut blob, 1);
        rdb_encode_length(&mut blob, 2);
        let lp = build_listpack_for_test(&[b"a", b"b", b"c"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode list_quicklist_2 packed");
        match &entries[0].value {
            RdbValue::List(items) => {
                assert_eq!(items, &vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
            }
            other => panic!("expected RdbValue::List, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_list_quicklist_2_plain_node() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_LIST_QUICKLIST_2);
        rdb_encode_string(&mut blob, b"lq_plain");
        // 1 node, container=1 (PLAIN), payload = the element bytes themselves.
        rdb_encode_length(&mut blob, 1);
        rdb_encode_length(&mut blob, 1);
        rdb_encode_string(&mut blob, b"single_plain_element");
        let bytes = finalize_rdb_blob(&mut blob);

        let (entries, _) = decode_rdb(&bytes).expect("decode list_quicklist_2 plain");
        match &entries[0].value {
            RdbValue::List(items) => {
                assert_eq!(items, &vec![b"single_plain_element".to_vec()]);
            }
            other => panic!("expected RdbValue::List, got {other:?}"),
        }
    }

    #[test]
    fn rdb_decodes_compact_list_quicklist_2_rejects_unknown_container() {
        let mut blob = Vec::new();
        blob.extend_from_slice(b"REDIS0011");
        blob.push(RDB_TYPE_LIST_QUICKLIST_2);
        rdb_encode_string(&mut blob, b"bad");
        rdb_encode_length(&mut blob, 1);
        rdb_encode_length(&mut blob, 99); // not 1 or 2
        let lp = build_listpack_for_test(&[b"x"]);
        append_rdb_wrapped_string(&mut blob, &lp);
        let bytes = finalize_rdb_blob(&mut blob);

        assert!(matches!(
            decode_rdb(&bytes),
            Err(PersistError::InvalidFrame)
        ));
    }

    #[test]
    fn intset_helper_decoder_handles_each_width() {
        // 16-bit
        let blob_16 = build_intset_for_test(&[-1, 0, 1, 32_000]);
        let got = decode_intset_members(&blob_16).expect("16-bit intset");
        assert_eq!(
            got,
            vec![
                b"-1".to_vec(),
                b"0".to_vec(),
                b"1".to_vec(),
                b"32000".to_vec()
            ]
        );

        // 32-bit (force >= 16-bit range)
        let blob_32 = build_intset_for_test(&[-100_000, 0, 100_000]);
        let got = decode_intset_members(&blob_32).expect("32-bit intset");
        assert_eq!(
            got,
            vec![b"-100000".to_vec(), b"0".to_vec(), b"100000".to_vec()]
        );

        // 64-bit (force >= 32-bit range)
        let blob_64 = build_intset_for_test(&[i64::MIN, 0, i64::MAX]);
        let got = decode_intset_members(&blob_64).expect("64-bit intset");
        assert_eq!(
            got,
            vec![
                i64::MIN.to_string().into_bytes(),
                b"0".to_vec(),
                i64::MAX.to_string().into_bytes(),
            ]
        );

        // Truncated buffer must reject.
        assert!(decode_intset_members(&[0; 4]).is_none());
    }

    /// Acceptance gate for the fuzz_rdb_decoder corpus seeds added in
    /// `fuzz/scripts/gen_compact_rdb_seeds.py`. Each well-formed seed
    /// must decode cleanly into the expected `RdbValue` shape; each
    /// adversarial seed must be rejected with `PersistError::InvalidFrame`
    /// without panicking.
    ///
    /// The corpus lives under `fuzz/corpus/fuzz_rdb_decoder/compact_*`
    /// — that's the directory libfuzzer reads when running
    /// `cargo fuzz run fuzz_rdb_decoder`. The script that emits these
    /// is idempotent and re-runnable; this test is the safety net so a
    /// future decoder change can't silently make a previously-handled
    /// seed regress.
    /// (br-frankenredis-aqgx fuzz follow-up)
    #[test]
    fn compact_corpus_seeds_decode_or_reject_cleanly() {
        let corpus_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/fuzz_rdb_decoder");
        if !corpus_root.exists() {
            // Fuzz corpus not present in this checkout — skip rather
            // than fail. (Some clean-room build environments strip the
            // fuzz/ tree.)
            eprintln!(
                "[SKIP] fuzz corpus dir {} not present",
                corpus_root.display()
            );
            return;
        }

        // Well-formed compact seeds: must decode cleanly to the matching
        // RdbValue shape.
        let well_formed = [
            ("compact_intset_small_16bit", "Set", 5usize),
            ("compact_intset_32bit", "Set", 3),
            ("compact_intset_64bit", "Set", 3),
            ("compact_set_listpack", "Set", 3),
            ("compact_set_listpack_single", "Set", 1),
            ("compact_hash_listpack", "Hash", 3),
            ("compact_zset_listpack", "SortedSet", 4),
            ("compact_quicklist2_packed", "List", 5),
            ("compact_quicklist2_plain", "List", 1),
            ("compact_quicklist2_mixed", "List", 3),
        ];

        for (name, expected_kind, expected_len) in well_formed {
            let path = corpus_root.join(name);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read corpus seed {}: {e}", path.display()));
            let (entries, _aux) =
                decode_rdb(&bytes).unwrap_or_else(|e| panic!("seed {name} should decode: {e:?}"));
            assert_eq!(entries.len(), 1, "seed {name} should produce 1 entry");
            let (kind, len) = match &entries[0].value {
                RdbValue::String(_) => ("String", 1),
                RdbValue::List(items) => ("List", items.len()),
                RdbValue::Set(members) => ("Set", members.len()),
                RdbValue::Hash(fields) => ("Hash", fields.len()),
                RdbValue::SortedSet(members) => ("SortedSet", members.len()),
                _ => ("Other", 0),
            };
            assert_eq!(
                kind, expected_kind,
                "seed {name} decoded to wrong RdbValue kind: got {kind}, expected {expected_kind}"
            );
            assert_eq!(
                len, expected_len,
                "seed {name} decoded to wrong cardinality: got {len}, expected {expected_len}"
            );
        }

        // Adversarial seeds: must be rejected (Err) without panic.
        let adversarial = [
            "compact_intset_truncated",
            "compact_intset_invalid_encoding",
            "compact_hash_listpack_odd",
            "compact_zset_listpack_non_numeric",
            "compact_quicklist2_unknown_container",
            "compact_listpack_truncated",
        ];

        for name in adversarial {
            let path = corpus_root.join(name);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("read corpus seed {}: {e}", path.display()));
            assert!(
                decode_rdb(&bytes).is_err(),
                "adversarial seed {name} should be rejected by decode_rdb",
            );
        }
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
                None,
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
            value: RdbValue::Stream(vec![], None, Vec::new(), None),
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
                None,
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
                None,
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[cfg(feature = "upstream-stream-rdb")]
    #[test]
    fn rdb_feature_encodes_streams_as_upstream_type21() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"cg_stream".to_vec(),
            value: RdbValue::Stream(
                vec![(1000, 0, vec![(b"msg".to_vec(), b"hello".to_vec())])],
                Some((1000, 0)),
                vec![RdbStreamConsumerGroup {
                    name: b"mygroup".to_vec(),
                    last_delivered_id_ms: 1000,
                    last_delivered_id_seq: 0,
                    consumers: vec![b"alice".to_vec()],
                    pending: vec![RdbStreamPendingEntry {
                        entry_id_ms: 1000,
                        entry_id_seq: 0,
                        consumer: b"alice".to_vec(),
                        deliveries: 2,
                        last_delivered_ms: 5000,
                    }],
                }],
                None,
            ),
            expire_ms: None,
        }];

        let encoded = encode_rdb(&entries, &[]);

        // Header + SELECTDB(2) + RESIZEDB(3) puts the first value type byte at 14.
        assert_eq!(encoded[14], UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3);

        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded.len(), 1);
        let RdbValue::Stream(decoded_entries, decoded_watermark, decoded_groups, metadata) =
            &decoded[0].value
        else {
            panic!("expected decoded stream");
        };
        let RdbValue::Stream(expected_entries, expected_watermark, expected_groups, _) =
            &entries[0].value
        else {
            panic!("expected source stream");
        };
        assert_eq!(decoded_entries, expected_entries);
        assert_eq!(decoded_watermark, expected_watermark);
        assert_eq!(decoded_groups, expected_groups);
        assert!(
            metadata.is_some(),
            "upstream stream decode should retain raw payload metadata"
        );
    }

    #[cfg(feature = "upstream-stream-rdb")]
    #[test]
    fn rdb_feature_type21_streams_load_in_vendored_redis() {
        let root = project_root();
        let redis_server = root.join("legacy_redis_code/redis/src/redis-server");
        let redis_cli = root.join("legacy_redis_code/redis/src/redis-cli");
        if !redis_server.is_file() || !redis_cli.is_file() {
            eprintln!(
                "[SKIP] vendored redis-server/redis-cli unavailable under {}",
                root.display()
            );
            return;
        }

        let fixture_count = 20_u64;
        let entries: Vec<RdbEntry> = (0..fixture_count)
            .map(|fixture| {
                let key = format!("stream:{fixture}").into_bytes();
                let first_value = format!("fixture-{fixture}-first").into_bytes();
                let second_value = format!("fixture-{fixture}-second").into_bytes();
                let ms = 1000 + fixture;
                let groups = if fixture % 5 == 0 {
                    vec![RdbStreamConsumerGroup {
                        name: format!("group-{fixture}").into_bytes(),
                        last_delivered_id_ms: ms,
                        last_delivered_id_seq: 1,
                        consumers: vec![b"alice".to_vec()],
                        pending: vec![RdbStreamPendingEntry {
                            entry_id_ms: ms,
                            entry_id_seq: 0,
                            consumer: b"alice".to_vec(),
                            deliveries: fixture + 1,
                            last_delivered_ms: 50_000 + fixture,
                        }],
                    }]
                } else {
                    Vec::new()
                };

                RdbEntry {
                    db: 0,
                    key,
                    value: RdbValue::Stream(
                        vec![
                            (ms, 0, vec![(b"field".to_vec(), first_value)]),
                            (
                                ms,
                                1,
                                vec![
                                    (b"field".to_vec(), second_value),
                                    (b"extra".to_vec(), format!("extra-{fixture}").into_bytes()),
                                ],
                            ),
                        ],
                        Some((ms, 1)),
                        groups,
                        None,
                    ),
                    expire_ms: None,
                }
            })
            .collect();

        let port = pick_free_port();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fr_persist_upstream_stream_rdb_{}_{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).expect("create temp redis dir");
        let dump_path = dir.join("dump.rdb");
        std::fs::write(&dump_path, encode_rdb(&entries, &[])).expect("write upstream rdb");

        let child = std::process::Command::new(&redis_server)
            .arg("--dir")
            .arg(&dir)
            .arg("--dbfilename")
            .arg("dump.rdb")
            .arg("--port")
            .arg(port.to_string())
            .arg("--bind")
            .arg("127.0.0.1")
            .arg("--protected-mode")
            .arg("no")
            .arg("--save")
            .arg("")
            .arg("--appendonly")
            .arg("no")
            .arg("--daemonize")
            .arg("no")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn vendored redis-server");
        let _redis = ManagedRedis { child };
        assert!(
            wait_for_redis_cli(&redis_cli, port),
            "vendored redis-server did not become ready"
        );

        for fixture in 0..fixture_count {
            let key = format!("stream:{fixture}");
            let output = redis_cli_output(&redis_cli, port, &["XINFO", "STREAM", &key, "FULL"]);
            assert!(
                output.contains(&format!("fixture-{fixture}-first")),
                "missing first entry in XINFO STREAM FULL for {key}: {output}"
            );
            assert!(
                output.contains(&format!("fixture-{fixture}-second")),
                "missing second entry in XINFO STREAM FULL for {key}: {output}"
            );
            assert!(
                output.contains(&format!("extra-{fixture}")),
                "missing multi-field entry in XINFO STREAM FULL for {key}: {output}"
            );
            if fixture % 5 == 0 {
                assert!(
                    output.contains(&format!("group-{fixture}")) && output.contains("alice"),
                    "missing consumer-group metadata in XINFO STREAM FULL for {key}: {output}"
                );
            }
        }

        let _ = std::fs::remove_file(dump_path);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn rdb_hash_with_ttls_uses_private_non_upstream_stream_type_tag() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"httl".to_vec(),
            value: RdbValue::HashWithTtls(vec![
                (b"persist".to_vec(), b"v0".to_vec(), None),
                (b"expiring".to_vec(), b"v1".to_vec(), Some(123_456)),
            ]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);

        // Header + SELECTDB(2) + RESIZEDB(3) puts the first value type byte at 14.
        assert_eq!(encoded[14], RDB_TYPE_HASH_WITH_TTLS);
        assert_ne!(encoded[14], UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3);

        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded, entries);
    }

    #[test]
    fn rdb_decodes_upstream_type21_stream_consumer_groups() {
        let mut payload = Vec::new();
        rdb_encode_length(&mut payload, 0); // listpacks_count
        rdb_encode_length(&mut payload, 0); // stream length
        rdb_encode_length(&mut payload, 42); // last_id.ms
        rdb_encode_length(&mut payload, 7); // last_id.seq
        rdb_encode_length(&mut payload, 42); // first_id.ms
        rdb_encode_length(&mut payload, 7); // first_id.seq
        rdb_encode_length(&mut payload, 0); // max_deleted_id.ms
        rdb_encode_length(&mut payload, 0); // max_deleted_id.seq
        rdb_encode_length(&mut payload, 1); // entries_added
        rdb_encode_length(&mut payload, 1); // groups_count

        rdb_encode_string(&mut payload, b"g");
        rdb_encode_length(&mut payload, 42); // group last_id.ms
        rdb_encode_length(&mut payload, 7); // group last_id.seq
        rdb_encode_length(&mut payload, 1); // entries_read
        rdb_encode_length(&mut payload, 1); // global PEL count
        rdb_encode_raw_stream_id(&mut payload, 42, 7);
        rdb_encode_millisecond_time(&mut payload, 1000);
        rdb_encode_length(&mut payload, 3); // delivery_count
        rdb_encode_length(&mut payload, 1); // consumers_count
        rdb_encode_string(&mut payload, b"alice");
        rdb_encode_millisecond_time(&mut payload, 1100); // seen_time
        rdb_encode_millisecond_time(&mut payload, 1200); // active_time
        rdb_encode_length(&mut payload, 1); // consumer PEL count
        rdb_encode_raw_stream_id(&mut payload, 42, 7);

        let encoded =
            encode_single_raw_rdb_entry(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, b"stream", &payload);
        let (decoded, _) = decode_rdb(&encoded).expect("decode type21 stream");
        assert_eq!(
            decoded,
            vec![RdbEntry {
                db: 0,
                key: b"stream".to_vec(),
                value: RdbValue::Stream(
                    Vec::new(),
                    Some((42, 7)),
                    vec![RdbStreamConsumerGroup {
                        name: b"g".to_vec(),
                        last_delivered_id_ms: 42,
                        last_delivered_id_seq: 7,
                        consumers: vec![b"alice".to_vec()],
                        pending: vec![RdbStreamPendingEntry {
                            entry_id_ms: 42,
                            entry_id_seq: 7,
                            consumer: b"alice".to_vec(),
                            deliveries: 3,
                            last_delivered_ms: 1000,
                        }],
                    }],
                    Some(RdbStreamMetadata {
                        upstream_type_byte: UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3,
                        upstream_payload: payload.clone(),
                    }),
                ),
                expire_ms: None,
            }]
        );
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
                None,
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
                    None,
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
                value: RdbValue::Stream(vec![], None, Vec::new(), None),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            // Private TYPE_STREAM = 15 (0x0F) remains the default encoding.
            // Empty streams without a watermark also keep this shape under the
            // upstream feature because type-21 always decodes a concrete last-id.
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
                        RdbValue::Stream(entries, watermark, groups, None)
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

        // ── LZF encoder fuzz (br-frankenredis-xmr8 follow-up) ──────────
        //
        // Property-based coverage of the LZF compressor — the round-trip
        // identity, never-panic on arbitrary input, and budget-rejection
        // contract. Compression itself is generative (subject to the
        // input's compressibility and the budget), so the strategy
        // mixes pure-random byte vectors (low compressibility) with
        // repetition-heavy mosaics (high compressibility) so both code
        // paths in the encoder (literal-only and backref) are exercised.

        fn lzf_input_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
            // Three flavors:
            //   1. Random bytes
            //   2. Repeated byte (always compresses well)
            //   3. Tile of small random pattern (also compresses well)
            prop_oneof![
                prop::collection::vec(any::<u8>(), 0..=max_len),
                (any::<u8>(), 0..=max_len).prop_map(|(b, len)| vec![b; len]),
                (
                    prop::collection::vec(any::<u8>(), 1..=8),
                    1..=(max_len.max(1) / 4 + 1),
                )
                    .prop_map(|(pattern, reps)| {
                        let mut out = Vec::with_capacity(pattern.len() * reps);
                        for _ in 0..reps {
                            out.extend_from_slice(&pattern);
                        }
                        out
                    }),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1024))]

            /// LZF compress never panics on arbitrary input + budget combinations.
            #[test]
            fn lzf_compress_never_panics(
                input in lzf_input_strategy(2048),
                budget in 0usize..=8192,
            ) {
                let _ = lzf_compress(&input, budget);
            }

            /// Round-trip identity: when lzf_compress succeeds, lzf_decompress
            /// must reconstruct the original input byte-for-byte.
            #[test]
            fn lzf_compress_decompress_round_trips(
                input in lzf_input_strategy(4096),
            ) {
                // Use a generous budget so the encoder doesn't spuriously
                // reject; we're stressing the round-trip, not the budget.
                let budget = input.len() * 2 + 64;
                if let Some(compressed) = lzf_compress(&input, budget) {
                    let restored = lzf_decompress(&compressed, input.len());
                    prop_assert!(
                        restored.is_some(),
                        "round-trip decode failed for input.len()={}, compressed.len()={}",
                        input.len(),
                        compressed.len(),
                    );
                    prop_assert_eq!(restored.unwrap(), input.clone());
                }
            }

            /// Budget contract: when lzf_compress returns Some, the output
            /// length is strictly within the caller's budget.
            #[test]
            fn lzf_compress_respects_budget(
                input in lzf_input_strategy(2048),
                budget in 1usize..=4096,
            ) {
                if let Some(compressed) = lzf_compress(&input, budget) {
                    prop_assert!(
                        compressed.len() <= budget,
                        "encoder returned Some({}) but exceeded budget {}",
                        compressed.len(),
                        budget,
                    );
                }
            }

            /// rdb_encode_string + rdb_decode_string round-trips for any
            /// byte vector, regardless of whether the LZF heuristic chose
            /// the compressed or raw form. This is the production
            /// invariant that matters: encoders and decoders agree.
            #[test]
            fn rdb_encode_decode_string_round_trips(
                input in lzf_input_strategy(8192),
            ) {
                let mut buf = Vec::new();
                rdb_encode_string(&mut buf, &input);
                let (decoded, consumed) = rdb_decode_string(&buf)
                    .expect("rdb_decode_string must succeed on rdb_encode_string output");
                prop_assert_eq!(decoded, input.clone(), "round-trip drift");
                prop_assert_eq!(consumed, buf.len(), "decoder must consume exactly the encoded bytes");
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
                        RdbValue::Stream(entries, watermark, groups, None)
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
