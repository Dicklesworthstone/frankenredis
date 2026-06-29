#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
#[cfg(feature = "upstream-stream-rdb")]
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use fr_protocol::{RespFrame, RespParseError};

pub mod listpack;
#[allow(dead_code)]
pub(crate) mod rdb_stream;
pub mod ziplist;

pub(crate) fn decimal_i64_scratch(value: i64) -> ([u8; 20], usize) {
    let mut scratch = [0u8; 20];
    let end = scratch.len();
    let mut start = fr_protocol::write_u64_digits(&mut scratch, end, value.unsigned_abs());
    if value < 0 {
        start -= 1;
        scratch[start] = b'-';
    }
    (scratch, start)
}

pub(crate) fn decimal_i64_bytes(value: i64) -> Vec<u8> {
    let (scratch, start) = decimal_i64_scratch(value);
    scratch[start..].to_vec()
}

thread_local! {
    static LZF_SCRATCH: RefCell<LzfScratch> = const { RefCell::new(LzfScratch::new()) };
}

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

        if raw_line.len() > AOF_MANIFEST_MAX_LINE {
            return Err(manifest_parse_error(line_number, "line too long"));
        }

        let line = raw_line.trim_matches(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'));
        if line.is_empty() || line.as_bytes().first() == Some(&b'#') {
            continue;
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

    // Reject manifests that contained only blank/comment lines and no actual entries.
    // Such manifests format to empty strings which then fail to reparse.
    if manifest.base.is_none() && manifest.history.is_empty() && manifest.incremental.is_empty() {
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

/// A loaded multi-part AOF (redis 7 `appendonlydir`): the base RDB payload
/// (present only when the base file is RDB-format) plus the ordered AOF command
/// records from an AOF-format base file followed by every incremental file.
/// (frankenredis-nvcby)
#[derive(Debug, Clone, Default)]
pub struct MultipartAofLoad {
    pub base_rdb_entries: Vec<RdbEntry>,
    pub base_rdb_functions: Vec<Vec<u8>>,
    pub records: Vec<AofRecord>,
}

/// Load a redis 7 multi-part AOF from its manifest file. Data files named in
/// the manifest are resolved relative to the manifest's directory; `history`
/// entries are skipped (they are superseded). The base file is decoded as RDB
/// (`*.base.rdb`, the `aof-use-rdb-preamble yes` default) or as an AOF stream
/// (`*.base.aof`); incrementals are AOF streams. The result preserves replay
/// order: base RDB first, then base-AOF records (if any), then incr records.
pub fn read_aof_manifest_dir(manifest_path: &Path) -> Result<MultipartAofLoad, PersistError> {
    let manifest = read_aof_manifest_file(manifest_path)?;
    let dir = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| Path::new(".").to_path_buf(), Path::to_path_buf);
    let mut out = MultipartAofLoad::default();
    for entry in manifest.replay_entries() {
        let data = std::fs::read(dir.join(&entry.file_name)).map_err(PersistError::Io)?;
        if data.is_empty() {
            continue;
        }
        let is_rdb_base =
            entry.file_type == AofManifestFileType::Base && entry.file_name.ends_with(".rdb");
        if is_rdb_base {
            let decoded = decode_rdb_prefix(&data)?;
            out.base_rdb_entries = decoded.entries;
            out.base_rdb_functions = decoded.functions;
        } else {
            out.records.extend(decode_aof_stream(&data)?);
        }
    }
    Ok(out)
}

/// Write a redis 7 multi-part AOF (`appendonlydir`) into `dir`: a base RDB
/// snapshot, an incremental AOF file, and the manifest that points at both.
///
/// Files are named `<basename>.<seq>.base.rdb`, `<basename>.<seq>.incr.aof`,
/// and `<basename>.manifest` — exactly the layout redis-server loads and the
/// inverse of [`read_aof_manifest_dir`]. The data files are written (atomically,
/// temp + rename + parent fsync) before the manifest, so a concurrent reader
/// never sees a manifest referencing a not-yet-present file. (frankenredis-aofw1)
pub fn write_aof_manifest_dir(
    dir: &Path,
    basename: &str,
    seq: u64,
    base_rdb: &[u8],
    incr_records: &[AofRecord],
) -> Result<(), PersistError> {
    std::fs::create_dir_all(dir).map_err(PersistError::Io)?;
    let base_file = format!("{basename}.{seq}.base.rdb");
    let incr_file = format!("{basename}.{seq}.incr.aof");

    write_rdb_bytes_atomically(&dir.join(&base_file), base_rdb)?;
    write_rdb_bytes_atomically(&dir.join(&incr_file), &encode_aof_stream(incr_records))?;

    let manifest = AofManifest {
        base: Some(AofManifestEntry {
            file_name: base_file,
            file_seq: seq,
            file_type: AofManifestFileType::Base,
        }),
        history: Vec::new(),
        incremental: vec![AofManifestEntry {
            file_name: incr_file,
            file_seq: seq,
            file_type: AofManifestFileType::Incremental,
        }],
        curr_base_file_seq: seq,
        curr_incr_file_seq: seq,
    };
    write_rdb_bytes_atomically(
        &dir.join(format!("{basename}.manifest")),
        format_aof_manifest(&manifest).as_bytes(),
    )
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

    /// Byte length of this record's RESP multibulk wire encoding, computed
    /// WITHOUT materializing the frame or the encoded bytes. Exactly equals
    /// `self.to_resp_frame().to_bytes().len()` (asserted in tests). The hot
    /// AOF/replication propagation path only needs this length for offset
    /// accounting; the prior `to_resp_frame().to_bytes().len()` cloned EVERY
    /// argument's bytes into a `Vec<RespFrame>` AND allocated+encoded the full
    /// wire `Vec<u8>` per write — for a replicated/AOF 256 KiB SET that is
    /// ~2× the value bytes copied solely to be counted and dropped. This is
    /// O(argc) arithmetic, zero allocation. (frankenredis-cc aofreclen)
    #[must_use]
    pub fn encoded_resp_len(&self) -> usize {
        // Decimal digit count, matching `push_usize`'s output width.
        fn decimal_len(n: usize) -> usize {
            if n == 0 { 1 } else { n.ilog10() as usize + 1 }
        }
        // Header: `*<argc>\r\n`
        let mut len = 1 + decimal_len(self.argv.len()) + 2;
        // Each element: `$<len>\r\n<bytes>\r\n`
        for arg in &self.argv {
            len += 1 + decimal_len(arg.len()) + 2 + arg.len() + 2;
        }
        len
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
    // (CrimsonHawk) Encode each record's argv DIRECTLY as a RESP multibulk into `out`
    // via the borrow-encode helpers, instead of `record.to_resp_frame().to_bytes()`
    // which cloned every arg into a `RespFrame`, allocated a fresh `to_bytes` Vec, then
    // copied it into `out` (3 allocs+copies per record). Byte-identical to the
    // RespFrame::Array(BulkString…) form. Measured -67.6% (3.1x) on a 10k-record AOF
    // rewrite chunk (isolated A/B).
    let mut out = Vec::new();
    for record in records {
        fr_protocol::encode_aggregate_header(record.argv.len(), false, &mut out);
        for arg in &record.argv {
            fr_protocol::encode_bulk_string_slice(Some(arg), false, &mut out);
        }
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
// Recursion depth limit for nested RESP arrays. 64 is plenty for any
// valid Redis command while preventing stack overflow from malicious
// deeply-nested input (the parser recurses before checking depth).
const AOF_MAX_RECURSION_DEPTH: usize = 64;

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
/// RDB_OPCODE_FUNCTION2 (245): a function-library payload written by redis
/// 7.0+ at the head of the RDB (one opcode per registered library), each
/// followed by a single raw string holding the library source code. We must
/// consume it — failing here discarded the ENTIRE dump (every key after the
/// functions was lost) whenever `FUNCTION LOAD` had been used.
/// (br-frankenredis-rdb-function2)
const RDB_OPCODE_FUNCTION2: u8 = 0xF5;

/// Pre-size cap for collection element vectors during RDB decode. `count` comes
/// from an untrusted RDB header, so the speculative reservation is bounded to
/// avoid OOM amplification from a hostile small-header/huge-count payload — but at
/// the prior 1024 any collection in its hashtable/skiplist/full encoding (hash >
/// 512 fields, set > 128, etc.) grew its outer Vec ~log2(count/1024) realloc+copy
/// times during load. 65536 pre-sizes the overwhelming majority of real large
/// collections in one allocation while keeping the worst-case speculative reserve
/// bounded (~1.5–3 MiB of element structs). Capacity never affects content.
/// (frankenredis-cc lzfcap sibling)
const RDB_COLLECTION_PRESIZE_CAP: usize = 1 << 16;

/// RDB value type tags.
const RDB_TYPE_STRING: u8 = 0;
const RDB_TYPE_LIST: u8 = 1;
const RDB_TYPE_SET: u8 = 2;
const RDB_TYPE_ZSET: u8 = 3; // Legacy zset: ASCII-string double scores (redis ≤ 6.2)
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
/// Legacy ziplist/zipmap encodings written by redis ≤ 6.2 (and the old
/// quicklist whose nodes are ziplists). Modern redis upgrades these to listpack
/// on load; fr-persist decodes them via the `ziplist` module so dumps from older
/// redis releases load instead of failing closed. (br-frankenredis-rdb-ziplist)
const RDB_TYPE_HASH_ZIPMAP: u8 = 9;
const RDB_TYPE_LIST_ZIPLIST: u8 = 10;
const RDB_TYPE_ZSET_ZIPLIST: u8 = 12;
const RDB_TYPE_HASH_ZIPLIST: u8 = 13;
const RDB_TYPE_LIST_QUICKLIST: u8 = 14;
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
const CRC64_REDIS_REFLECTED_POLY: u64 = reflect_u64(CRC64_REDIS_POLY);
const CRC64_REDIS_TABLE: [u64; 256] = build_crc64_redis_table();

/// A key-value entry for RDB serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbEntry {
    pub db: usize,
    pub key: Vec<u8>,
    pub value: RdbValue,
    pub expire_ms: Option<u64>,
}

/// Borrowed string-only RDB entry for snapshot paths that can prove all values
/// are plain strings and preserve the canonical `(db, key)` order upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdbStringEntryRef<'a> {
    pub db: usize,
    pub key: &'a [u8],
    pub value: &'a [u8],
    pub expire_ms: Option<u64>,
}

/// Decoded RDB payload plus the byte offset immediately after the checksum.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbDecodeResult {
    pub entries: Vec<RdbEntry>,
    pub aux: BTreeMap<String, String>,
    pub consumed: usize,
    /// Function-library source payloads (RDB_OPCODE_FUNCTION2), in file
    /// order. Empty unless the dump registered `FUNCTION`/`FCALL` libraries.
    /// Captured so the runtime can re-register them; their presence no longer
    /// aborts the whole load.
    pub functions: Vec<Vec<u8>>,
}

/// Stream entry: (ms, seq, fields).
pub type StreamEntry = EncodableStreamEntry<Vec<u8>, Vec<u8>>;
pub type EncodableStreamEntry<F, V> = (u64, u64, Vec<(F, V)>);

/// A pending entry in a consumer group (PEL entry).
#[derive(Debug, Clone, PartialEq)]
pub struct RdbStreamPendingEntry {
    pub entry_id_ms: u64,
    pub entry_id_seq: u64,
    pub consumer: Vec<u8>,
    pub deliveries: u64,
    pub last_delivered_ms: u64,
}

/// A single consumer within a persisted consumer group, carrying the
/// per-consumer timestamps redis stores in the RDB (STREAM_LISTPACKS_2 adds
/// `seen_time`, _3 adds `active_time`). `active_time_ms` is `None` for the
/// upstream `-1` sentinel ("never actively consumed"). (frankenredis-sq4ov)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdbStreamConsumer {
    pub name: Vec<u8>,
    pub seen_time_ms: u64,
    pub active_time_ms: Option<u64>,
}

impl RdbStreamConsumer {
    /// Construct a consumer whose `seen_time`/`active_time` are unknown
    /// (defaults to 0 / never-active). Convenience for callers/tests that only
    /// have the name.
    #[must_use]
    pub fn named(name: Vec<u8>) -> Self {
        Self {
            name,
            seen_time_ms: 0,
            active_time_ms: None,
        }
    }
}

/// A consumer group persisted in an RDB snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct RdbStreamConsumerGroup {
    pub name: Vec<u8>,
    pub last_delivered_id_ms: u64,
    pub last_delivered_id_seq: u64,
    pub entries_read: Option<u64>,
    pub consumers: Vec<RdbStreamConsumer>,
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
    /// Encode-only fast path for lists that already hold Redis-shaped
    /// QUICKLIST_2 PACKED listpack nodes in memory. Decoding still returns
    /// `List`, so load/apply paths keep a single canonical semantic shape.
    ListQuicklist2Packed(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    /// A set that is HASHTABLE-encoded (upstream `RDB_TYPE_SET`, the plain
    /// count-prefixed member list). Distinct from `Set` (which re-derives
    /// intset/listpack/hashtable from content+thresholds) so a sticky hashtable
    /// set whose final content happens to fit listpack still round-trips as
    /// `hashtable` across a whole-DB save/load — upstream saves by ENCODING, not
    /// by re-deriving from content. (frankenredis-39is8)
    SetHashtable(Vec<Vec<u8>>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    /// Redis 7.4 hash with per-field TTLs. Each tuple is
    /// (field, value, Some(abs_deadline_ms)) for a TTL'd field or
    /// (field, value, None) for a field without a TTL. Encoded via
    /// RDB_TYPE_HASH_WITH_TTLS (100). (br-frankenredis-th7q)
    HashWithTtls(Vec<(Vec<u8>, Vec<u8>, Option<u64>)>),
    SortedSet(Vec<(Vec<u8>, f64)>),
    /// Stream: entries + optional watermark + consumer groups + raw upstream
    /// metadata (for byte-exact replay) + entries-added counter +
    /// max-deleted-entry-id watermark (`None` == `0-0`, i.e. nothing deleted).
    Stream(
        Vec<StreamEntry>,
        Option<(u64, u64)>,
        Vec<RdbStreamConsumerGroup>,
        Option<RdbStreamMetadata>,
        Option<u64>,
        Option<(u64, u64)>,
    ),
}

/// Encode a Redis 7.2+ STREAM_LISTPACKS_3 payload for DUMP/RESTORE values.
///
/// The returned bytes start after the type byte and before the DUMP trailer.
#[must_use]
pub fn encode_upstream_stream_listpacks3_payload<F, V>(
    entries: &[EncodableStreamEntry<F, V>],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
    entries_added: Option<u64>,
    max_deleted: Option<(u64, u64)>,
) -> Option<Vec<u8>>
where
    F: AsRef<[u8]> + Clone,
    V: AsRef<[u8]> + Clone,
{
    rdb_stream::encode_upstream_stream_listpacks3(
        entries,
        watermark,
        groups,
        entries_added,
        max_deleted,
    )
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
    if let Ok(len) = u8::try_from(data.len())
        && len <= 20
    {
        buf.push(len);
        buf.extend_from_slice(data);
        return;
    }

    // Upstream's compressed-fits budget: `out_len = in_len - 4`.
    // lzf_compress returns None if it can't fit within that.
    let budget = data.len() - 4;
    if let Some(compressed) = lzf_compress(data, budget) {
        let raw_size = rdb_length_size(data.len()) + data.len();
        let lzf_size =
            1 + rdb_length_size(compressed.len()) + rdb_length_size(data.len()) + compressed.len();
        if lzf_size < raw_size {
            buf.push(0xC3);
            rdb_encode_length(buf, compressed.len());
            rdb_encode_length(buf, data.len());
            buf.extend_from_slice(&compressed);
            return;
        }
    }
    rdb_encode_length(buf, data.len());
    buf.extend_from_slice(data);
}

const fn reflect_u64(mut data: u64) -> u64 {
    let mut reflected = data & 1;
    let mut bit = 1;
    while bit < 64 {
        data >>= 1;
        reflected = (reflected << 1) | (data & 1);
        bit += 1;
    }
    reflected
}

const fn crc64_redis_table_entry(index: u8) -> u64 {
    let mut crc = index as u64;
    let mut bit = 0;
    while bit < 8 {
        if (crc & 1) != 0 {
            crc = (crc >> 1) ^ CRC64_REDIS_REFLECTED_POLY;
        } else {
            crc >>= 1;
        }
        bit += 1;
    }
    crc
}

const fn build_crc64_redis_table() -> [u64; 256] {
    let mut table = [0_u64; 256];
    let mut index = 0;
    while index < 256 {
        table[index] = crc64_redis_table_entry(index as u8);
        index += 1;
    }
    table
}

/// Slice-by-16 acceleration tables for `crc64_redis`. `table[0]` is the standard
/// byte table; `table[k][n]` folds `table[0][n]` through `k` additional byte
/// steps. This is the const-built equivalent of Redis `crcspeed`'s little-endian
/// table init, extended from slice-by-8 to slice-by-16 so the main loop consumes
/// 16 input bytes per iteration via two little-endian word loads + sixteen table
/// lookups. Halving the iteration count (and exposing two independent word loads
/// for ILP) measured -10.5% time on 1MB/4KB and -28% on 64B payloads vs the
/// slice-by-8 form, interleaved isolated A/B — CRC runs over the full payload on
/// every DUMP/RESTORE/RDB checksum, so fr now beats Redis 7.2.4's slice-by-8.
/// (frankenredis-3qhkr; slice-by-16 CrimsonHawk)
const fn build_crc64_redis_slice() -> [[u64; 256]; 16] {
    let mut tables = [[0_u64; 256]; 16];
    tables[0] = CRC64_REDIS_TABLE;
    let mut n = 0;
    while n < 256 {
        let mut crc = tables[0][n];
        let mut k = 1;
        while k < 16 {
            crc = tables[0][(crc & 0xff) as usize] ^ (crc >> 8);
            tables[k][n] = crc;
            k += 1;
        }
        n += 1;
    }
    tables
}

const CRC64_REDIS_SLICE: [[u64; 256]; 16] = build_crc64_redis_slice();

/// Redis CRC-64 (Jones reflected polynomial), slice-by-16. Folds 16 input bytes
/// per iteration through `CRC64_REDIS_SLICE` (two word loads), with a byte-wise
/// remainder tail identical to the classic single-table form. Bit-identical to
/// the byte-at-a-time CRC (the slice tables are derived from the same byte
/// table), so DUMP / RESTORE / RDB checksums are unchanged. (frankenredis-3qhkr;
/// slice-by-16 CrimsonHawk)
pub fn crc64_redis(data: &[u8]) -> u64 {
    let mut crc = 0_u64;
    let mut chunks = data.chunks_exact(16);
    for chunk in chunks.by_ref() {
        let one = u64::from_le_bytes(chunk[0..8].try_into().unwrap()) ^ crc;
        let two = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        crc = CRC64_REDIS_SLICE[15][(one & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[14][((one >> 8) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[13][((one >> 16) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[12][((one >> 24) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[11][((one >> 32) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[10][((one >> 40) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[9][((one >> 48) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[8][((one >> 56) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[7][(two & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[6][((two >> 8) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[5][((two >> 16) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[4][((two >> 24) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[3][((two >> 32) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[2][((two >> 40) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[1][((two >> 48) & 0xff) as usize]
            ^ CRC64_REDIS_SLICE[0][((two >> 56) & 0xff) as usize];
    }
    for &byte in chunks.remainder() {
        crc = (crc >> 8) ^ CRC64_REDIS_SLICE[0][((crc as u8) ^ byte) as usize];
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

/// Compact-encoding thresholds for `encode_rdb_with_options`. When
/// `Some`, the encoder selects upstream-compatible compact RDB type
/// tags (`RDB_TYPE_SET_INTSET=11`, `_SET_LISTPACK=20`,
/// `_HASH_LISTPACK=16`, `_ZSET_LISTPACK=17`, `_LIST_QUICKLIST_2=18`)
/// for shapes whose cardinality / per-element byte size fits within
/// the matching threshold. When `None`, the encoder always emits the
/// canonical (non-compact) tags 1/2/4/5 — preserving the historical
/// `encode_rdb` behavior. Defaults mirror Redis 7.2 vanilla.
/// (br-frankenredis-91kt)
#[derive(Debug, Clone, Copy)]
pub struct CompactRdbThresholds {
    pub hash_max_listpack_entries: usize,
    pub hash_max_listpack_value: usize,
    pub set_max_intset_entries: usize,
    pub set_max_listpack_entries: usize,
    pub set_max_listpack_value: usize,
    pub zset_max_listpack_entries: usize,
    pub zset_max_listpack_value: usize,
    /// Upper bound on the listpack body emitted for an
    /// `RDB_TYPE_LIST_QUICKLIST_2` PACKED node (mirrors upstream
    /// `list-max-listpack-size`'s byte interpretation, default 8 KiB).
    pub list_max_listpack_size: usize,
}

impl Default for CompactRdbThresholds {
    fn default() -> Self {
        Self {
            // (frankenredis-0o5hj) Upstream Redis 7.2.4 default is 512
            // (config.c:3215). Prior comment claiming 128 was wrong.
            hash_max_listpack_entries: 512,
            hash_max_listpack_value: 64,
            set_max_intset_entries: 512,
            set_max_listpack_entries: 128,
            set_max_listpack_value: 64,
            zset_max_listpack_entries: 128,
            zset_max_listpack_value: 64,
            list_max_listpack_size: 8192,
        }
    }
}

/// Options for `encode_rdb_with_options`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RdbEncodeOptions {
    /// When `Some`, emit compact RDB type tags for shapes within the
    /// supplied thresholds. When `None`, always emit canonical tags.
    pub compact: Option<CompactRdbThresholds>,
}

/// Encode a complete RDB file using the supplied encoding options.
#[must_use]
pub fn encode_rdb_with_options(
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
    options: RdbEncodeOptions,
) -> Vec<u8> {
    encode_rdb_internal(entries, aux, &[], options)
}

/// Encode a complete RDB file including FUNCTION library payloads.
///
/// Each entry of `functions` is a library's source code, emitted as a
/// `RDB_OPCODE_FUNCTION2` record at the head of the file (after aux, before the
/// keyspace) exactly as redis writes it, so the libraries survive a save/load
/// round-trip. (frankenredis-tm139)
#[must_use]
pub fn encode_rdb_with_functions(
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
    functions: &[&[u8]],
) -> Vec<u8> {
    encode_rdb_internal(
        entries,
        aux,
        functions,
        RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        },
    )
}

/// Encode a complete RDB file including FUNCTION libraries, choosing compact
/// type tags against the supplied (live) thresholds rather than the compiled
/// defaults. (frankenredis-2j9wz) The snapshot path must save each collection by
/// its ACTUAL in-memory encoding: a listpack set/hash that fits only because the
/// runtime raised `*-max-listpack-entries` above the default would otherwise be
/// emitted as the plain `RDB_TYPE_SET`/`RDB_TYPE_HASH` and reload with a
/// different (hashtable) encoding, breaking DUMP byte-stability across DEBUG
/// RELOAD. Passing the live thresholds keeps the saved tag in lock-step with the
/// in-memory encoding the thresholds produced.
#[must_use]
pub fn encode_rdb_with_functions_and_thresholds(
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
    functions: &[&[u8]],
    thresholds: CompactRdbThresholds,
) -> Vec<u8> {
    encode_rdb_internal(
        entries,
        aux,
        functions,
        RdbEncodeOptions {
            compact: Some(thresholds),
        },
    )
}

/// Encode a complete RDB file for an already-sorted, string-only keyspace.
///
/// This mirrors `encode_rdb_with_functions` for `RdbValue::String` entries but
/// avoids materializing `RdbEntry`/`RdbValue` vectors when the runtime can borrow
/// store keys and values directly. Callers must pass entries sorted by `(db,
/// key)`; debug builds assert that contract.
#[must_use]
pub fn encode_rdb_string_entries_with_functions(
    entries: &[RdbStringEntryRef<'_>],
    aux: &[(&str, &str)],
    functions: &[&[u8]],
) -> Vec<u8> {
    debug_assert!(entries.windows(2).all(|pair| {
        pair[0].db < pair[1].db || (pair[0].db == pair[1].db && pair[0].key <= pair[1].key)
    }));

    let mut buf = Vec::new();

    buf.extend_from_slice(b"REDIS");
    let version_str = format!("{RDB_VERSION:04}");
    buf.extend_from_slice(version_str.as_bytes());

    for (key, value) in aux {
        buf.push(RDB_OPCODE_AUX);
        rdb_encode_string(&mut buf, key.as_bytes());
        rdb_encode_string(&mut buf, value.as_bytes());
    }

    for code in functions {
        buf.push(RDB_OPCODE_FUNCTION2);
        rdb_encode_string(&mut buf, code);
    }

    let mut group_start = 0usize;
    while group_start < entries.len() {
        let db = entries[group_start].db;
        let mut group_end = group_start;
        let mut db_expires = 0usize;
        while group_end < entries.len() && entries[group_end].db == db {
            if entries[group_end].expire_ms.is_some() {
                db_expires += 1;
            }
            group_end += 1;
        }

        buf.push(RDB_OPCODE_SELECTDB);
        rdb_encode_length(&mut buf, db);
        buf.push(RDB_OPCODE_RESIZEDB);
        rdb_encode_length(&mut buf, group_end - group_start);
        rdb_encode_length(&mut buf, db_expires);

        for entry in &entries[group_start..group_end] {
            if let Some(ms) = entry.expire_ms {
                buf.push(RDB_OPCODE_EXPIRETIME_MS);
                buf.extend_from_slice(&ms.to_le_bytes());
            }

            buf.push(RDB_TYPE_STRING);
            rdb_encode_string(&mut buf, entry.key);
            rdb_encode_string(&mut buf, entry.value);
        }
        group_start = group_end;
    }

    buf.push(RDB_OPCODE_EOF);
    let checksum = crc64_redis(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());

    buf
}

/// Encode a complete RDB file from a set of entries.
///
/// Convenience wrapper around `encode_rdb_with_options` using upstream
/// Redis's default compact-encoding thresholds. Small lists, sets,
/// hashes, and sorted sets therefore re-emit with the compact type tags
/// Redis itself would choose (`LIST_QUICKLIST_2`, `SET_INTSET`,
/// `SET_LISTPACK`, `HASH_LISTPACK`, `ZSET_LISTPACK`) instead of the
/// canonical-only tags.
#[must_use]
pub fn encode_rdb(entries: &[RdbEntry], aux: &[(&str, &str)]) -> Vec<u8> {
    encode_rdb_internal(
        entries,
        aux,
        &[],
        RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        },
    )
}

fn encode_rdb_internal(
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
    functions: &[&[u8]],
    options: RdbEncodeOptions,
) -> Vec<u8> {
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

    // FUNCTION libraries are written after aux and before the keyspace, one
    // RDB_OPCODE_FUNCTION2 record (the library source) each.
    for code in functions {
        buf.push(RDB_OPCODE_FUNCTION2);
        rdb_encode_string(&mut buf, code);
    }

    let mut sorted_entries: Vec<&RdbEntry> = entries.iter().collect();
    sorted_entries.sort_by(|left, right| {
        left.db
            .cmp(&right.db)
            .then_with(|| left.key.cmp(&right.key))
    });

    let mut group_start = 0usize;
    while group_start < sorted_entries.len() {
        let db = sorted_entries[group_start].db;
        let mut group_end = group_start;
        let mut db_expires = 0usize;
        while group_end < sorted_entries.len() && sorted_entries[group_end].db == db {
            if sorted_entries[group_end].expire_ms.is_some() {
                db_expires += 1;
            }
            group_end += 1;
        }

        buf.push(RDB_OPCODE_SELECTDB);
        rdb_encode_length(&mut buf, db);
        buf.push(RDB_OPCODE_RESIZEDB);
        rdb_encode_length(&mut buf, group_end - group_start);
        rdb_encode_length(&mut buf, db_expires);

        for &entry in &sorted_entries[group_start..group_end] {
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
                    if let Some(thresholds) = options.compact.as_ref()
                        && let Some(payload) = encode_compact_list_quicklist2(items, thresholds)
                    {
                        buf.push(RDB_TYPE_LIST_QUICKLIST_2);
                        rdb_encode_string(&mut buf, &entry.key);
                        buf.extend_from_slice(&payload);
                    } else {
                        buf.push(RDB_TYPE_LIST);
                        rdb_encode_string(&mut buf, &entry.key);
                        rdb_encode_length(&mut buf, items.len());
                        for item in items {
                            rdb_encode_string(&mut buf, item);
                        }
                    }
                }
                RdbValue::ListQuicklist2Packed(nodes) => {
                    let payload = encode_quicklist2_packed_payload(nodes);
                    buf.push(RDB_TYPE_LIST_QUICKLIST_2);
                    rdb_encode_string(&mut buf, &entry.key);
                    buf.extend_from_slice(&payload);
                }
                RdbValue::Set(members) => {
                    if let Some(thresholds) = options.compact.as_ref() {
                        if let Some(payload) = encode_compact_set_intset(members, thresholds) {
                            buf.push(RDB_TYPE_SET_INTSET);
                            rdb_encode_string(&mut buf, &entry.key);
                            buf.extend_from_slice(&payload);
                        } else if let Some(payload) =
                            encode_compact_set_listpack(members, thresholds)
                        {
                            buf.push(RDB_TYPE_SET_LISTPACK);
                            rdb_encode_string(&mut buf, &entry.key);
                            buf.extend_from_slice(&payload);
                        } else {
                            buf.push(RDB_TYPE_SET);
                            rdb_encode_string(&mut buf, &entry.key);
                            rdb_encode_length(&mut buf, members.len());
                            for member in members {
                                rdb_encode_string(&mut buf, member);
                            }
                        }
                    } else {
                        buf.push(RDB_TYPE_SET);
                        rdb_encode_string(&mut buf, &entry.key);
                        rdb_encode_length(&mut buf, members.len());
                        for member in members {
                            rdb_encode_string(&mut buf, member);
                        }
                    }
                }
                RdbValue::SetHashtable(members) => {
                    // (frankenredis-39is8) A hashtable-encoded set always emits
                    // the plain RDB_TYPE_SET, regardless of whether its content
                    // would otherwise fit intset/listpack — matching upstream's
                    // save-by-encoding so the encoding survives a save/load.
                    buf.push(RDB_TYPE_SET);
                    rdb_encode_string(&mut buf, &entry.key);
                    rdb_encode_length(&mut buf, members.len());
                    for member in members {
                        rdb_encode_string(&mut buf, member);
                    }
                }
                RdbValue::Hash(fields) => {
                    if let Some(thresholds) = options.compact.as_ref()
                        && let Some(payload) = encode_compact_hash_listpack(fields, thresholds)
                    {
                        buf.push(RDB_TYPE_HASH_LISTPACK);
                        rdb_encode_string(&mut buf, &entry.key);
                        buf.extend_from_slice(&payload);
                    } else {
                        buf.push(RDB_TYPE_HASH);
                        rdb_encode_string(&mut buf, &entry.key);
                        rdb_encode_length(&mut buf, fields.len());
                        for (field, value) in fields {
                            rdb_encode_string(&mut buf, field);
                            rdb_encode_string(&mut buf, value);
                        }
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
                    if let Some(thresholds) = options.compact.as_ref()
                        && let Some(payload) = encode_compact_zset_listpack(members, thresholds)
                    {
                        buf.push(RDB_TYPE_ZSET_LISTPACK);
                        rdb_encode_string(&mut buf, &entry.key);
                        buf.extend_from_slice(&payload);
                    } else {
                        buf.push(RDB_TYPE_ZSET_2);
                        rdb_encode_string(&mut buf, &entry.key);
                        rdb_encode_length(&mut buf, members.len());
                        for (member, score) in members {
                            rdb_encode_string(&mut buf, member);
                            // ZSET2 encoding: 8-byte LE double
                            buf.extend_from_slice(&score.to_le_bytes());
                        }
                    }
                }
                RdbValue::Stream(
                    stream_entries,
                    watermark,
                    groups,
                    metadata,
                    entries_added,
                    max_deleted,
                ) => {
                    encode_stream_rdb_value(
                        &mut buf,
                        &entry.key,
                        StreamRdbValueParts {
                            entries: stream_entries,
                            watermark: *watermark,
                            groups,
                            metadata,
                            entries_added: *entries_added,
                            max_deleted: *max_deleted,
                        },
                    );
                }
            }
        }
        group_start = group_end;
    }

    // EOF
    buf.push(RDB_OPCODE_EOF);
    let checksum = crc64_redis(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());

    buf
}

// ── Compact-shape selection (br-frankenredis-91kt) ─────────────────
//
// Each helper below returns `Some(payload)` if the input fits the
// supplied threshold and the encoded form is well-formed, or `None`
// to signal "fall back to the canonical (non-compact) RDB type tag."
// `payload` does NOT include the leading type byte or the key — those
// are emitted by `encode_rdb_internal` per-value-kind.

fn encode_compact_set_intset(
    members: &[Vec<u8>],
    thresholds: &CompactRdbThresholds,
) -> Option<Vec<u8>> {
    if members.len() > thresholds.set_max_intset_entries {
        return None;
    }
    // Every member must parse as an i64 with a canonical decimal
    // representation matching itself (rejects "+1", "01", " 1", etc.
    // which upstream's intset path also refuses). Use the shared allocation-free
    // canonical parser instead of formatting a String per candidate member.
    let mut values = Vec::with_capacity(members.len());
    let mut width = 2u32;
    for raw in members {
        let value = parse_listpack_integer(raw)?;
        if width < 8 && i16::try_from(value).is_err() {
            if i32::try_from(value).is_ok() {
                width = 4;
            } else {
                width = 8;
            }
        }
        values.push(value);
    }
    let blob = encode_intset_blob(values, width)?;
    let mut out = Vec::with_capacity(blob.len() + 4);
    rdb_encode_length(&mut out, blob.len());
    out.extend_from_slice(&blob);
    Some(out)
}

fn encode_compact_set_listpack(
    members: &[Vec<u8>],
    thresholds: &CompactRdbThresholds,
) -> Option<Vec<u8>> {
    if members.len() > thresholds.set_max_listpack_entries {
        return None;
    }
    if members
        .iter()
        .any(|m| m.len() > thresholds.set_max_listpack_value)
    {
        return None;
    }
    let lp = encode_set_listpack_blob(members)?;
    let mut out = Vec::with_capacity(lp.len() + 4);
    // Upstream rdbSaveObject persists a listpack via rdbSaveRawString, which LZF-
    // compresses it when it is large enough to beat the wire overhead (>20 bytes
    // and the compressed form is smaller). Emitting it raw made DUMP/RDB diverge
    // from redis for large listpack hashes/sets/zsets (a 200-field hash dumped
    // 2200 bytes vs redis's 1560). (frankenredis listpack DUMP LZF parity)
    rdb_encode_string(&mut out, &lp);
    Some(out)
}

fn encode_set_listpack_blob(members: &[Vec<u8>]) -> Option<Vec<u8>> {
    // Pre-size to a safe upper bound (each listpack string entry is <= len + ~10 of
    // type-header + backlen; int-encoded entries are shorter) so the blob is built in ONE
    // allocation instead of growing from empty (≈log2(size) realloc+copies per key on the
    // bulk RDB-save path). Under-estimates are harmless (Vec just grows); output is
    // byte-identical. (frankenredis perf: presize listpack blob, code-first batch-test pending)
    let cap = LISTPACK_BLOB_OVERHEAD + members.iter().map(|m| m.len() + 11).sum::<usize>();
    let mut encoded = listpack_blob_with_header(cap);
    for member in members {
        encode_listpack_entry(&mut encoded, member);
    }
    finish_listpack_blob(encoded, members.len())
}

fn encode_compact_hash_listpack(
    fields: &[(Vec<u8>, Vec<u8>)],
    thresholds: &CompactRdbThresholds,
) -> Option<Vec<u8>> {
    if fields.len() > thresholds.hash_max_listpack_entries {
        return None;
    }
    if fields.iter().any(|(f, v)| {
        f.len() > thresholds.hash_max_listpack_value || v.len() > thresholds.hash_max_listpack_value
    }) {
        return None;
    }
    let lp = encode_hash_listpack_blob(fields)?;
    let mut out = Vec::with_capacity(lp.len() + 4);
    // Upstream rdbSaveObject persists a listpack via rdbSaveRawString, which LZF-
    // compresses it when it is large enough to beat the wire overhead (>20 bytes
    // and the compressed form is smaller). Emitting it raw made DUMP/RDB diverge
    // from redis for large listpack hashes/sets/zsets (a 200-field hash dumped
    // 2200 bytes vs redis's 1560). (frankenredis listpack DUMP LZF parity)
    rdb_encode_string(&mut out, &lp);
    Some(out)
}

fn encode_hash_listpack_blob(fields: &[(Vec<u8>, Vec<u8>)]) -> Option<Vec<u8>> {
    // Pre-size to a safe upper bound (two entries per field, each <= len + ~10) so the blob
    // is built in one allocation. Under-estimates are harmless; output byte-identical.
    // (frankenredis perf: presize listpack blob, code-first batch-test pending)
    let cap = LISTPACK_BLOB_OVERHEAD
        + fields
            .iter()
            .map(|(f, v)| f.len() + v.len() + 22)
            .sum::<usize>();
    let mut encoded = listpack_blob_with_header(cap);
    for (field, value) in fields {
        encode_listpack_entry(&mut encoded, field);
        encode_listpack_entry(&mut encoded, value);
    }
    finish_listpack_blob(encoded, fields.len().saturating_mul(2))
}

fn encode_compact_zset_listpack(
    members: &[(Vec<u8>, f64)],
    thresholds: &CompactRdbThresholds,
) -> Option<Vec<u8>> {
    if members.len() > thresholds.zset_max_listpack_entries {
        return None;
    }
    if members
        .iter()
        .any(|(m, _)| m.len() > thresholds.zset_max_listpack_value)
    {
        return None;
    }
    // Reject NaN scores — upstream's zset listpack path uses
    // `d2string` which doesn't represent NaN; the wire form would be
    // unparseable on the read side.
    if members.iter().any(|(_, score)| score.is_nan()) {
        return None;
    }

    let lp = if zset_members_are_sorted(members) {
        encode_zset_score_listpack_blob_from_members(members)?
    } else {
        let mut sorted_members: Vec<(&[u8], f64)> = members
            .iter()
            .map(|(member, score)| (member.as_slice(), *score))
            .collect();
        sorted_members.sort_by(|left, right| zset_member_cmp(*left, *right));
        encode_zset_score_listpack_blob(&sorted_members)?
    };
    let mut out = Vec::with_capacity(lp.len() + 4);
    // Upstream rdbSaveObject persists a listpack via rdbSaveRawString, which LZF-
    // compresses it when it is large enough to beat the wire overhead (>20 bytes
    // and the compressed form is smaller). Emitting it raw made DUMP/RDB diverge
    // from redis for large listpack hashes/sets/zsets (a 200-field hash dumped
    // 2200 bytes vs redis's 1560). (frankenredis listpack DUMP LZF parity)
    rdb_encode_string(&mut out, &lp);
    Some(out)
}

fn zset_member_cmp(left: (&[u8], f64), right: (&[u8], f64)) -> std::cmp::Ordering {
    left.1
        .partial_cmp(&right.1)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| left.0.cmp(right.0))
}

fn zset_members_are_sorted(members: &[(Vec<u8>, f64)]) -> bool {
    members.windows(2).all(|pair| {
        zset_member_cmp(
            (pair[0].0.as_slice(), pair[0].1),
            (pair[1].0.as_slice(), pair[1].1),
        ) != std::cmp::Ordering::Greater
    })
}

fn encode_zset_score_listpack_blob_from_members(
    sorted_members: &[(Vec<u8>, f64)],
) -> Option<Vec<u8>> {
    let cap = LISTPACK_BLOB_OVERHEAD
        + sorted_members
            .iter()
            .map(|(m, _)| m.len() + 11 + 32)
            .sum::<usize>();
    let mut encoded = listpack_blob_with_header(cap);
    for (member, score) in sorted_members {
        encode_zset_score_listpack_entry(&mut encoded, member, *score);
    }
    finish_listpack_blob(encoded, sorted_members.len().saturating_mul(2))
}

fn encode_zset_score_listpack_blob(sorted_members: &[(&[u8], f64)]) -> Option<Vec<u8>> {
    // Pre-size to a safe upper bound: per member, the member entry (<= len + ~10) plus the
    // score entry (a d2string/i64 decimal, always <= ~21 chars + ~10 header => budget 32) so
    // the blob is built in one allocation. Under-estimates are harmless; output byte-identical.
    // (frankenredis perf: presize listpack blob, code-first batch-test pending)
    let cap = LISTPACK_BLOB_OVERHEAD
        + sorted_members
            .iter()
            .map(|(m, _)| m.len() + 11 + 32)
            .sum::<usize>();
    let mut encoded = listpack_blob_with_header(cap);
    for (member, score) in sorted_members {
        encode_zset_score_listpack_entry(&mut encoded, member, *score);
    }
    finish_listpack_blob(encoded, sorted_members.len().saturating_mul(2))
}

fn encode_zset_score_listpack_entry(encoded: &mut Vec<u8>, member: &[u8], score: f64) {
    encode_listpack_entry(encoded, member);
    if let Some(score) = zset_listpack_integer_score(score) {
        let (scratch, start) = decimal_i64_scratch(score);
        encode_listpack_entry(encoded, &scratch[start..]);
    } else {
        let score = format!("{score}");
        encode_listpack_entry(encoded, score.as_bytes());
    }
}

fn zset_listpack_integer_score(score: f64) -> Option<i64> {
    if score.fract() == 0.0 && (-1e18..=1e18).contains(&score) && score.is_finite() {
        Some(score as i64)
    } else {
        None
    }
}

fn encode_compact_list_quicklist2(
    items: &[Vec<u8>],
    thresholds: &CompactRdbThresholds,
) -> Option<Vec<u8>> {
    // Upstream lists are ALWAYS quicklist-encoded (RDB_TYPE_LIST_QUICKLIST_2):
    // items are split into PACKED listpack nodes each bounded by
    // list_max_listpack_size (default 8192 B), with an over-budget single item
    // emitted as a PLAIN node (container=1). The previous form only handled the
    // single-node case and returned None for a large all-small-item list,
    // forcing the caller onto the legacy RDB_TYPE_LIST (type 1) — which redis
    // 7.2 never emits. Build the multi-node form, tracking each node's listpack
    // byte size incrementally (listpack_entry_encoded_len is O(1)) so packing
    // stays O(n) and the node boundaries match the DUMP encoder
    // (encode_dump_quicklist2 / frankenredis-list-dump-quicklist-reencode-q5ody).
    if items.is_empty() {
        return None;
    }
    let budget = thresholds.list_max_listpack_size;
    let mut buf = Vec::new();
    rdb_encode_length(&mut buf, quicklist2_node_count(items, budget));
    // Keep a borrowed slice roster for each PACKED node and let the shared
    // listpack encoder build the node payload. A direct streaming encoder was
    // measured slower on the focused quicklist RDB gate, so the buffered path
    // remains the production path while the 1 GiB threshold below preserves
    // Redis-compatible PLAIN/PACKED classification.
    let mut packed: Vec<&[u8]> = Vec::new();
    let mut packed_bytes = LISTPACK_BLOB_OVERHEAD;
    let flush = |packed: &mut Vec<&[u8]>, buf: &mut Vec<u8>| -> Option<()> {
        if !packed.is_empty() {
            let lp = encode_listpack_strings_blob(packed)?;
            rdb_encode_length(buf, 2); // PACKED
            rdb_encode_string(buf, &lp);
            packed.clear();
        }
        Some(())
    };
    const QUICKLIST_PACKED_THRESHOLD: usize = 1 << 30;
    for item in items {
        if item.len() >= QUICKLIST_PACKED_THRESHOLD {
            // (frankenredis-1z4ba) Upstream marks a node PLAIN only when
            // isLargeElement(sz) = sz >= packed_threshold (1<<30, 1 GiB). A merely
            // over-budget element (>budget but <1 GiB) is NOT plain — it becomes its OWN
            // 1-element PACKED listpack node (handled by the budget-flush in the packing
            // path below; node_count counts an over-budget element as its own node either
            // way, so the count stays consistent). Previously `item.len() > budget` made
            // any >8 KiB element a PLAIN node, diverging from redis's DUMP bytes.
            flush(&mut packed, &mut buf)?;
            packed_bytes = LISTPACK_BLOB_OVERHEAD;
            rdb_encode_length(&mut buf, 1); // PLAIN
            rdb_encode_string(&mut buf, item);
            continue;
        }
        let entry_bytes = listpack_entry_encoded_len(item);
        if !packed.is_empty() && packed_bytes + entry_bytes > budget {
            flush(&mut packed, &mut buf)?;
            packed_bytes = LISTPACK_BLOB_OVERHEAD;
        }
        packed.push(item.as_slice());
        packed_bytes += entry_bytes;
    }
    flush(&mut packed, &mut buf)?;
    Some(buf)
}

fn quicklist2_node_count(items: &[Vec<u8>], budget: usize) -> usize {
    let mut node_count = 0;
    let mut packed_has_items = false;
    let mut packed_bytes = LISTPACK_BLOB_OVERHEAD;
    for item in items {
        if item.len() > budget {
            if packed_has_items {
                node_count += 1;
                packed_has_items = false;
                packed_bytes = LISTPACK_BLOB_OVERHEAD;
            }
            node_count += 1;
            continue;
        }
        let entry_bytes = listpack_entry_encoded_len(item);
        if packed_has_items && packed_bytes + entry_bytes > budget {
            node_count += 1;
            packed_bytes = LISTPACK_BLOB_OVERHEAD;
        }
        packed_has_items = true;
        packed_bytes += entry_bytes;
    }
    if packed_has_items {
        node_count += 1;
    }
    node_count
}

fn listpack_blob_header_matches(blob: &[u8]) -> bool {
    if blob.len() < listpack::LISTPACK_HEADER_SIZE + 1
        || blob.last() != Some(&listpack::LISTPACK_EOF)
    {
        return false;
    }
    let total = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    total == blob.len()
}

fn encode_quicklist2_packed_payload(nodes: &[Vec<u8>]) -> Vec<u8> {
    debug_assert!(!nodes.is_empty());
    debug_assert!(nodes.iter().all(|node| listpack_blob_header_matches(node)));
    let mut buf = Vec::new();
    rdb_encode_length(&mut buf, nodes.len());
    for node in nodes {
        rdb_encode_length(&mut buf, 2);
        rdb_encode_string(&mut buf, node);
    }
    buf
}

struct StreamRdbValueParts<'a> {
    entries: &'a [StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &'a [RdbStreamConsumerGroup],
    metadata: &'a Option<RdbStreamMetadata>,
    entries_added: Option<u64>,
    max_deleted: Option<(u64, u64)>,
}

fn encode_stream_rdb_value(buf: &mut Vec<u8>, key: &[u8], stream: StreamRdbValueParts<'_>) {
    if let Some(metadata) = stream.metadata {
        buf.push(metadata.upstream_type_byte);
        rdb_encode_string(buf, key);
        buf.extend_from_slice(&metadata.upstream_payload);
        return;
    }

    #[cfg(feature = "upstream-stream-rdb")]
    {
        if can_encode_upstream_stream_losslessly(stream.entries, stream.watermark, stream.groups)
            && let Some(payload) = rdb_stream::encode_upstream_stream_listpacks3(
                stream.entries,
                stream.watermark,
                stream.groups,
                stream.entries_added,
                stream.max_deleted,
            )
        {
            buf.push(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3);
            rdb_encode_string(buf, key);
            buf.extend_from_slice(&payload);
            return;
        }
    }

    encode_private_stream_rdb_value(
        buf,
        key,
        stream.entries,
        stream.watermark,
        stream.groups,
        stream.entries_added,
        stream.max_deleted,
    );
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
            .any(|consumer| consumer.name.as_slice() == pending.consumer.as_slice())
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
                .filter(|pending| pending.consumer.as_slice() == consumer.name.as_slice()),
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
    entries_added: Option<u64>,
    max_deleted: Option<(u64, u64)>,
) {
    buf.push(RDB_TYPE_STREAM);
    rdb_encode_string(buf, key);
    let (wm_ms, wm_seq) = watermark.unwrap_or((0, 0));
    buf.extend_from_slice(&wm_ms.to_le_bytes());
    buf.extend_from_slice(&wm_seq.to_le_bytes());
    let entries_added =
        entries_added.unwrap_or(u64::try_from(stream_entries.len()).unwrap_or(u64::MAX));
    buf.extend_from_slice(&entries_added.to_le_bytes());
    // max-deleted-entry-id watermark (frankenredis-fplrm); 0-0 == nothing deleted.
    let (md_ms, md_seq) = max_deleted.unwrap_or((0, 0));
    buf.extend_from_slice(&md_ms.to_le_bytes());
    buf.extend_from_slice(&md_seq.to_le_bytes());
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
        buf.extend_from_slice(&group.entries_read.unwrap_or(u64::MAX).to_le_bytes());
        rdb_encode_length(buf, group.consumers.len());
        for consumer in &group.consumers {
            rdb_encode_string(buf, &consumer.name);
            // Per-consumer seen_time (u64 LE) + active_time (i64 LE, -1 == None),
            // so fr's native stream RDB round-trips them too. (frankenredis-sq4ov)
            buf.extend_from_slice(&consumer.seen_time_ms.to_le_bytes());
            buf.extend_from_slice(
                &consumer
                    .active_time_ms
                    .map_or(-1i64, |v| v as i64)
                    .to_le_bytes(),
            );
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
        members.push(decimal_i64_bytes(value));
    }
    Some(members)
}

// ── Compact-encoding emitters (br-frankenredis-91kt) ────────────────
//
// Mirror the encoder helpers in `fr-store::dump_key` (DUMP/RESTORE
// path) so RDB blobs emitted by `fr-persist::encode_rdb_with_options`
// can pick the same compact RDB type tags upstream's `redis-server`
// would have chosen for shapes within the listpack/intset thresholds.
// fr-store can't be a dep of fr-persist (the dep arrow goes the other
// way), so the helpers are duplicated here. Both sides round-trip
// through the shared `decode_listpack` / `decode_intset_members`
// readers.

/// Encode a sorted-ascending intset blob (8-byte header + per-element
/// little-endian fixed-width values). Returns `None` when any value
/// exceeds the i64 range or `len > u32::MAX`.
fn encode_intset_blob(mut values: Vec<i64>, width: u32) -> Option<Vec<u8>> {
    // Take the parsed values by value and sort in place — the sole caller
    // (encode_compact_set_intset) builds this Vec fresh and discards it, so owning it lets us
    // skip the extra `to_vec()` allocation+copy per intset DUMP/RDB-save. Sorting the owned Vec
    // vs a copy yields the identical canonical order => byte-identical output.
    // (frankenredis perf: intset encode sorts in place, code-first batch-test pending)
    debug_assert!(matches!(width, 2 | 4 | 8));
    let len = u32::try_from(values.len()).ok()?;
    values.sort_unstable();
    let sorted = values;
    let mut out = Vec::with_capacity(8usize.saturating_add(sorted.len() * width as usize));
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    for value in &sorted {
        match width {
            2 => out.extend_from_slice(&i16::try_from(*value).ok()?.to_le_bytes()),
            4 => out.extend_from_slice(&i32::try_from(*value).ok()?.to_le_bytes()),
            8 => out.extend_from_slice(&value.to_le_bytes()),
            _ => unreachable!("width is one of 2, 4, 8"),
        }
    }
    Some(out)
}

fn encode_listpack_backlen(buf: &mut Vec<u8>, len: usize) {
    if len <= 127 {
        buf.push(len as u8);
    } else if len < 16_383 {
        buf.push((len >> 7) as u8);
        buf.push(((len & 0x7F) as u8) | 0x80);
    } else if len < 2_097_151 {
        buf.push((len >> 14) as u8);
        buf.push((((len >> 7) & 0x7F) as u8) | 0x80);
        buf.push(((len & 0x7F) as u8) | 0x80);
    } else if len < 268_435_455 {
        buf.push((len >> 21) as u8);
        buf.push((((len >> 14) & 0x7F) as u8) | 0x80);
        buf.push((((len >> 7) & 0x7F) as u8) | 0x80);
        buf.push(((len & 0x7F) as u8) | 0x80);
    } else {
        buf.push((len >> 28) as u8);
        buf.push((((len >> 21) & 0x7F) as u8) | 0x80);
        buf.push((((len >> 14) & 0x7F) as u8) | 0x80);
        buf.push((((len >> 7) & 0x7F) as u8) | 0x80);
        buf.push(((len & 0x7F) as u8) | 0x80);
    }
}

fn parse_listpack_integer(entry: &[u8]) -> Option<i64> {
    if entry.is_empty() || entry.len() >= 21 {
        return None;
    }
    // Accept only the CANONICAL decimal form (exactly what the prior
    // `value.to_string() == entry` round-trip accepted), but WITHOUT allocating a
    // String per element — this runs once per element on every listpack RDB
    // encode, so the per-int allocation showed up as wall-clock cost on
    // integer-heavy collections. Canonical = optional leading '-', no '+', no
    // redundant leading zero, and not "-0".
    if !listpack_int_bytes_are_canonical(entry) {
        return None;
    }
    // Bytes are already verified canonical ASCII `[-]?[0-9]+`, so skip the
    // redundant `from_utf8` validation pass (it showed up at ~19% of listpack RDB
    // encode on integer-heavy collections) and accumulate the i64 directly. Build
    // in the NEGATIVE direction so i64::MIN fits; `checked_*` rejects out-of-range
    // exactly as `str::parse::<i64>()` did (e.g. "9223372036854775808" -> None).
    let (neg, digits): (bool, &[u8]) = match entry.first() {
        Some(b'-') => (true, &entry[1..]),
        _ => (false, entry),
    };
    let mut acc: i64 = 0;
    for &b in digits {
        acc = acc.checked_mul(10)?.checked_sub((b - b'0') as i64)?;
    }
    if neg { Some(acc) } else { acc.checked_neg() }
}

/// True iff `entry` is the canonical base-10 text of some integer — i.e. equal to
/// `n.to_string().as_bytes()` for the `n` it parses to. Allocation-free.
fn listpack_int_bytes_are_canonical(entry: &[u8]) -> bool {
    let digits = match entry.first() {
        Some(b'-') => &entry[1..],
        Some(_) => entry,
        None => return false,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    // Redundant leading zero ("007", "00") — only "0" itself may start with '0'.
    if digits[0] == b'0' && digits.len() > 1 {
        return false;
    }
    // "-0" is non-canonical (to_string yields "0").
    if entry[0] == b'-' && digits == b"0" {
        return false;
    }
    true
}

fn encode_listpack_integer_entry(buf: &mut Vec<u8>, value: i64) {
    let start = buf.len();
    if (0..=127).contains(&value) {
        buf.push(value as u8);
    } else if (-4096..=4095).contains(&value) {
        let encoded = if value < 0 {
            ((1_i64 << 13) + value) as u16
        } else {
            value as u16
        };
        buf.push(((encoded >> 8) as u8) | 0xC0);
        buf.push((encoded & 0xFF) as u8);
    } else if let Ok(value) = i16::try_from(value) {
        buf.push(0xF1);
        buf.extend_from_slice(&value.to_le_bytes());
    } else if (-8_388_608..=8_388_607).contains(&value) {
        let bytes = (value as i32).to_le_bytes();
        buf.push(0xF2);
        buf.extend_from_slice(&bytes[..3]);
    } else if let Ok(value) = i32::try_from(value) {
        buf.push(0xF3);
        buf.extend_from_slice(&value.to_le_bytes());
    } else {
        buf.push(0xF4);
        buf.extend_from_slice(&value.to_le_bytes());
    }
    let data_len = buf.len() - start;
    encode_listpack_backlen(buf, data_len);
}

fn encode_listpack_entry(buf: &mut Vec<u8>, entry: &[u8]) {
    if let Some(value) = parse_listpack_integer(entry) {
        encode_listpack_integer_entry(buf, value);
        return;
    }

    let start = buf.len();
    if entry.len() < 64 {
        buf.push(0x80 | entry.len() as u8);
    } else if entry.len() < 4096 {
        buf.push(0xE0 | ((entry.len() >> 8) as u8 & 0x0F));
        buf.push((entry.len() & 0xFF) as u8);
    } else {
        buf.push(0xF0);
        buf.extend_from_slice(&(entry.len() as u32).to_le_bytes());
    }
    buf.extend_from_slice(entry);
    let data_len = buf.len() - start;
    encode_listpack_backlen(buf, data_len);
}

/// Encode a flat list of byte-strings as an upstream-compatible
/// listpack body (header + entries + 0xFF terminator). Canonical
/// integer-looking strings are emitted with the same compact integer
/// encodings `lpAppend` would choose upstream. Returns `None` when
/// the total wire size doesn't fit in a u32.
/// Listpack frame overhead: 6-byte header (4 total-bytes + 2 element count) +
/// 1-byte 0xFF terminator. Matches `encode_listpack_strings_blob`.
const LISTPACK_BLOB_OVERHEAD: usize = 7;

/// O(1) encoded byte length of one listpack entry — must EXACTLY equal the bytes
/// `encode_listpack_entry` appends for `entry` (header/payload + backlen), so a
/// running sum reproduces `encode_listpack_strings_blob(..).len()` without
/// encoding. Used to pack quicklist nodes incrementally in O(n).
fn listpack_entry_encoded_len(entry: &[u8]) -> usize {
    fn backlen_len(len: usize) -> usize {
        if len <= 127 {
            1
        } else if len < 16_383 {
            2
        } else if len < 2_097_151 {
            3
        } else if len < 268_435_455 {
            4
        } else {
            5
        }
    }
    let data_len = if let Some(value) = parse_listpack_integer(entry) {
        if (0..=127).contains(&value) {
            1
        } else if (-4096..=4095).contains(&value) {
            2
        } else if i16::try_from(value).is_ok() {
            3
        } else if (-8_388_608..=8_388_607).contains(&value) {
            4
        } else if i32::try_from(value).is_ok() {
            5
        } else {
            9
        }
    } else {
        let header = if entry.len() < 64 {
            1
        } else if entry.len() < 4096 {
            2
        } else {
            5
        };
        header + entry.len()
    };
    data_len + backlen_len(data_len)
}

/// Encode an ordered list of byte-strings into a standard listpack blob
/// (`[u32 total_bytes][u16 entry_count][entries...][0xFF]`). Returns `None` only
/// if the total would overflow `u32` (>4 GiB), which never happens for a bounded
/// chunk. Public so the in-memory `ChunkedList` can SEAL a full `Owned` chunk
/// into the compact `Listpack` representation (frankenredis-99fwc).
pub fn encode_listpack_strings_blob(entries: &[&[u8]]) -> Option<Vec<u8>> {
    let mut encoded = listpack_blob_with_header(0);
    for entry in entries {
        encode_listpack_entry(&mut encoded, entry);
    }
    finish_listpack_blob(encoded, entries.len())
}

/// Finish a listpack blob built IN PLACE: `buf` must already start with a 6-byte
/// header placeholder (written by [`listpack_blob_with_header`]) followed by the
/// encoded entries. Appends the `0xFF` terminator and backpatches the
/// `[u32 total_bytes][u16 entry_count]` header. This replaces the prior
/// build-entries-then-allocate-a-second-buffer-and-copy form, removing one
/// allocation and one full-blob memcpy per collection encode (every DUMP /
/// RDB-save listpack node). Output bytes are identical. (frankenredis-cc lpblob1)
fn finish_listpack_blob(mut buf: Vec<u8>, entry_count: usize) -> Option<Vec<u8>> {
    buf.push(0xFF);
    let total_bytes = u32::try_from(buf.len()).ok()?;
    buf[0..4].copy_from_slice(&total_bytes.to_le_bytes());
    let entry_count = u16::try_from(entry_count).unwrap_or(u16::MAX);
    buf[4..6].copy_from_slice(&entry_count.to_le_bytes());
    Some(buf)
}

/// A fresh listpack output buffer with `capacity` reserved and the 6-byte header
/// placeholder already written. Entries are appended after it; [`finish_listpack_blob`]
/// backpatches the header.
#[inline]
fn listpack_blob_with_header(capacity: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(capacity.max(6));
    buf.extend_from_slice(&[0u8; 6]);
    buf
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
pub fn lzf_compress(input: &[u8], out_budget: usize) -> Option<Vec<u8>> {
    LZF_SCRATCH
        .with(|scratch| lzf_compress_with_scratch(input, out_budget, &mut scratch.borrow_mut()))
}

#[derive(Clone, Copy, Default)]
struct LzfHashSlot {
    generation: u32,
    pos_plus_one: u32,
}

struct LzfScratch {
    generation: u32,
    htab: Vec<LzfHashSlot>,
    // (frankenredis-g9h0v) Packed half-size table for inputs < 16 MiB (the
    // overwhelming majority): one u32 per slot = `(gen8 << 24) | pos_plus_one`
    // instead of the 8-byte {generation, pos} pair. Halves the table footprint
    // (512 KiB -> 256 KiB), which fits a typical L2 — the lzf hash probes are
    // cache-cold for low-compressibility data (mostly-literal payloads), where
    // lzf is ~79% of DUMP CPU. Keeps the epoch trick (stale slots read as unset
    // via the generation tag) so there is no per-call memset; the 8-bit
    // generation simply wraps — and clears — every 256 calls instead of 2^32.
    // Inputs >= 16 MiB can't pack pos into 24 bits, so they keep `htab`.
    packed: Vec<u32>,
    packed_generation: u8,
    use_packed: bool,
}

impl LzfScratch {
    const fn new() -> Self {
        Self {
            generation: 0,
            htab: Vec::new(),
            packed: Vec::new(),
            packed_generation: 0,
            use_packed: false,
        }
    }

    /// Returns the active generation tag (u32 for the wide path; the packed path
    /// ignores the return and uses `packed_generation` internally).
    fn begin_call(&mut self, hsize: usize, in_len: usize) -> u32 {
        // pos_plus_one is ip+1 <= in_len, so a 24-bit field holds it iff in_len
        // < 2^24. Above that, fall back to the 8-byte epoch table.
        self.use_packed = in_len < (1 << 24);
        if self.use_packed {
            if self.packed.len() != hsize {
                self.packed.resize(hsize, 0);
                self.packed_generation = 0;
            }
            self.packed_generation = self.packed_generation.wrapping_add(1);
            if self.packed_generation == 0 {
                self.packed.fill(0);
                self.packed_generation = 1;
            }
            return u32::from(self.packed_generation);
        }
        if self.htab.len() != hsize {
            self.htab.resize(hsize, LzfHashSlot::default());
            self.generation = 0;
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.htab.fill(LzfHashSlot::default());
            self.generation = 1;
        }
        self.generation
    }

    #[inline]
    fn get(&self, index: usize, generation: u32) -> u32 {
        if self.use_packed {
            let slot = self.packed[index];
            // High byte is the generation tag; low 24 bits are pos_plus_one.
            if (slot >> 24) == (generation & 0xFF) {
                slot & 0x00FF_FFFF
            } else {
                0
            }
        } else {
            let slot = self.htab[index];
            if slot.generation == generation {
                slot.pos_plus_one
            } else {
                0
            }
        }
    }

    #[inline]
    fn set(&mut self, index: usize, generation: u32, pos_plus_one: u32) {
        if self.use_packed {
            // pos_plus_one < 2^24 is guaranteed by the use_packed gate (in_len
            // < 2^24). Pack the 8-bit generation tag into the high byte.
            self.packed[index] = ((generation & 0xFF) << 24) | (pos_plus_one & 0x00FF_FFFF);
        } else {
            self.htab[index] = LzfHashSlot {
                generation,
                pos_plus_one,
            };
        }
    }
}

/// Length of the common byte prefix of two equal-or-unequal slices (capped at
/// the shorter). Safe SWAR: compare 8 bytes at a time as little-endian u64 and
/// locate the first differing byte via XOR + trailing_zeros, with a byte tail.
/// Byte-IDENTICAL to `a.iter().zip(b).take_while(|(x,y)| x==y).count()` but
/// vectorized (LLVM does not reliably vectorize the take_while early-exit).
/// Used for the lzf match-length tail scan. (frankenredis-g9h0v)
#[inline]
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let d = x ^ y;
        if d != 0 {
            return i + (d.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n {
        if a[i] != b[i] {
            return i;
        }
        i += 1;
    }
    n
}

fn lzf_compress_with_scratch(
    input: &[u8],
    out_budget: usize,
    scratch: &mut LzfScratch,
) -> Option<Vec<u8>> {
    // Faithful port of vendored deps/lzf/lzf_c.c with HLOG=16 and the
    // VERY_FAST configuration redis builds (lzfP.h: VERY_FAST=1,
    // ULTRA_FAST=0). Reproducing the rolling `hval`, the liblzf IDX hash,
    // and the post-match double rehash is required for byte-exact DUMP /
    // RDB output — the earlier Knuth-hash/HLOG=14 version produced
    // valid-but-different compressed streams for mixed-byte inputs (the
    // all-'x' z4tsz test passed only because identical trigrams collide
    // regardless of hash). (frankenredis-wmh2p, supersedes z4tsz)
    const HLOG: u32 = 16;
    const HSIZE: usize = 1 << HLOG; // 65536
    const MAX_LIT: usize = 1 << 5; // 32
    const MAX_OFF: usize = 1 << 13; // 8192
    const MAX_REF: usize = (1 << 8) + (1 << 3); // 264

    let in_len = input.len();
    if in_len == 0 || out_budget == 0 {
        return None;
    }

    // liblzf VERY_FAST: IDX(h) = ((h >> (24 - HLOG)) - h*5) & (HSIZE-1).
    let idx = |h: u32| -> usize {
        (((h >> (24 - HLOG)).wrapping_sub(h.wrapping_mul(5))) & (HSIZE as u32 - 1)) as usize
    };

    let mut out: Vec<u8> = Vec::with_capacity(out_budget);
    // htab stores ip+1 (0 = unset). Epoch tags make stale slots read as unset,
    // preserving the zero-initialised C table semantics without clearing 256 KiB
    // on every compression attempt. (frankenredis-gu5nf.27)
    let generation = scratch.begin_call(HSIZE, in_len);

    let mut ip: usize = 0;
    let mut lit: usize = 0;
    let mut lit_hdr_pos: usize = out.len();
    out.push(0); // start run: placeholder literal-run header
    if out.len() > out_budget {
        return None;
    }

    // `hval` is a rolling 32-bit value: FRST(ip) = (ip[0]<<8)|ip[1] seeds
    // it; each step NEXTs in ip[2]; it is reseeded with FRST after every
    // match. It is NOT a fresh per-position trigram.
    let mut hval: u32 = if in_len >= 2 {
        ((input[0] as u32) << 8) | (input[1] as u32)
    } else {
        0
    };

    while in_len >= 3 && ip < in_len - 2 {
        // hval = NEXT(hval, ip) = (hval << 8) | ip[2]
        hval = (hval << 8) | (input[ip + 2] as u32);
        let h = idx(hval);
        let stored = scratch.get(h, generation);
        scratch.set(h, generation, (ip as u32) + 1);

        let mut emitted_match = false;
        if stored != 0 {
            let r = (stored - 1) as usize;
            // ref > in_data (r >= 1); off = ip-ref-1 < MAX_OFF;
            // ref[0..2] == ip[0..2] (u16 + 3rd byte in C).
            if r >= 1
                && r < ip
                && ip - r - 1 < MAX_OFF
                && input[r + 2] == input[ip + 2]
                && input[r] == input[ip]
                && input[r + 1] == input[ip + 1]
            {
                let off = ip - r - 1;
                // len starts at 2; maxlen = in_end - ip - 2, capped MAX_REF.
                let maxlen = std::cmp::min(MAX_REF, in_len - ip - 2);
                let mut len = 2usize;
                // C match-length loop: an unrolled fast path (maxlen > 16)
                // does 16 UNCHECKED len++ before the bounded do-while, so a
                // run can overshoot maxlen by up to ~16 — this is why
                // e.g. "a"*21 matches 19 bytes, not 17. The unchecked reads
                // are in bounds because maxlen > 16 => ip[18] < in_end.
                'matchloop: {
                    if maxlen > 16 {
                        // SWAR the 16-byte unchecked fast path (compare offsets
                        // 3..=18) as 2x u64 and locate the first differing byte via
                        // XOR + trailing_zeros. Byte-IDENTICAL to the scalar
                        // `len += 1; if ref[len] != ip[len] break` loop — first
                        // mismatch at offset o gives len=o; all 16 equal gives
                        // len=18 (then the bounded tail scan below continues) — but
                        // with one bounds check per 8 bytes instead of two per byte.
                        // In bounds: maxlen>16 => in_len-ip-2>16 => ip+18<in_len, and
                        // r<ip => r+18<in_len, so both 16-byte windows are valid.
                        // (frankenredis-g9h0v: lzf is 79% of list/value DUMP CPU.)
                        let r0 = u64::from_le_bytes(input[r + 3..r + 11].try_into().unwrap());
                        let i0 = u64::from_le_bytes(input[ip + 3..ip + 11].try_into().unwrap());
                        let d0 = r0 ^ i0;
                        if d0 != 0 {
                            len = 3 + (d0.trailing_zeros() / 8) as usize;
                            break 'matchloop;
                        }
                        let r1 = u64::from_le_bytes(input[r + 11..r + 19].try_into().unwrap());
                        let i1 = u64::from_le_bytes(input[ip + 11..ip + 19].try_into().unwrap());
                        let d1 = r1 ^ i1;
                        if d1 != 0 {
                            len = 11 + (d1.trailing_zeros() / 8) as usize;
                            break 'matchloop;
                        }
                        len = 18;
                    }
                    // C: do len++ while (len < maxlen && ref[len] == ip[len]).
                    // Equivalent slice-based common-prefix scan over the aligned
                    // tails, so the per-element bounds checks are hoisted out of the
                    // variable-length match loop (the dominant lzf cost on
                    // compressible RDB/DUMP payloads). The original loop ends at the
                    // first position p in (len, maxlen] with p == maxlen or a
                    // mismatch, i.e. len += 1 + (#consecutive matches at len+1..).
                    // r+maxlen and ip+maxlen are <= in_len (maxlen <= in_len-ip-2,
                    // r < ip), so both subslices are in bounds.
                    if len + 1 < maxlen {
                        let a = &input[r + len + 1..r + maxlen];
                        let b = &input[ip + len + 1..ip + maxlen];
                        len += 1 + common_prefix_len(a, b);
                    } else {
                        len += 1;
                    }
                }
                let enc = len - 2; // len -= 2 (octets - 1)

                // Conservative budget guard, mirroring vendored lzf_c.c:
                //   if (op+3+1 >= out_end) if (op - !lit + 3 + 1 >= out_end) return 0;
                // redis bails to the raw encoding once the output comes within a
                // few bytes of out_end, even when the match would technically
                // still fit — this is what makes the compress-vs-raw DECISION
                // byte-exact (e.g. "hello world hello world" stays raw). The
                // inner test implies the outer, so collapse to the inner form.
                // (frankenredis-wmh2p)
                if out.len() - usize::from(lit == 0) + 4 >= out_budget {
                    return None;
                }

                // Stop the open literal run (op[-lit-1] = lit-1; op -= !lit).
                if lit > 0 {
                    out[lit_hdr_pos] = (lit - 1) as u8;
                } else {
                    out.pop();
                }

                let off_hi = (off >> 8) as u8; // off < 8192 => off>>8 <= 31
                if enc < 7 {
                    out.push(off_hi + ((enc as u8) << 5));
                } else {
                    out.push(off_hi + (7u8 << 5));
                    out.push((enc - 7) as u8);
                }
                out.push((off & 0xFF) as u8);

                // Start a new literal run.
                lit = 0;
                lit_hdr_pos = out.len();
                out.push(0);

                // ip advances by the full match length (C: ip++; ip += enc+1).
                ip += len;
                emitted_match = true;

                if ip >= in_len - 2 {
                    // C: if (ip >= in_end - 2) break; (skip the rehash)
                    break;
                }
                // VERY_FAST post-match rehash: insert htab entries for the
                // last two bytes of the matched region. (--ip; --ip; then
                // two NEXT/store/ip++ steps return ip to ip_m + match.)
                ip -= 2;
                hval = ((input[ip] as u32) << 8) | (input[ip + 1] as u32); // FRST(ip)
                hval = (hval << 8) | (input[ip + 2] as u32); // NEXT
                scratch.set(idx(hval), generation, (ip as u32) + 1);
                ip += 1;
                hval = (hval << 8) | (input[ip + 2] as u32); // NEXT
                scratch.set(idx(hval), generation, (ip as u32) + 1);
                ip += 1;
            }
        }

        if !emitted_match {
            // C: if (op >= out_end) return 0;  (before each literal byte)
            if out.len() >= out_budget {
                return None;
            }
            out.push(input[ip]);
            lit += 1;
            ip += 1;
            if lit == MAX_LIT {
                out[lit_hdr_pos] = (lit - 1) as u8;
                lit = 0;
                lit_hdr_pos = out.len();
                out.push(0);
            }
        }
    }

    // Tail: drain remaining 0..2 bytes as literals.
    // C: if (op + 3 > out_end) return 0;  -- reserve room for the tail
    // literals and the closing run header before flushing. (frankenredis-wmh2p)
    if out.len() + 3 > out_budget {
        return None;
    }
    while ip < in_len {
        out.push(input[ip]);
        lit += 1;
        ip += 1;
        if lit == MAX_LIT {
            out[lit_hdr_pos] = (lit - 1) as u8;
            lit = 0;
            lit_hdr_pos = out.len();
            out.push(0);
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
    // Cap initial allocation to avoid OOM from malicious RDB payloads. The cap
    // bounds the speculative reservation against a hostile header while still
    // pre-sizing legitimate blobs in one allocation — at 8 KiB any compressible
    // value/listpack that decompresses past 8 KiB paid ~log2(len/8K) realloc+copy
    // grows; 1 MiB covers the overwhelming majority of real blobs in a single
    // alloc and keeps the malicious-header reservation bounded. Capacity never
    // affects content, so decoded bytes are identical. (frankenredis-cc lzfcap)
    let mut output = Vec::with_capacity(expected_len.min(1 << 20));
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
        // Replicate the back-reference as chunked memcpys instead of pushing one
        // byte at a time. For overlapping runs (backref < copy_len, e.g. RLE),
        // each `extend_from_within` reads the just-grown tail snapshot, so the
        // available source doubles every iteration and the byte-by-byte
        // propagation is reproduced exactly; for non-overlapping copies it is a
        // single memcpy. Byte-identical to the scalar loop. (frankenredis-5boi9)
        output.reserve(copy_len);
        let mut remaining = copy_len;
        while remaining > 0 {
            let avail = output.len() - copy_start;
            let chunk = remaining.min(avail);
            output.extend_from_within(copy_start..copy_start + chunk);
            remaining -= chunk;
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
                Some((decimal_i64_bytes(i64::from(val)), 2))
            }
            1 => {
                // 16-bit integer
                if data.len() < 3 {
                    return None;
                }
                let val = i16::from_le_bytes([data[1], data[2]]);
                Some((decimal_i64_bytes(i64::from(val)), 3))
            }
            2 => {
                // 32-bit integer
                if data.len() < 5 {
                    return None;
                }
                let val = i32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                Some((decimal_i64_bytes(i64::from(val)), 5))
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

/// Decode redis's legacy ASCII double encoding (`rdbLoadDoubleValue`): a length
/// byte where 253/254/255 mean NaN/+Inf/-Inf, otherwise that many ASCII bytes
/// holding the textual score (e.g. "3.14"). Returns the value and bytes consumed.
fn rdb_load_legacy_double(data: &[u8]) -> Option<(f64, usize)> {
    let len = *data.first()?;
    match len {
        255 => Some((f64::NEG_INFINITY, 1)),
        254 => Some((f64::INFINITY, 1)),
        253 => Some((f64::NAN, 1)),
        n => {
            let end = 1usize.checked_add(n as usize)?;
            let s = std::str::from_utf8(data.get(1..end)?).ok()?;
            let v: f64 = s.trim().parse().ok()?;
            Some((v, end))
        }
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

    // Redis accepts any RDB whose version is in 1..=RDB_VERSION (rdb.c
    // rdbLoadRioWithLoadingCtx rejects only `rdbver < 1 || rdbver > RDB_VERSION`).
    // Match that range so dumps written by older redis releases (6.x = v9,
    // 7.0/7.1 = v10) still load — RDB type tags are version-stable, and any
    // encoding we don't recognise still fails closed at the per-type arm below.
    // A newer version (> RDB_VERSION) is rejected: its format is unknown to us.
    let version_str = std::str::from_utf8(&data[5..9]).map_err(|_| PersistError::InvalidFrame)?;
    let version: u32 = version_str
        .parse()
        .map_err(|_| PersistError::InvalidFrame)?;
    if !(1..=RDB_VERSION).contains(&version) {
        return Err(PersistError::InvalidFrame);
    }
    let mut cursor = 9; // Skip "REDIS" + 4-digit version
    let mut entries = Vec::new();
    let mut aux = BTreeMap::new();
    let mut functions: Vec<Vec<u8>> = Vec::new();
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
                | RDB_TYPE_ZSET
                | RDB_TYPE_ZSET_2
                | RDB_TYPE_STREAM
                | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2
                | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3
                | RDB_TYPE_SET_INTSET
                | RDB_TYPE_HASH_ZIPMAP
                | RDB_TYPE_LIST_ZIPLIST
                | RDB_TYPE_ZSET_ZIPLIST
                | RDB_TYPE_HASH_ZIPLIST
                | RDB_TYPE_LIST_QUICKLIST
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
                if pending_expire_ms.is_some() {
                    return Err(PersistError::InvalidFrame);
                }
                // RDB versions < 5 predate the CRC64 trailer entirely — EOF is
                // the final byte. Versions >= 5 always append 8 bytes, but a
                // stored checksum of 0 means "checksum disabled" and redis skips
                // verification (rdb.c: `if (server.rdb_checksum && cksum) ...`).
                if version >= 5 {
                    if cursor + RDB_CHECKSUM_LEN > data.len() {
                        return Err(PersistError::InvalidFrame);
                    }
                    let expected_checksum = u64::from_le_bytes(
                        data[cursor..cursor + RDB_CHECKSUM_LEN]
                            .try_into()
                            .map_err(|_| PersistError::InvalidFrame)?,
                    );
                    let actual_checksum = crc64_redis(&data[..cursor]);
                    if expected_checksum != 0 && expected_checksum != actual_checksum {
                        return Err(PersistError::InvalidFrame);
                    }
                    cursor += RDB_CHECKSUM_LEN;
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
            RDB_OPCODE_FUNCTION2 => {
                // Function-library payload: a single raw string (the library
                // source). Capture it and continue so the keyspace that
                // follows still loads. Re-registering the library into the
                // function engine is the runtime's responsibility; dropping
                // the whole RDB here was a data-loss bug.
                let (code, consumed) =
                    rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                cursor += consumed;
                functions.push(code);
            }
            type_byte @ (RDB_TYPE_STRING
            | RDB_TYPE_LIST
            | RDB_TYPE_SET
            | RDB_TYPE_HASH
            | RDB_TYPE_HASH_WITH_TTLS
            | RDB_TYPE_ZSET
            | RDB_TYPE_ZSET_2
            | RDB_TYPE_STREAM
            | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2
            | UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3
            | RDB_TYPE_SET_INTSET
            | RDB_TYPE_HASH_ZIPMAP
            | RDB_TYPE_LIST_ZIPLIST
            | RDB_TYPE_ZSET_ZIPLIST
            | RDB_TYPE_HASH_ZIPLIST
            | RDB_TYPE_LIST_QUICKLIST
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
                        let mut items = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                        let mut members = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
                        for _ in 0..count {
                            let (m, c) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            members.push(m);
                        }
                        // (frankenredis-39is8) Plain RDB_TYPE_SET is the hashtable
                        // encoding; preserve it so the load doesn't re-derive a
                        // smaller encoding from content. INTSET/LISTPACK arms keep
                        // the plain `Set` (re-derive intset/listpack).
                        RdbValue::SetHashtable(members)
                    }
                    RDB_TYPE_HASH => {
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut fields = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                        let mut fields = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                        let mut members = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                    RDB_TYPE_ZSET => {
                        // Legacy zset (redis ≤ 6.2): count, then (member:string,
                        // score:legacy-ASCII-double) pairs.
                        let (count, c) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += c;
                        let mut members = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
                        for _ in 0..count {
                            let (m, c) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            let (score, c) = rdb_load_legacy_double(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += c;
                            members.push((m, score));
                        }
                        RdbValue::SortedSet(members)
                    }
                    RDB_TYPE_STREAM => {
                        // Decode watermark, private entries-added counter, and the
                        // max-deleted-entry-id watermark (frankenredis-fplrm).
                        if cursor + 40 > data.len() {
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
                        let entries_added = u64::from_le_bytes(
                            data[cursor..cursor + 8]
                                .try_into()
                                .map_err(|_| PersistError::InvalidFrame)?,
                        );
                        cursor += 8;
                        let md_ms = u64::from_le_bytes(
                            data[cursor..cursor + 8]
                                .try_into()
                                .map_err(|_| PersistError::InvalidFrame)?,
                        );
                        cursor += 8;
                        let md_seq = u64::from_le_bytes(
                            data[cursor..cursor + 8]
                                .try_into()
                                .map_err(|_| PersistError::InvalidFrame)?,
                        );
                        cursor += 8;
                        let max_deleted = if md_ms == 0 && md_seq == 0 {
                            None
                        } else {
                            Some((md_ms, md_seq))
                        };
                        let (count, consumed) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let mut stream_entries = Vec::with_capacity(count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                            let mut fields = Vec::with_capacity(field_count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                            if cursor + 24 > data.len() {
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
                            let entries_read = u64::from_le_bytes(
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
                                if cursor + 16 > data.len() {
                                    return Err(PersistError::InvalidFrame);
                                }
                                let seen_time_ms = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                let active_raw = u64::from_le_bytes(
                                    data[cursor..cursor + 8]
                                        .try_into()
                                        .map_err(|_| PersistError::InvalidFrame)?,
                                );
                                cursor += 8;
                                consumers.push(RdbStreamConsumer {
                                    name: cname,
                                    seen_time_ms,
                                    active_time_ms: if active_raw as i64 == -1 {
                                        None
                                    } else {
                                        Some(active_raw)
                                    },
                                });
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
                                entries_read: if entries_read == u64::MAX {
                                    None
                                } else {
                                    Some(entries_read)
                                },
                                consumers,
                                pending,
                            });
                        }
                        RdbValue::Stream(
                            stream_entries,
                            watermark,
                            groups,
                            None,
                            Some(entries_added),
                            max_deleted,
                        )
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
                            .map(listpack::ListpackEntry::into_bytes)
                            .collect();
                        RdbValue::Set(members)
                    }
                    RDB_TYPE_HASH_LISTPACK => {
                        // Listpack of f1, v1, f2, v2, ... pairs.
                        let (listpack, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        // Pair owned decoded entries straight into `fields`.
                        // Moving string payloads avoids a clone+drop allocation;
                        // integer entries still render to canonical decimal bytes.
                        let decoded = listpack::decode_listpack(&listpack)
                            .map_err(|_| PersistError::InvalidFrame)?;
                        if !decoded.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let mut fields = Vec::with_capacity(decoded.len() / 2);
                        let mut it = decoded.into_iter();
                        while let Some(field) = it.next() {
                            let value = it.next().ok_or(PersistError::InvalidFrame)?;
                            fields.push((field.into_bytes(), value.into_bytes()));
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
                        // Same owned-entry move as the hash listpack path above:
                        // string members/scores move out, integer scores render.
                        let decoded = listpack::decode_listpack(&listpack)
                            .map_err(|_| PersistError::InvalidFrame)?;
                        if !decoded.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let mut members = Vec::with_capacity(decoded.len() / 2);
                        let mut it = decoded.into_iter();
                        while let Some(member) = it.next() {
                            // (CrimsonHawk) Integer-valued scores round-trip through
                            // the listpack as INT entries (the encoder int-encodes the
                            // decimal). Reading the i64 straight to f64 skips the
                            // render→from_utf8→parse::<f64> round-trip (a decimal alloc
                            // + a float parse) for those — measured -24.7% on the zset
                            // listpack decode (isolated A/B). Byte-identical: `n as f64`
                            // and `parse(decimal(n))` both yield the nearest f64 to n.
                            // Non-integer scores stay String entries (1.5, inf, ...) and
                            // take the textual parse path unchanged.
                            let score = match it.next().ok_or(PersistError::InvalidFrame)? {
                                listpack::ListpackEntry::Integer(n) => n as f64,
                                listpack::ListpackEntry::String(bytes) => {
                                    std::str::from_utf8(&bytes)
                                        .ok()
                                        .and_then(|s| s.parse::<f64>().ok())
                                        .ok_or(PersistError::InvalidFrame)?
                                }
                            };
                            members.push((member.into_bytes(), score));
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
                        let mut items = Vec::with_capacity(node_count.min(RDB_COLLECTION_PRESIZE_CAP));
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
                                    // PACKED: the blob is a listpack. Move each
                                    // decoded entry's payload out with `into_bytes`
                                    // (the iterator yields owned entries) rather than
                                    // `to_bytes`, which cloned the string payload and
                                    // dropped the original — one wasted alloc+copy+free
                                    // per packed list element on the quicklist2 decode
                                    // (RESTORE / DEBUG RELOAD) path. Byte-identical.
                                    for entry in listpack::decode_listpack(&node_blob)
                                        .map_err(|_| PersistError::InvalidFrame)?
                                    {
                                        items.push(entry.into_bytes());
                                    }
                                }
                                _ => return Err(PersistError::InvalidFrame),
                            }
                        }
                        RdbValue::List(items)
                    }
                    RDB_TYPE_LIST_ZIPLIST => {
                        // Legacy single-ziplist list (redis ≤ 6.2).
                        let (zl, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let items =
                            ziplist::decode_ziplist(&zl).ok_or(PersistError::InvalidFrame)?;
                        RdbValue::List(items)
                    }
                    RDB_TYPE_LIST_QUICKLIST => {
                        // Legacy quicklist: node_count plain ziplist nodes (no
                        // per-node container byte, unlike QUICKLIST_2).
                        let (node_count, consumed) =
                            rdb_decode_length(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let mut items = Vec::with_capacity(node_count.min(RDB_COLLECTION_PRESIZE_CAP));
                        for _ in 0..node_count {
                            let (node_blob, consumed) = rdb_decode_string(&data[cursor..])
                                .ok_or(PersistError::InvalidFrame)?;
                            cursor += consumed;
                            items.extend(
                                ziplist::decode_ziplist(&node_blob)
                                    .ok_or(PersistError::InvalidFrame)?,
                            );
                        }
                        RdbValue::List(items)
                    }
                    RDB_TYPE_HASH_ZIPLIST => {
                        // Ziplist of f1, v1, f2, v2, ... pairs.
                        let (zl, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let entries =
                            ziplist::decode_ziplist(&zl).ok_or(PersistError::InvalidFrame)?;
                        if !entries.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let fields = entries
                            .chunks_exact(2)
                            .map(|pair| (pair[0].clone(), pair[1].clone()))
                            .collect();
                        RdbValue::Hash(fields)
                    }
                    RDB_TYPE_HASH_ZIPMAP => {
                        // Even-older zipmap small-hash encoding (redis ≤ 2.4).
                        let (zm, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let entries =
                            ziplist::decode_zipmap(&zm).ok_or(PersistError::InvalidFrame)?;
                        if !entries.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let fields = entries
                            .chunks_exact(2)
                            .map(|pair| (pair[0].clone(), pair[1].clone()))
                            .collect();
                        RdbValue::Hash(fields)
                    }
                    RDB_TYPE_ZSET_ZIPLIST => {
                        // Ziplist of m1, score1, m2, score2, ... with the score
                        // as its decimal-string form.
                        let (zl, consumed) =
                            rdb_decode_string(&data[cursor..]).ok_or(PersistError::InvalidFrame)?;
                        cursor += consumed;
                        let entries =
                            ziplist::decode_ziplist(&zl).ok_or(PersistError::InvalidFrame)?;
                        if !entries.len().is_multiple_of(2) {
                            return Err(PersistError::InvalidFrame);
                        }
                        let mut members = Vec::with_capacity(entries.len() / 2);
                        for pair in entries.chunks_exact(2) {
                            let score = std::str::from_utf8(&pair[1])
                                .ok()
                                .and_then(|s| s.parse::<f64>().ok())
                                .ok_or(PersistError::InvalidFrame)?;
                            members.push((pair[0].clone(), score));
                        }
                        RdbValue::SortedSet(members)
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
        functions,
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
    write_rdb_bytes_atomically(path, &encode_rdb(entries, aux))
}

/// Like [`write_rdb_file`] but also persists FUNCTION library payloads as
/// `RDB_OPCODE_FUNCTION2` records, so libraries survive a save/load cycle
/// (restart, DEBUG RELOAD, replication snapshot). (frankenredis-c0u9q)
pub fn write_rdb_file_with_functions(
    path: &Path,
    entries: &[RdbEntry],
    aux: &[(&str, &str)],
    functions: &[&[u8]],
) -> Result<(), PersistError> {
    write_rdb_bytes_atomically(path, &encode_rdb_with_functions(entries, aux, functions))
}

/// Durably write already-encoded RDB `bytes` to `path` (temp file + rename +
/// parent fsync). Lets callers that pre-encode the snapshot — e.g. via the
/// borrowed string-only fast path — reuse the atomic-write machinery without
/// going back through an owned `RdbEntry` list.
pub fn write_rdb_bytes(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    write_rdb_bytes_atomically(path, bytes)
}

/// Durably write RDB bytes to `path` via a temp file + rename + parent fsync.
fn write_rdb_bytes_atomically(path: &Path, encoded: &[u8]) -> Result<(), PersistError> {
    let tmp_path = path.with_extension("rdb.tmp");
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(encoded)?;
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

/// Like [`read_rdb_file`] but also returns the `FUNCTION2` library payloads
/// captured from the dump, so the caller can re-register them into the function
/// engine. A missing file yields empty collections. (frankenredis-tm139)
#[allow(clippy::type_complexity)]
pub fn read_rdb_file_with_functions(
    path: &Path,
) -> Result<(Vec<RdbEntry>, BTreeMap<String, String>, Vec<Vec<u8>>), PersistError> {
    match std::fs::read(path) {
        Ok(data) => {
            if data.is_empty() {
                return Err(PersistError::InvalidFrame);
            }
            let decoded = decode_rdb_prefix(&data)?;
            if decoded.consumed != data.len() {
                return Err(PersistError::InvalidFrame);
            }
            Ok((decoded.entries, decoded.aux, decoded.functions))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((Vec::new(), BTreeMap::new(), Vec::new()))
        }
        Err(e) => Err(PersistError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use fr_protocol::{RespFrame, RespParseError};

    #[test]
    fn parse_listpack_integer_matches_to_string_roundtrip() {
        // The allocation-free canonical check must accept/reject EXACTLY the set
        // the old `value.to_string() == entry` round-trip did.
        fn oracle(entry: &[u8]) -> Option<i64> {
            if entry.is_empty() || entry.len() >= 21 {
                return None;
            }
            let value = std::str::from_utf8(entry).ok()?.parse::<i64>().ok()?;
            if value.to_string().as_bytes() == entry {
                Some(value)
            } else {
                None
            }
        }
        let mut cases: Vec<Vec<u8>> = vec![
            b"0".to_vec(),
            b"-0".to_vec(),
            b"00".to_vec(),
            b"007".to_vec(),
            b"+7".to_vec(),
            b"7".to_vec(),
            b"-7".to_vec(),
            b"123456789".to_vec(),
            b"-123456789".to_vec(),
            b"9223372036854775807".to_vec(),  // i64::MAX
            b"-9223372036854775808".to_vec(), // i64::MIN
            b"9223372036854775808".to_vec(),  // MAX+1 -> overflow
            b"99999999999999999999".to_vec(), // 20 digits, overflow
            b" 7".to_vec(),
            b"7 ".to_vec(),
            b"-".to_vec(),
            b"".to_vec(),
            b"1.0".to_vec(),
            b"0x10".to_vec(),
            b"1e3".to_vec(),
            b"\xff\x01".to_vec(),
        ];
        for n in [0i64, 1, -1, 42, -42, i64::MAX, i64::MIN, 1000000007] {
            cases.push(n.to_string().into_bytes());
        }
        for entry in &cases {
            assert_eq!(
                super::parse_listpack_integer(entry),
                oracle(entry),
                "mismatch for {entry:?}"
            );
        }
    }

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
    fn aof_manifest_skips_blank_and_comment_lines() {
        let manifest = parse_aof_manifest(
            "# generated by Redis\n\
             \n\
             file appendonly.aof.1.base.rdb seq 1 type b\n\
             \n\
             # history comes before active incrementals\n\
             file appendonly.aof.2.incr.aof seq 2 type h\n\
             file appendonly.aof.3.incr.aof seq 3 type i\n",
        )
        .expect("parse manifest with comments and blanks");

        assert_eq!(
            manifest.base.as_ref().map(|entry| entry.file_name.as_str()),
            Some("appendonly.aof.1.base.rdb")
        );
        assert_eq!(manifest.history.len(), 1);
        assert_eq!(manifest.incremental.len(), 1);
        assert_eq!(manifest.curr_incr_file_seq, 3);
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

    #[test]
    fn read_aof_manifest_dir_loads_base_rdb_and_incr_records() {
        // (frankenredis-nvcby) A redis 7 multi-part AOF (manifest + base.rdb +
        // incr.aof) must load the base RDB snapshot plus the ordered incr
        // command records.
        let dir = std::env::temp_dir().join("fr_persist_multipart_aof_test");
        let _ = std::fs::create_dir_all(&dir);

        // Base RDB carries one key + a FUNCTION library.
        let base_entries = vec![RdbEntry {
            db: 0,
            key: b"base_key".to_vec(),
            value: RdbValue::String(b"base_val".to_vec()),
            expire_ms: None,
        }];
        let lib = b"#!lua name=ml\nredis.register_function('f', function() return 1 end)";
        let base_rdb = encode_rdb_with_functions(&base_entries, &[], &[lib.as_slice()]);
        std::fs::write(dir.join("appendonly.aof.1.base.rdb"), &base_rdb).expect("write base");

        // Incremental AOF: a SET applied on top of the base.
        let incr = encode_aof_stream(&[AofRecord {
            argv: vec![b"SET".to_vec(), b"incr_key".to_vec(), b"incr_val".to_vec()],
        }]);
        std::fs::write(dir.join("appendonly.aof.1.incr.aof"), &incr).expect("write incr");

        let manifest_path = dir.join("appendonly.aof.manifest");
        std::fs::write(
            &manifest_path,
            "file appendonly.aof.1.base.rdb seq 1 type b\n\
             file appendonly.aof.1.incr.aof seq 1 type i\n",
        )
        .expect("write manifest");

        let loaded = super::read_aof_manifest_dir(&manifest_path).expect("load multipart aof");
        assert_eq!(loaded.base_rdb_entries, base_entries);
        assert_eq!(loaded.base_rdb_functions, vec![lib.to_vec()]);
        assert_eq!(
            loaded.records,
            vec![AofRecord {
                argv: vec![b"SET".to_vec(), b"incr_key".to_vec(), b"incr_val".to_vec()],
            }]
        );

        let _ = std::fs::remove_file(dir.join("appendonly.aof.1.base.rdb"));
        let _ = std::fs::remove_file(dir.join("appendonly.aof.1.incr.aof"));
        let _ = std::fs::remove_file(&manifest_path);
    }

    #[test]
    fn write_aof_manifest_dir_round_trips_through_read() {
        // (frankenredis-aofw1) The appendonlydir written by write_aof_manifest_dir
        // must load back through read_aof_manifest_dir: base RDB entries +
        // FUNCTION libs + incr records, intact and in order.
        let dir = std::env::temp_dir().join("fr_persist_aof_write_roundtrip_test");
        let _ = std::fs::remove_dir_all(&dir);

        let base_entries = vec![RdbEntry {
            db: 0,
            key: b"base_key".to_vec(),
            value: RdbValue::String(b"base_val".to_vec()),
            expire_ms: None,
        }];
        let lib = b"#!lua name=ml\nredis.register_function('f', function() return 1 end)";
        let base_rdb = encode_rdb_with_functions(&base_entries, &[], &[lib.as_slice()]);
        let incr = vec![
            AofRecord {
                argv: vec![b"SET".to_vec(), b"incr_key".to_vec(), b"v".to_vec()],
            },
            AofRecord {
                argv: vec![b"DEL".to_vec(), b"base_key".to_vec()],
            },
        ];

        super::write_aof_manifest_dir(&dir, "appendonly.aof", 1, &base_rdb, &incr)
            .expect("write appendonlydir");

        // Manifest references both data files, which exist on disk.
        let manifest_path = dir.join("appendonly.aof.manifest");
        assert!(manifest_path.exists());
        assert!(dir.join("appendonly.aof.1.base.rdb").exists());
        assert!(dir.join("appendonly.aof.1.incr.aof").exists());

        let loaded = super::read_aof_manifest_dir(&manifest_path).expect("read back");
        assert_eq!(loaded.base_rdb_entries, base_entries);
        assert_eq!(loaded.base_rdb_functions, vec![lib.to_vec()]);
        assert_eq!(loaded.records, incr);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_manifest_fuzz_corpus_seeds_parse_or_reject_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let corpus_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/fuzz_aof_manifest_parser");
        assert!(
            corpus_root.exists(),
            "fuzz corpus dir {} not present",
            corpus_root.display()
        );

        let mut seed_paths = std::fs::read_dir(&corpus_root)
            .map_err(|err| format!("read corpus dir {}: {err}", corpus_root.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        seed_paths.sort();
        seed_paths.retain(|path| path.is_file());
        assert!(
            seed_paths.len() >= 10,
            "expected at least 10 AOF manifest parser fuzz seeds, found {}",
            seed_paths.len()
        );

        for path in seed_paths {
            let bytes = std::fs::read(&path)
                .map_err(|err| format!("read seed {}: {err}", path.display()))?;
            if bytes.len() > 1_000_000 {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };

            if let Ok(manifest) = parse_aof_manifest(text) {
                let formatted = format_aof_manifest(&manifest);
                assert_eq!(
                    parse_aof_manifest(&formatted).map_err(|err| format!(
                        "reparse formatted seed {}: {err:?}",
                        path.display()
                    ))?,
                    manifest,
                    "formatted manifest did not round-trip for {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    // ── RDB tests ────────────────────────────────────────────────────

    use super::{
        CompactRdbThresholds, RDB_CHECKSUM_LEN, RDB_OPCODE_AUX, RDB_OPCODE_EOF,
        RDB_OPCODE_EXPIRETIME_MS, RDB_OPCODE_FUNCTION2, RDB_OPCODE_RESIZEDB, RDB_OPCODE_SELECTDB,
        RDB_TYPE_HASH, RDB_TYPE_HASH_LISTPACK, RDB_TYPE_HASH_WITH_TTLS, RDB_TYPE_LIST,
        RDB_TYPE_LIST_QUICKLIST_2, RDB_TYPE_SET, RDB_TYPE_SET_INTSET, RDB_TYPE_SET_LISTPACK,
        RDB_TYPE_STRING, RDB_TYPE_ZSET_2, RDB_TYPE_ZSET_LISTPACK, RdbEncodeOptions, RdbEntry,
        RdbStreamConsumer, RdbStreamConsumerGroup, RdbStreamMetadata, RdbStreamPendingEntry,
        RdbValue, UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, crc64_redis, decode_intset_members,
        decode_rdb, decode_rdb_prefix, encode_compact_set_intset, encode_hash_listpack_blob,
        encode_listpack_strings_blob, encode_rdb, encode_rdb_with_functions,
        encode_rdb_with_options, encode_set_listpack_blob, lzf_compress, lzf_decompress,
        rdb_decode_string, rdb_encode_length, rdb_encode_string,
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

    #[test]
    fn borrowed_string_entries_encode_like_generic_rdb_entries() {
        let entries = vec![
            RdbEntry {
                db: 2,
                key: b"z".to_vec(),
                value: RdbValue::String(b"last".to_vec()),
                expire_ms: Some(55),
            },
            RdbEntry {
                db: 0,
                key: b"a".to_vec(),
                value: RdbValue::String(b"first".to_vec()),
                expire_ms: None,
            },
            RdbEntry {
                db: 2,
                key: b"m".to_vec(),
                value: RdbValue::String(b"middle".to_vec()),
                expire_ms: None,
            },
        ];
        let mut refs: Vec<crate::RdbStringEntryRef<'_>> = entries
            .iter()
            .map(|entry| {
                let RdbValue::String(value) = &entry.value else {
                    unreachable!("test entries are string-only");
                };
                crate::RdbStringEntryRef {
                    db: entry.db,
                    key: &entry.key,
                    value,
                    expire_ms: entry.expire_ms,
                }
            })
            .collect();
        refs.sort_by(|left, right| left.db.cmp(&right.db).then_with(|| left.key.cmp(right.key)));

        let aux = [("redis-ver", "7.2.4"), ("frankenredis", "true")];
        let lib = b"#!lua name=borrowed\nredis.register_function('bf', function() return 1 end)";
        let functions = [lib.as_slice()];

        assert_eq!(
            crate::encode_rdb_string_entries_with_functions(&refs, &aux, &functions),
            encode_rdb_with_functions(&entries, &aux, &functions)
        );
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
    fn lzf_decompress_never_panics_on_adversarial_input() {
        // lzf_decompress parses UNTRUSTED bytes (RESTORE payloads, RDB string
        // fields). A crafted corrupt stream — truncated control/backref bytes,
        // a back-reference pointing before the output start, an over-long copy,
        // or a literal run that overruns the input — must be REJECTED (None),
        // never panic (a panic here is a remote DoS). Regression guard for the
        // bounds discipline (all reads via get()?/checked_add + the
        // backref > output.len() check) and the 512MB OOM cap. (frankenredis-lzfadv)
        let cases: &[&[u8]] = &[
            &[],                                               // empty
            &[0x04, b'h', b'i'], // literal run wants 5 bytes, only 2 present
            &[0x1F],             // literal ctrl, no payload
            &[0x20, 0x00],       // backref=1 into empty output
            &[0xE0, 0x00],       // copy_len=9 marker, truncated backref
            &[0xE0, 0xFF],       // copy_len extended, no backref byte
            &[0xFF, 0xFF, 0xFF], // max ctrl, backref far past output start
            &[0x04, b'a', b'b', b'c', b'd', b'e', 0x20, 0x00], // literal then bad backref
        ];
        for c in cases {
            for &elen in &[0usize, 1, 5, 64, 1_000_000, 600_000_000, usize::MAX] {
                // Contract: returns Some/None, NEVER panics, NEVER over-allocates.
                let _ = lzf_decompress(c, elen);
            }
        }
        // Oversized expected_len is rejected outright (OOM guard).
        assert!(lzf_decompress(&[0x00, b'x'], 600_000_000).is_none());
        // Back-reference before output start → None, not an arithmetic underflow.
        assert!(lzf_decompress(&[0x20, 0x00], 4).is_none());
    }

    #[test]
    fn lzf_decompress_chunked_matches_bytewise_and_reports_ab_ratio() {
        // Byte-at-a-time reference (the pre-chunk back-reference path), used as
        // the equivalence oracle and A/B baseline. (frankenredis-5boi9)
        fn ref_decompress(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
            let mut output = Vec::with_capacity(expected_len.min(8192));
            let mut cursor = 0usize;
            while cursor < input.len() && output.len() < expected_len {
                let ctrl = usize::from(*input.get(cursor)?);
                cursor += 1;
                if ctrl < 32 {
                    let literal_len = ctrl + 1;
                    let end = cursor.checked_add(literal_len)?;
                    output.extend_from_slice(input.get(cursor..end)?);
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

        // Backref-heavy + overlapping-run payloads: long repeats (RLE-like) and
        // periodic patterns compress to many back-references.
        let payloads: Vec<Vec<u8>> = vec![
            vec![b'a'; 4096],
            b"abcabcabcabc".iter().copied().cycle().take(8192).collect(),
            b"the quick brown fox "
                .iter()
                .copied()
                .cycle()
                .take(16384)
                .collect(),
            {
                let mut v = Vec::new();
                for i in 0..4000u32 {
                    v.extend_from_slice(&(i % 7).to_le_bytes());
                }
                v
            },
        ];
        let mut compressed_set = Vec::new();
        for p in &payloads {
            let c = lzf_compress(p, p.len().saturating_sub(1)).expect("payload should compress");
            // equivalence: chunked == bytewise == original
            let chunked = lzf_decompress(&c, p.len()).expect("chunked decode");
            let bytewise = ref_decompress(&c, p.len()).expect("bytewise decode");
            assert_eq!(chunked, *p, "chunked round-trip");
            assert_eq!(chunked, bytewise, "chunked != bytewise");
            compressed_set.push((c, p.len()));
        }

        // A/B over the backref-heavy compressed set.
        let reps = 20000;
        let t0 = std::time::Instant::now();
        let mut acc = 0usize;
        for _ in 0..reps {
            for (c, len) in &compressed_set {
                acc =
                    acc.wrapping_add(ref_decompress(std::hint::black_box(c), *len).unwrap().len());
            }
        }
        let bytewise_ns = t0.elapsed().as_nanos().max(1);
        std::hint::black_box(acc);
        let t1 = std::time::Instant::now();
        let mut acc2 = 0usize;
        for _ in 0..reps {
            for (c, len) in &compressed_set {
                acc2 =
                    acc2.wrapping_add(lzf_decompress(std::hint::black_box(c), *len).unwrap().len());
            }
        }
        let chunked_ns = t1.elapsed().as_nanos().max(1);
        std::hint::black_box(acc2);
        let ratio = bytewise_ns as f64 / chunked_ns as f64;
        println!(
            "LZF decompress A/B (4 backref-heavy payloads x{reps}): bytewise={bytewise_ns}ns chunked={chunked_ns}ns ratio={ratio:.2}x"
        );
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
    }

    #[test]
    fn large_list_rdb_uses_multinode_quicklist2_not_legacy() {
        use super::{
            CompactRdbThresholds, encode_compact_list_quicklist2, encode_listpack_strings_blob,
            rdb_decode_length, rdb_encode_length, rdb_encode_string,
        };
        let th = CompactRdbThresholds::default();
        // > one 8 KiB listpack node, no single oversized element.
        let items: Vec<Vec<u8>> = (0..10000).map(|i| format!("e{i}").into_bytes()).collect();
        let payload = encode_compact_list_quicklist2(&items, &th)
            .expect("large all-small-item list must encode as QUICKLIST_2 (not fall back to None)");
        let (node_count, _) = rdb_decode_length(&payload).expect("node count");
        assert!(
            node_count > 1,
            "expected multiple PACKED nodes, got {node_count}"
        );
        // Small list → exactly one node.
        let small =
            encode_compact_list_quicklist2(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], &th)
                .expect("small list encodes");
        assert_eq!(rdb_decode_length(&small).unwrap().0, 1);

        let mixed_thresholds = CompactRdbThresholds {
            list_max_listpack_size: 13,
            ..CompactRdbThresholds::default()
        };
        let plain = b"plain-payload!".to_vec();
        let mixed = encode_compact_list_quicklist2(
            &[b"a".to_vec(), b"b".to_vec(), plain.clone(), b"c".to_vec()],
            &mixed_thresholds,
        )
        .expect("mixed quicklist2 encodes");
        let first_reference =
            encode_listpack_strings_blob(&[b"a".as_slice(), b"b".as_slice()]).unwrap();
        let middle_reference = encode_listpack_strings_blob(&[plain.as_slice()]).unwrap();
        let last_reference = encode_listpack_strings_blob(&[b"c".as_slice()]).unwrap();
        let mut expected = Vec::new();
        rdb_encode_length(&mut expected, 3);
        rdb_encode_length(&mut expected, 2);
        rdb_encode_string(&mut expected, &first_reference);
        rdb_encode_length(&mut expected, 2);
        rdb_encode_string(&mut expected, &middle_reference);
        rdb_encode_length(&mut expected, 2);
        rdb_encode_string(&mut expected, &last_reference);
        assert_eq!(mixed, expected);

        let (node_count, mut cursor) = rdb_decode_length(&mixed).expect("mixed node count");
        assert_eq!(node_count, 3);
        let (first_container, consumed) =
            rdb_decode_length(&mixed[cursor..]).expect("first container");
        cursor += consumed;
        assert_eq!(first_container, 2);
        let (first_node, consumed) = rdb_decode_string(&mixed[cursor..]).expect("first node");
        cursor += consumed;
        assert_eq!(
            crate::listpack::decode_listpack(&first_node).expect("first packed node"),
            vec![
                crate::listpack::ListpackEntry::String(b"a".to_vec()),
                crate::listpack::ListpackEntry::String(b"b".to_vec()),
            ]
        );
        let (plain_container, consumed) =
            rdb_decode_length(&mixed[cursor..]).expect("middle container");
        cursor += consumed;
        assert_eq!(plain_container, 2);
        let (plain_node, consumed) = rdb_decode_string(&mixed[cursor..]).expect("middle node");
        cursor += consumed;
        assert_eq!(
            crate::listpack::decode_listpack(&plain_node).expect("middle packed node"),
            vec![crate::listpack::ListpackEntry::String(plain)]
        );
        let (last_container, consumed) =
            rdb_decode_length(&mixed[cursor..]).expect("last container");
        cursor += consumed;
        assert_eq!(last_container, 2);
        let (last_node, consumed) = rdb_decode_string(&mixed[cursor..]).expect("last node");
        cursor += consumed;
        assert_eq!(
            crate::listpack::decode_listpack(&last_node).expect("last packed node"),
            vec![crate::listpack::ListpackEntry::String(b"c".to_vec())]
        );
        assert_eq!(cursor, mixed.len());

        // Full RDB round-trip through the canonical QUICKLIST_2 path is byte-faithful.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"big".to_vec(),
            value: RdbValue::List(items),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
    fn rdb_function2_opcode_is_captured_and_keys_still_load() {
        // Regression: a dump containing a FUNCTION library (RDB_OPCODE_FUNCTION2,
        // emitted by redis 7.0+ at the head of the file) used to fall through to
        // the fail-closed arm and discard the ENTIRE keyspace. The library
        // payload must be captured and the keys that follow must still decode.
        let lib_a = b"#!lua name=liba\nredis.register_function('fa', function() return 1 end)";
        let lib_b = b"#!lua name=libb\nredis.register_function('fb', function() return 2 end)";

        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"REDIS0011");
        // Functions are written before any SELECTDB / keys, one opcode each.
        encoded.push(RDB_OPCODE_FUNCTION2);
        rdb_encode_string(&mut encoded, lib_a);
        encoded.push(RDB_OPCODE_FUNCTION2);
        rdb_encode_string(&mut encoded, lib_b);
        // A normal key after the function payloads.
        encoded.push(RDB_OPCODE_SELECTDB);
        rdb_encode_length(&mut encoded, 0);
        encoded.push(RDB_TYPE_STRING);
        rdb_encode_string(&mut encoded, b"survivor");
        rdb_encode_string(&mut encoded, b"value");
        encoded.push(RDB_OPCODE_EOF);
        append_rdb_checksum(&mut encoded);

        let decoded = decode_rdb_prefix(&encoded).expect("FUNCTION2 dump must load, not abort");
        assert_eq!(decoded.consumed, encoded.len());
        assert_eq!(
            decoded.functions,
            vec![lib_a.to_vec(), lib_b.to_vec()],
            "both library payloads must be captured in file order"
        );
        assert_eq!(
            decoded.entries,
            vec![RdbEntry {
                db: 0,
                key: b"survivor".to_vec(),
                value: RdbValue::String(b"value".to_vec()),
                expire_ms: None,
            }],
            "keys following the function payloads must survive the load"
        );
    }

    #[test]
    fn encode_rdb_with_functions_round_trips_through_decode() {
        // The FUNCTION2 records the encoder emits must decode back to the same
        // library payloads (and the keyspace must be intact) — the basis for
        // persisting functions across a save/load cycle.
        let lib = b"#!lua name=rt\nredis.register_function('g', function() return 9 end)";
        let entries = vec![RdbEntry {
            db: 0,
            key: b"k".to_vec(),
            value: RdbValue::String(b"v".to_vec()),
            expire_ms: None,
        }];
        let encoded = encode_rdb_with_functions(&entries, &[], &[lib.as_slice()]);
        let decoded = decode_rdb_prefix(&encoded).expect("round-trip decode");
        assert_eq!(decoded.consumed, encoded.len());
        assert_eq!(decoded.functions, vec![lib.to_vec()]);
        assert_eq!(decoded.entries, entries);
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
    fn crc64_slice_by_8_matches_bytewise_and_reports_ab_ratio() {
        // Reference byte-at-a-time CRC (the pre-slice-by-8 form; table[0] is the
        // same single byte table). Isomorphism guard: slice-by-8 must be
        // bit-identical for every length class, including all chunks_exact(8)
        // remainder sizes 0..8. (frankenredis-3qhkr)
        fn bytewise(data: &[u8]) -> u64 {
            let mut crc = 0u64;
            for &b in data {
                crc = (crc >> 8) ^ super::CRC64_REDIS_SLICE[0][((crc as u8) ^ b) as usize];
            }
            crc
        }
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut buf: Vec<u8> = Vec::new();
        for len in 0..600usize {
            buf.clear();
            while buf.len() < len {
                buf.extend_from_slice(&next().to_ne_bytes());
            }
            buf.truncate(len);
            assert_eq!(crc64_redis(&buf), bytewise(&buf), "len={len}");
        }

        // A/B over a large buffer (run with `--release -- --nocapture`).
        let n = 32 * 1024 * 1024;
        let mut big = vec![0u8; n];
        for (i, b) in big.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let expected = bytewise(&big);
        let reps = 5;
        let t0 = std::time::Instant::now();
        let mut acc = 0u64;
        for _ in 0..reps {
            acc ^= bytewise(std::hint::black_box(&big));
        }
        let bytewise_ns = t0.elapsed().as_nanos().max(1);
        std::hint::black_box(acc);
        let t1 = std::time::Instant::now();
        let mut acc2 = 0u64;
        for _ in 0..reps {
            acc2 ^= crc64_redis(std::hint::black_box(&big));
        }
        let slice_ns = t1.elapsed().as_nanos().max(1);
        std::hint::black_box(acc2);
        assert_eq!(crc64_redis(&big), expected);
        let ratio = bytewise_ns as f64 / slice_ns as f64;
        println!(
            "CRC64 A/B over {n} bytes x{reps}: bytewise={bytewise_ns}ns slice8={slice_ns}ns ratio={ratio:.2}x"
        );
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
    fn lzf_compress_matches_vendored_wire_format_for_all_xs_z4tsz() {
        // (frankenredis-z4tsz) Vendored lzf_c.c emits an LZF stream that
        // differs from a tighter encoder for all-repeating inputs because:
        //   (a) line 164 requires ref > in_data — no back-reference to the
        //       very first byte, so position 1 can't match position 0; and
        //   (b) line 175 caps maxlen at in_end - ip - len (i.e. - 2), so
        //       the trailing 2 bytes always land in a literal run.
        // For input "x" * 30, vendored emits exactly:
        //   01 78 78 e0 11 00 01 78 78  (9 bytes)
        // i.e. [literal-2, 'x', 'x', long-match-26, literal-2, 'x', 'x'].
        let input: Vec<u8> = vec![b'x'; 30];
        let out =
            lzf_compress(&input, input.len() - 4).expect("compression of 30 x's must succeed");
        assert_eq!(
            out,
            vec![0x01, b'x', b'x', 0xe0, 0x11, 0x00, 0x01, b'x', b'x'],
            "fr LZF wire form must match vendored byte-for-byte for 'x' * 30",
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
        assert_eq!(strip_stream_metadata(decoded), entries);

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
        assert_eq!(strip_stream_metadata(decoded), entries);
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

    // ── Compact-type encoder selection (br-frankenredis-91kt) ─────

    /// Helper: scan `encoded` for the leading type byte of the entry
    /// for `key`. Skips REDIS magic, AUX records, SELECTDB / RESIZEDB,
    /// and any expiry opcodes. Used by the compact-encoder tests to
    /// confirm the chosen RDB type tag without decoding the full file.
    fn type_byte_for_key(encoded: &[u8], key: &[u8]) -> Option<u8> {
        let (_, aux) = decode_rdb(encoded).ok()?;
        // Sanity: header should have at least one aux field for
        // upstream-shaped output (we don't pin a specific one here).
        let _ = aux;
        // Re-walk the bytes manually to find the type byte preceding
        // the key marker. The key wire form is rdb_encode_length(len)
        // + key_bytes; we look for that exact 2..N-byte sequence.
        let mut wire_key = Vec::new();
        rdb_encode_length(&mut wire_key, key.len());
        wire_key.extend_from_slice(key);
        let mut idx = 0;
        while idx + wire_key.len() <= encoded.len() {
            if encoded[idx..idx + wire_key.len()] == wire_key[..] {
                if idx > 0 {
                    return Some(encoded[idx - 1]);
                }
                return None;
            }
            idx += 1;
        }
        None
    }

    fn compact_listpack_payload_for_key(encoded: &[u8], key: &[u8]) -> Option<Vec<u8>> {
        let mut wire_key = Vec::new();
        rdb_encode_length(&mut wire_key, key.len());
        wire_key.extend_from_slice(key);
        let mut idx = 0;
        while idx + wire_key.len() <= encoded.len() {
            if encoded[idx..idx + wire_key.len()] == wire_key[..] {
                let cursor = idx + wire_key.len();
                let (payload, _) = rdb_decode_string(&encoded[cursor..])?;
                return Some(payload);
            }
            idx += 1;
        }
        None
    }

    #[test]
    fn encode_rdb_default_options_emit_canonical_type_tags() {
        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"s_canon".to_vec(),
                value: RdbValue::Set(vec![b"alpha".to_vec(), b"beta".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"h_canon".to_vec(),
                value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_canon".to_vec(),
                value: RdbValue::SortedSet(vec![(b"a".to_vec(), 1.0)]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"l_canon".to_vec(),
                value: RdbValue::List(vec![b"x".to_vec(), b"y".to_vec()]),
                expire_ms: None,
            },
        ];
        let encoded = encode_rdb_with_options(&entries, &[], RdbEncodeOptions::default());
        // Default `RdbEncodeOptions` remains the explicit canonical-only
        // opt-out for tests or call sites that need the historical tags.
        assert_eq!(type_byte_for_key(&encoded, b"s_canon"), Some(RDB_TYPE_SET));
        assert_eq!(type_byte_for_key(&encoded, b"h_canon"), Some(RDB_TYPE_HASH));
        assert_eq!(
            type_byte_for_key(&encoded, b"z_canon"),
            Some(RDB_TYPE_ZSET_2)
        );
        assert_eq!(type_byte_for_key(&encoded, b"l_canon"), Some(RDB_TYPE_LIST));
    }

    #[test]
    fn encode_rdb_default_wrapper_emits_compact_type_tags() {
        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"s_intset".to_vec(),
                value: RdbValue::Set(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"s_listpack".to_vec(),
                value: RdbValue::Set(vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"h_listpack".to_vec(),
                value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_listpack".to_vec(),
                value: RdbValue::SortedSet(vec![(b"a".to_vec(), 1.0)]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"l_quicklist".to_vec(),
                value: RdbValue::List(vec![b"x".to_vec(), b"y".to_vec()]),
                expire_ms: None,
            },
        ];
        let encoded = encode_rdb(&entries, &[]);
        assert_eq!(
            type_byte_for_key(&encoded, b"s_intset"),
            Some(RDB_TYPE_SET_INTSET)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"s_listpack"),
            Some(RDB_TYPE_SET_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"h_listpack"),
            Some(RDB_TYPE_HASH_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_listpack"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"l_quicklist"),
            Some(RDB_TYPE_LIST_QUICKLIST_2)
        );
    }

    #[test]
    fn encode_rdb_compact_zset_listpack_orders_scores_like_redis() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"z".to_vec(),
            value: RdbValue::SortedSet(vec![
                (b"c".to_vec(), 7.25),
                (b"b".to_vec(), 2.5),
                (b"aa".to_vec(), 1.0),
                (b"a".to_vec(), 1.0),
            ]),
            expire_ms: None,
        }];

        let encoded = encode_rdb(&entries, &[]);
        assert_eq!(
            type_byte_for_key(&encoded, b"z"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );

        let (decoded, _) = decode_rdb(&encoded).expect("decode compact zset listpack");
        match &decoded[0].value {
            RdbValue::SortedSet(members) => {
                let actual: Vec<(&[u8], f64)> = members
                    .iter()
                    .map(|(member, score)| (member.as_slice(), *score))
                    .collect();
                let expected: Vec<(&[u8], f64)> = vec![
                    (&b"a"[..], 1.0),
                    (&b"aa"[..], 1.0),
                    (&b"b"[..], 2.5),
                    (&b"c"[..], 7.25),
                ];
                assert_eq!(actual, expected);
            }
            other => assert!(
                matches!(other, RdbValue::SortedSet(_)),
                "expected sorted set after compact zset decode"
            ),
        }
    }

    #[test]
    fn encode_rdb_compact_zset_presorted_input_is_byte_identical() {
        let presorted = vec![RdbEntry {
            db: 0,
            key: b"z".to_vec(),
            value: RdbValue::SortedSet(vec![
                (b"neg".to_vec(), -7.0),
                (b"same-a".to_vec(), 2.0),
                (b"same-b".to_vec(), 2.0),
                (b"frac".to_vec(), 3.5),
                (b"large".to_vec(), 4095.0),
            ]),
            expire_ms: None,
        }];
        let unsorted = vec![RdbEntry {
            db: 0,
            key: b"z".to_vec(),
            value: RdbValue::SortedSet(vec![
                (b"large".to_vec(), 4095.0),
                (b"same-b".to_vec(), 2.0),
                (b"neg".to_vec(), -7.0),
                (b"frac".to_vec(), 3.5),
                (b"same-a".to_vec(), 2.0),
            ]),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&presorted, &[]);

        assert_eq!(encoded, encode_rdb(&unsorted, &[]));
        assert_eq!(
            type_byte_for_key(&encoded, b"z"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );
    }

    #[test]
    fn encode_rdb_compact_listpacks_integer_encode_numeric_strings_like_redis() {
        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"s_listpack".to_vec(),
                value: RdbValue::Set(vec![b"alpha".to_vec(), b"127".to_vec(), b"4095".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"h_listpack".to_vec(),
                value: RdbValue::Hash(vec![
                    (b"field".to_vec(), b"127".to_vec()),
                    (b"4095".to_vec(), b"value".to_vec()),
                ]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_listpack".to_vec(),
                value: RdbValue::SortedSet(vec![
                    (b"127".to_vec(), 1.0),
                    (b"member".to_vec(), 4095.0),
                    (b"other".to_vec(), 2.5),
                ]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_int_listpack".to_vec(),
                value: RdbValue::SortedSet(vec![
                    (b"same-b".to_vec(), 2.0),
                    (b"large".to_vec(), 4095.0),
                    (b"zero".to_vec(), 0.0),
                    (b"same-a".to_vec(), 2.0),
                    (b"neg".to_vec(), -7.0),
                ]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_mixed_listpack".to_vec(),
                value: RdbValue::SortedSet(vec![
                    (b"frac-b".to_vec(), 3.5),
                    (b"int-high".to_vec(), 4.0),
                    (b"frac-a".to_vec(), 3.5),
                    (b"neg".to_vec(), -2.0),
                ]),
                expire_ms: None,
            },
        ];

        let encoded = encode_rdb(&entries, &[]);
        assert_eq!(
            type_byte_for_key(&encoded, b"s_listpack"),
            Some(RDB_TYPE_SET_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"h_listpack"),
            Some(RDB_TYPE_HASH_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_listpack"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_int_listpack"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_mixed_listpack"),
            Some(RDB_TYPE_ZSET_LISTPACK)
        );

        let set_payload = compact_listpack_payload_for_key(&encoded, b"s_listpack")
            .expect("set listpack payload");
        let set_entries = crate::listpack::decode_listpack(&set_payload).expect("decode set lp");
        assert_eq!(
            set_entries,
            vec![
                crate::listpack::ListpackEntry::String(b"alpha".to_vec()),
                crate::listpack::ListpackEntry::Integer(127),
                crate::listpack::ListpackEntry::Integer(4095),
            ]
        );

        let hash_payload = compact_listpack_payload_for_key(&encoded, b"h_listpack")
            .expect("hash listpack payload");
        let hash_entries = crate::listpack::decode_listpack(&hash_payload).expect("decode hash lp");
        assert_eq!(
            hash_entries,
            vec![
                crate::listpack::ListpackEntry::String(b"field".to_vec()),
                crate::listpack::ListpackEntry::Integer(127),
                crate::listpack::ListpackEntry::Integer(4095),
                crate::listpack::ListpackEntry::String(b"value".to_vec()),
            ]
        );

        let zset_payload = compact_listpack_payload_for_key(&encoded, b"z_listpack")
            .expect("zset listpack payload");
        let zset_entries = crate::listpack::decode_listpack(&zset_payload).expect("decode zset lp");
        assert_eq!(
            zset_entries,
            vec![
                crate::listpack::ListpackEntry::Integer(127),
                crate::listpack::ListpackEntry::Integer(1),
                crate::listpack::ListpackEntry::String(b"other".to_vec()),
                crate::listpack::ListpackEntry::String(b"2.5".to_vec()),
                crate::listpack::ListpackEntry::String(b"member".to_vec()),
                crate::listpack::ListpackEntry::Integer(4095),
            ]
        );

        let zset_int_payload = compact_listpack_payload_for_key(&encoded, b"z_int_listpack")
            .expect("integer zset listpack payload");
        let zset_int_entries =
            crate::listpack::decode_listpack(&zset_int_payload).expect("decode integer zset lp");
        assert_eq!(
            zset_int_entries,
            vec![
                crate::listpack::ListpackEntry::String(b"neg".to_vec()),
                crate::listpack::ListpackEntry::Integer(-7),
                crate::listpack::ListpackEntry::String(b"zero".to_vec()),
                crate::listpack::ListpackEntry::Integer(0),
                crate::listpack::ListpackEntry::String(b"same-a".to_vec()),
                crate::listpack::ListpackEntry::Integer(2),
                crate::listpack::ListpackEntry::String(b"same-b".to_vec()),
                crate::listpack::ListpackEntry::Integer(2),
                crate::listpack::ListpackEntry::String(b"large".to_vec()),
                crate::listpack::ListpackEntry::Integer(4095),
            ]
        );

        let zset_mixed_payload = compact_listpack_payload_for_key(&encoded, b"z_mixed_listpack")
            .expect("mixed-score zset listpack payload");
        let zset_mixed_entries =
            crate::listpack::decode_listpack(&zset_mixed_payload).expect("decode mixed zset lp");
        assert_eq!(
            zset_mixed_entries,
            vec![
                crate::listpack::ListpackEntry::String(b"neg".to_vec()),
                crate::listpack::ListpackEntry::Integer(-2),
                crate::listpack::ListpackEntry::String(b"frac-a".to_vec()),
                crate::listpack::ListpackEntry::String(b"3.5".to_vec()),
                crate::listpack::ListpackEntry::String(b"frac-b".to_vec()),
                crate::listpack::ListpackEntry::String(b"3.5".to_vec()),
                crate::listpack::ListpackEntry::String(b"int-high".to_vec()),
                crate::listpack::ListpackEntry::Integer(4),
            ]
        );
    }

    #[test]
    fn compact_hash_listpack_direct_emit_matches_flat_reference() {
        let fields = vec![
            (b"field".to_vec(), b"127".to_vec()),
            (b"4095".to_vec(), b"value".to_vec()),
            (b"neg".to_vec(), b"-2".to_vec()),
            (b"bytes".to_vec(), b"hello\0world".to_vec()),
        ];
        let direct = encode_hash_listpack_blob(&fields).expect("direct hash listpack");
        let flat: Vec<&[u8]> = fields
            .iter()
            .flat_map(|(field, value)| [field.as_slice(), value.as_slice()])
            .collect();
        let reference = encode_listpack_strings_blob(&flat).expect("flat hash listpack reference");

        assert_eq!(direct, reference);
        assert_eq!(
            u16::from_le_bytes(direct[4..6].try_into().expect("listpack count")),
            flat.len() as u16
        );

        let decoded = crate::listpack::decode_listpack(&direct).expect("decode direct hash lp");
        assert_eq!(
            decoded,
            vec![
                crate::listpack::ListpackEntry::String(b"field".to_vec()),
                crate::listpack::ListpackEntry::Integer(127),
                crate::listpack::ListpackEntry::Integer(4095),
                crate::listpack::ListpackEntry::String(b"value".to_vec()),
                crate::listpack::ListpackEntry::String(b"neg".to_vec()),
                crate::listpack::ListpackEntry::Integer(-2),
                crate::listpack::ListpackEntry::String(b"bytes".to_vec()),
                crate::listpack::ListpackEntry::String(b"hello\0world".to_vec()),
            ]
        );
    }

    #[test]
    fn compact_set_listpack_direct_emit_matches_flat_reference() {
        let members = vec![
            b"alpha".to_vec(),
            b"127".to_vec(),
            b"4095".to_vec(),
            b"-2".to_vec(),
            b"hello\0world".to_vec(),
        ];
        let direct = encode_set_listpack_blob(&members).expect("direct set listpack");
        let flat: Vec<&[u8]> = members.iter().map(Vec::as_slice).collect();
        let reference = encode_listpack_strings_blob(&flat).expect("flat set listpack reference");

        assert_eq!(direct, reference);
        assert_eq!(
            u16::from_le_bytes(direct[4..6].try_into().expect("listpack count")),
            flat.len() as u16
        );

        let decoded = crate::listpack::decode_listpack(&direct).expect("decode direct set lp");
        assert_eq!(
            decoded,
            vec![
                crate::listpack::ListpackEntry::String(b"alpha".to_vec()),
                crate::listpack::ListpackEntry::Integer(127),
                crate::listpack::ListpackEntry::Integer(4095),
                crate::listpack::ListpackEntry::Integer(-2),
                crate::listpack::ListpackEntry::String(b"hello\0world".to_vec()),
            ]
        );
    }

    #[test]
    fn encode_rdb_with_compact_options_emits_compact_type_tags() {
        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"s_intset".to_vec(),
                value: RdbValue::Set(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"s_listpack".to_vec(),
                value: RdbValue::Set(vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"h_listpack".to_vec(),
                value: RdbValue::Hash(vec![
                    (b"f1".to_vec(), b"v1".to_vec()),
                    (b"f2".to_vec(), b"v2".to_vec()),
                ]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_listpack".to_vec(),
                value: RdbValue::SortedSet(vec![
                    (b"a".to_vec(), 1.0),
                    (b"b".to_vec(), 2.5),
                    (b"c".to_vec(), 7.25),
                ]),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"l_quicklist".to_vec(),
                value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]),
                expire_ms: None,
            },
        ];
        let opts = RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        };
        let encoded = encode_rdb_with_options(&entries, &[], opts);

        assert_eq!(
            type_byte_for_key(&encoded, b"s_intset"),
            Some(RDB_TYPE_SET_INTSET),
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"s_listpack"),
            Some(RDB_TYPE_SET_LISTPACK),
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"h_listpack"),
            Some(RDB_TYPE_HASH_LISTPACK),
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_listpack"),
            Some(RDB_TYPE_ZSET_LISTPACK),
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"l_quicklist"),
            Some(RDB_TYPE_LIST_QUICKLIST_2),
        );

        // Round-trip back through decode_rdb — every emitted shape
        // must restore byte-identical content.
        let (decoded, _) = decode_rdb(&encoded).expect("compact-encoded RDB should decode");
        assert_eq!(decoded.len(), entries.len());
        for original in &entries {
            let restored = decoded
                .iter()
                .find(|d| d.key == original.key)
                .unwrap_or_else(|| panic!("missing key {:?} in decoded", original.key));
            match (&original.value, &restored.value) {
                (RdbValue::Set(a), RdbValue::Set(b)) => {
                    let mut a = a.clone();
                    let mut b = b.clone();
                    a.sort();
                    b.sort();
                    assert_eq!(a, b, "set round-trip drift on key {:?}", original.key);
                }
                (RdbValue::Hash(a), RdbValue::Hash(b)) => {
                    let mut a = a.clone();
                    let mut b = b.clone();
                    a.sort();
                    b.sort();
                    assert_eq!(a, b, "hash round-trip drift on key {:?}", original.key);
                }
                (RdbValue::SortedSet(a), RdbValue::SortedSet(b)) => {
                    assert_eq!(a.len(), b.len());
                    for ((am, asc), (bm, bsc)) in a.iter().zip(b.iter()) {
                        assert_eq!(am, bm, "zset member drift");
                        assert!((asc - bsc).abs() < 1e-9, "zset score drift");
                    }
                }
                (RdbValue::List(a), RdbValue::List(b)) => {
                    assert_eq!(a, b, "list round-trip drift on key {:?}", original.key);
                }
                _ => panic!("kind mismatch on key {:?}", original.key),
            }
        }
    }

    #[test]
    fn encode_rdb_compact_falls_back_to_canonical_above_thresholds() {
        // Set with all-integer members BUT >set_max_intset_entries:
        // upstream falls through intset → listpack → hashtable. With
        // 600 entries (over both intset=512 and listpack=128), the
        // canonical RDB_TYPE_SET should win.
        let big_int_members: Vec<Vec<u8>> =
            (0..600).map(|i: i64| i.to_string().into_bytes()).collect();
        // Hash with one value > hash_max_listpack_value (64 bytes).
        let big_hash = vec![(b"f".to_vec(), vec![b'x'; 100])];
        // Set with one member > set_max_listpack_value.
        let big_set = vec![b"a".to_vec(), vec![b'b'; 100]];
        // Zset with one member > zset_max_listpack_value.
        let big_zset = vec![(b"a".to_vec(), 1.0), (vec![b'm'; 100], 2.0)];

        let entries = vec![
            RdbEntry {
                db: 0,
                key: b"s_big_int".to_vec(),
                value: RdbValue::Set(big_int_members),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"h_big_value".to_vec(),
                value: RdbValue::Hash(big_hash),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"s_big_value".to_vec(),
                value: RdbValue::Set(big_set),
                expire_ms: None,
            },
            RdbEntry {
                db: 0,
                key: b"z_big_value".to_vec(),
                value: RdbValue::SortedSet(big_zset),
                expire_ms: None,
            },
        ];
        let opts = RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        };
        let encoded = encode_rdb_with_options(&entries, &[], opts);

        // All four must fall back to canonical (non-compact) tags.
        assert_eq!(
            type_byte_for_key(&encoded, b"s_big_int"),
            Some(RDB_TYPE_SET),
            "600-entry integer set must overshoot intset threshold (512) into RDB_TYPE_SET",
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"h_big_value"),
            Some(RDB_TYPE_HASH),
            "100-byte value must overshoot hash listpack value threshold (64) into RDB_TYPE_HASH",
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"s_big_value"),
            Some(RDB_TYPE_SET),
            "100-byte member must overshoot set listpack value threshold (64) into RDB_TYPE_SET",
        );
        assert_eq!(
            type_byte_for_key(&encoded, b"z_big_value"),
            Some(RDB_TYPE_ZSET_2),
            "100-byte member must overshoot zset listpack value threshold (64) into RDB_TYPE_ZSET_2",
        );

        // Decode round-trip must still succeed (the canonical-shape
        // emitter has been on this path the whole time).
        let (decoded, _) = decode_rdb(&encoded).expect("compact fallback RDB should decode");
        assert_eq!(decoded.len(), entries.len());
    }

    #[test]
    fn encode_rdb_compact_set_intset_rejects_non_canonical_integers() {
        // Members "+1" and "01" parse as integers but DON'T round-trip
        // to the same byte string — upstream's intset encoder rejects
        // these. The encoder must fall back to the listpack form (or
        // canonical hashtable if listpack is over threshold).
        let opts = RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        };
        for non_canonical in [b"+1".to_vec(), b"01".to_vec(), b" 1".to_vec()] {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"s".to_vec(),
                value: RdbValue::Set(vec![non_canonical.clone(), b"2".to_vec()]),
                expire_ms: None,
            }];
            let encoded = encode_rdb_with_options(&entries, &[], opts);
            // Must not be intset — intset would round-trip "+1" to "1"
            // and lose the original byte form.
            assert_ne!(
                type_byte_for_key(&encoded, b"s"),
                Some(RDB_TYPE_SET_INTSET),
                "non-canonical integer member {:?} must NOT trigger intset encoding",
                non_canonical,
            );
        }
    }

    #[test]
    fn compact_set_intset_noalloc_canonical_check_matches_roundtrip_oracle() {
        fn old_roundtrip_accepts(raw: &[u8]) -> bool {
            let Ok(s) = std::str::from_utf8(raw) else {
                return false;
            };
            let Ok(value) = s.parse::<i64>() else {
                return false;
            };
            value.to_string().as_bytes() == raw
        }

        let thresholds = CompactRdbThresholds::default();
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![
                b"0".to_vec(),
                b"1".to_vec(),
                b"-2".to_vec(),
                b"9223372036854775807".to_vec(),
            ],
            vec![
                b"-9223372036854775808".to_vec(),
                b"42".to_vec(),
                b"-4096".to_vec(),
            ],
            vec![b"+1".to_vec(), b"2".to_vec()],
            vec![b"01".to_vec(), b"2".to_vec()],
            vec![b"-0".to_vec(), b"2".to_vec()],
            vec![b" 1".to_vec(), b"2".to_vec()],
            vec![b"1 ".to_vec(), b"2".to_vec()],
            vec![b"9223372036854775808".to_vec(), b"2".to_vec()],
            vec![b"-9223372036854775809".to_vec(), b"2".to_vec()],
            vec![b"\xff".to_vec(), b"2".to_vec()],
        ];

        for members in cases {
            let expected = members.iter().all(|raw| old_roundtrip_accepts(raw));
            assert_eq!(
                encode_compact_set_intset(&members, &thresholds).is_some(),
                expected,
                "compact intset selection drift for {members:?}"
            );
        }
    }

    /// br-frankenredis-91kt acceptance: for compact-encoded
    /// upstream-shaped RDB blobs (the seeds emitted by
    /// `fuzz/scripts/gen_compact_rdb_seeds.py`), `decode_rdb` followed
    /// by the default `encode_rdb` wrapper must re-emit the same compact
    /// payload byte-for-byte. This covers the compact type families whose
    /// encoder selection used to drift to canonical tags.
    #[test]
    fn compact_encoder_reemits_upstream_shaped_seeds_byte_identically() {
        let corpus_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus/fuzz_rdb_decoder");
        if !corpus_root.exists() {
            eprintln!(
                "[SKIP] fuzz corpus dir {} not present",
                corpus_root.display()
            );
            return;
        }

        let zset_listpack = encode_listpack_strings_blob(&[
            b"d".as_slice(),
            b"-3.14159".as_slice(),
            b"a".as_slice(),
            b"1".as_slice(),
            b"b".as_slice(),
            b"2.5".as_slice(),
            b"c".as_slice(),
            b"7.25".as_slice(),
        ])
        .expect("encode sorted compact zset listpack");
        let mut zset_payload = Vec::new();
        rdb_encode_length(&mut zset_payload, zset_listpack.len());
        zset_payload.extend_from_slice(&zset_listpack);

        let mut well_formed: Vec<(&str, u8, Vec<u8>, Vec<u8>)> = Vec::new();
        for (seed_name, expected_tag, key) in [
            (
                "compact_intset_small_16bit",
                RDB_TYPE_SET_INTSET,
                b"si".as_slice(),
            ),
            (
                "compact_set_listpack",
                RDB_TYPE_SET_LISTPACK,
                b"slp".as_slice(),
            ),
            (
                "compact_hash_listpack",
                RDB_TYPE_HASH_LISTPACK,
                b"hlp".as_slice(),
            ),
            (
                "compact_quicklist2_packed",
                RDB_TYPE_LIST_QUICKLIST_2,
                b"lq".as_slice(),
            ),
        ] {
            let path = corpus_root.join(seed_name);
            let bytes = std::fs::read(&path).expect("read compact corpus seed");
            well_formed.push((seed_name, expected_tag, key.to_vec(), bytes));
        }
        well_formed.push((
            "compact_zset_listpack_sorted_by_score",
            RDB_TYPE_ZSET_LISTPACK,
            b"zlp".to_vec(),
            encode_single_raw_rdb_entry(RDB_TYPE_ZSET_LISTPACK, b"zlp", &zset_payload),
        ));

        for (seed_name, expected_tag, key, seed_bytes) in well_formed {
            let (entries, _) =
                decode_rdb(&seed_bytes).expect("decode upstream-shaped compact seed");

            let re_emitted = encode_rdb(&entries, &[]);

            let re_emitted_tag =
                type_byte_for_key(&re_emitted, &key).expect("re-emitted compact RDB missing key");
            assert_eq!(
                re_emitted_tag,
                expected_tag,
                "compact re-emit drifted: seed {seed_name} (key {:?}) decoded → \
                 re-encoded with type byte 0x{:02X}, expected 0x{:02X}",
                std::str::from_utf8(&key).unwrap_or("?"),
                re_emitted_tag,
                expected_tag,
            );

            assert_eq!(
                re_emitted, seed_bytes,
                "compact re-emit must be byte-identical for seed {seed_name}"
            );
        }
    }

    /// Mirrors the invariants enforced by the libfuzzer harness in
    /// `fuzz/fuzz_targets/fuzz_rdb_encode_round_trip.rs`. Runs in
    /// regular `cargo test` (no nightly / libfuzzer infra needed) so a
    /// future regression in `encode_rdb_with_options` would be caught
    /// before it reaches the fuzz pipeline. Each fixture is a shape
    /// libfuzzer is known to mutate into via the seed corpus committed
    /// alongside the harness. (br-frankenredis-91kt fuzz follow-up)
    #[test]
    fn encode_rdb_round_trip_invariants_for_fuzz_seeds() {
        // Fixtures: (name, RdbValue, opts.compact)
        // Each invariant runs both sides (compact / canonical-only) so
        // a divergence in either path is caught.
        let fixtures: &[(&str, RdbValue)] = &[
            ("string_short", RdbValue::String(b"hello world".to_vec())),
            (
                "string_long",
                RdbValue::String(b"abc".repeat(50)), // 150 bytes — triggers LZF
            ),
            (
                "list_small",
                RdbValue::List(vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]),
            ),
            (
                "list_long_element",
                RdbValue::List(vec![b"x".repeat(8500)]), // overflows quicklist size
            ),
            (
                "set_intset",
                RdbValue::Set(vec![
                    b"1".to_vec(),
                    b"2".to_vec(),
                    b"3".to_vec(),
                    b"100".to_vec(),
                    b"-5".to_vec(),
                ]),
            ),
            (
                "set_listpack",
                RdbValue::Set(vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]),
            ),
            (
                "set_canonical",
                RdbValue::Set(
                    (0..200)
                        .map(|i: i32| format!("m{i}").into_bytes())
                        .collect(),
                ),
            ),
            (
                "hash_listpack",
                RdbValue::Hash(vec![
                    (b"f1".to_vec(), b"v1".to_vec()),
                    (b"f2".to_vec(), b"v2".to_vec()),
                    (b"f3".to_vec(), b"v3".to_vec()),
                ]),
            ),
            (
                "hash_canonical",
                RdbValue::Hash(
                    (0..600)
                        .map(|i: i32| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                        .collect(),
                ),
            ),
            (
                "zset_listpack",
                RdbValue::SortedSet(vec![
                    (b"a".to_vec(), 1.0),
                    (b"b".to_vec(), 2.5),
                    (b"c".to_vec(), -3.5),
                ]),
            ),
        ];

        for (name, value) in fixtures {
            for compact in [None, Some(CompactRdbThresholds::default())] {
                let label = if compact.is_some() {
                    "compact"
                } else {
                    "canonical"
                };
                let entries = vec![RdbEntry {
                    db: 0,
                    key: name.as_bytes().to_vec(),
                    value: value.clone(),
                    expire_ms: None,
                }];
                let opts = RdbEncodeOptions { compact };
                let encoded = encode_rdb_with_options(&entries, &[], opts);

                // Invariant: REDIS magic + 8-byte CRC trailer.
                assert!(
                    encoded.starts_with(b"REDIS"),
                    "[{label}] {name}: missing REDIS magic"
                );
                assert!(encoded.len() > 18, "[{label}] {name}: encoded too short");

                // Invariant: decode_rdb must accept what
                // encode_rdb_with_options produced.
                let (decoded, _aux) = decode_rdb(&encoded).unwrap_or_else(|e| {
                    panic!(
                        "[{label}] {name}: decode_rdb rejected encoder output: {e:?}\n\
                         encoded.len()={}\nfirst 64: {:02x?}",
                        encoded.len(),
                        &encoded[..encoded.len().min(64)],
                    )
                });
                assert_eq!(
                    decoded.len(),
                    1,
                    "[{label}] {name}: round-trip dropped entry"
                );
                assert_eq!(
                    decoded[0].key,
                    name.as_bytes(),
                    "[{label}] {name}: round-trip key drift",
                );

                // Invariant: shape-equivalent round-trip (under
                // canonicalisation for unordered shapes).
                match (value, &decoded[0].value) {
                    (RdbValue::String(a), RdbValue::String(b)) => {
                        assert_eq!(a, b, "[{label}] {name}: string drift")
                    }
                    (RdbValue::List(a), RdbValue::List(b)) => {
                        assert_eq!(a, b, "[{label}] {name}: list drift")
                    }
                    // A plain-RDB_TYPE_SET set decodes to SetHashtable, so a `Set`
                    // input that didn't fit listpack/intset round-trips to
                    // SetHashtable; compare members regardless of which set
                    // variant each side is. (frankenredis-39is8)
                    (
                        RdbValue::Set(a) | RdbValue::SetHashtable(a),
                        RdbValue::Set(b) | RdbValue::SetHashtable(b),
                    ) => {
                        let mut x = a.clone();
                        let mut y = b.clone();
                        x.sort();
                        y.sort();
                        assert_eq!(x, y, "[{label}] {name}: set drift");
                    }
                    (RdbValue::Hash(a), RdbValue::Hash(b)) => {
                        let mut x = a.clone();
                        let mut y = b.clone();
                        x.sort();
                        y.sort();
                        assert_eq!(x, y, "[{label}] {name}: hash drift");
                    }
                    (RdbValue::SortedSet(a), RdbValue::SortedSet(b)) => {
                        assert_eq!(a.len(), b.len(), "[{label}] {name}: zset card drift");
                        let mut x = a.clone();
                        let mut y = b.clone();
                        x.sort_by(|p, q| {
                            p.0.cmp(&q.0)
                                .then(p.1.partial_cmp(&q.1).unwrap_or(std::cmp::Ordering::Equal))
                        });
                        y.sort_by(|p, q| {
                            p.0.cmp(&q.0)
                                .then(p.1.partial_cmp(&q.1).unwrap_or(std::cmp::Ordering::Equal))
                        });
                        for (xi, yi) in x.iter().zip(y.iter()) {
                            assert_eq!(xi.0, yi.0, "[{label}] {name}: zset member drift");
                            assert!(
                                (xi.1 - yi.1).abs() < 1e-9,
                                "[{label}] {name}: zset score drift {} vs {}",
                                xi.1,
                                yi.1
                            );
                        }
                    }
                    _ => panic!("[{label}] {name}: kind mismatch"),
                }
            }
        }
    }

    #[test]
    fn encode_rdb_compact_zset_listpack_rejects_nan() {
        // Upstream's d2string can't represent NaN; the zset listpack
        // path must fall back rather than emit an unparseable wire form.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"z_nan".to_vec(),
            value: RdbValue::SortedSet(vec![(b"a".to_vec(), f64::NAN)]),
            expire_ms: None,
        }];
        let opts = RdbEncodeOptions {
            compact: Some(CompactRdbThresholds::default()),
        };
        let encoded = encode_rdb_with_options(&entries, &[], opts);
        assert_eq!(
            type_byte_for_key(&encoded, b"z_nan"),
            Some(RDB_TYPE_ZSET_2),
            "NaN score must force fallback to RDB_TYPE_ZSET_2",
        );
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
                // A plain RDB_TYPE_SET now decodes to SetHashtable; both are
                // "Set" for this round-trip kind check. (frankenredis-39is8)
                RdbValue::Set(members) | RdbValue::SetHashtable(members) => ("Set", members.len()),
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

    /// Rewrite the 4-digit version header in-place and repair the trailing
    /// CRC64 so the only thing under test is the version-range check (not an
    /// incidental checksum mismatch).
    fn reversion_rdb(encoded: &mut [u8], version: &[u8; 4]) {
        encoded[5..9].copy_from_slice(version);
        let crc_at = encoded.len() - RDB_CHECKSUM_LEN;
        let crc = crc64_redis(&encoded[..crc_at]);
        encoded[crc_at..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn rdb_rejects_too_new_version() {
        // A version greater than RDB_VERSION is a format we cannot know how to
        // read — redis rejects `rdbver > RDB_VERSION` and so must we, even with
        // a valid checksum.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"version".to_vec(),
            value: RdbValue::String(b"too-new".to_vec()),
            expire_ms: None,
        }];
        let mut encoded = encode_rdb(&entries, &[]);
        reversion_rdb(&mut encoded, b"0099");
        assert_eq!(
            decode_rdb(&encoded).unwrap_err(),
            PersistError::InvalidFrame
        );

        // Version 0 is also out of range (redis rejects `rdbver < 1`).
        let mut zero = encode_rdb(&entries, &[]);
        reversion_rdb(&mut zero, b"0000");
        assert_eq!(decode_rdb(&zero).unwrap_err(), PersistError::InvalidFrame);
    }

    #[test]
    fn rdb_accepts_older_supported_versions() {
        // Dumps from older redis releases (6.x = v9, 7.0/7.1 = v10) carry the
        // same version-stable type tags; with a valid checksum they must load,
        // matching redis which accepts any `1..=RDB_VERSION`.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"legacy-key".to_vec(),
            value: RdbValue::String(b"legacy-value".to_vec()),
            expire_ms: None,
        }];
        // encode_rdb always appends the v5+ CRC64 trailer, so this synthetic
        // round-trip covers the checksummed versions. The pre-v5 (no-trailer)
        // path is covered by the real v3/v4 redis fixtures below.
        for ver in [b"0005", b"0009", b"0010", b"0011"] {
            let mut encoded = encode_rdb(&entries, &[]);
            reversion_rdb(&mut encoded, ver);
            let (decoded, _aux) = decode_rdb(&encoded)
                .unwrap_or_else(|e| panic!("RDB version {ver:?} must load: {e:?}"));
            assert_eq!(decoded, entries, "version {ver:?} round-trip");
        }
    }

    /// Decode an ASCII-hex string into bytes (test helper).
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn rdb_decodes_legacy_ziplist_zipmap_quicklist_fixtures() {
        // Byte-for-byte RDB fixtures shipped in the redis test suite
        // (tests/assets/*.rdb), exercising the pre-listpack encodings. Expected
        // values are what a live redis 7.2.4 reports after loading each dump.

        // hash-ziplist.rdb (v9, RDB_TYPE_HASH_ZIPLIST=13) -> {f1:v1, f2:v2}
        let hash_zl = "524544495330303039fa0972656469732d7665720b3235352e3235352e323535fa0a72656469732d62697473c040fa056374696d65c2c85c9660fa08757365642d6d656dc290ad0c00fa0c616f662d707265616d626c65c000fe00fb01000d04686173681b1b00000016000000040000026631040276310402663204027632ffff4f9cd1fd16699883";
        let (e, _) = decode_rdb(&unhex(hash_zl)).expect("hash-ziplist must load");
        assert_eq!(
            e,
            vec![RdbEntry {
                db: 0,
                key: b"hash".to_vec(),
                value: RdbValue::Hash(vec![
                    (b"f1".to_vec(), b"v1".to_vec()),
                    (b"f2".to_vec(), b"v2".to_vec()),
                ]),
                expire_ms: None,
            }]
        );

        // zset-ziplist.rdb (v10, RDB_TYPE_ZSET_ZIPLIST=12) -> {one:1, two:2}
        let zset_zl = "524544495330303130fa0972656469732d7665720b3235352e3235352e323535fa0a72656469732d62697473c040fa056374696d65c262b71361fa08757365642d6d656dc250f40c00fa0c616f662d707265616d626c65c000fe00fb01000c047a736574191900000016000000040000036f6e6505f2020374776f05f3ffff1fb2fdf0997f9e19";
        let (e, _) = decode_rdb(&unhex(zset_zl)).expect("zset-ziplist must load");
        assert_eq!(
            e,
            vec![RdbEntry {
                db: 0,
                key: b"zset".to_vec(),
                value: RdbValue::SortedSet(vec![(b"one".to_vec(), 1.0), (b"two".to_vec(), 2.0),]),
                expire_ms: None,
            }]
        );

        // hash-zipmap.rdb (v3, RDB_TYPE_HASH_ZIPMAP=9) -> {f1:v1, f2:v2}
        let hash_zm = "524544495330303033fe0009046861736810020266310200763102663202007632ffff";
        let (e, _) = decode_rdb(&unhex(hash_zm)).expect("hash-zipmap must load");
        assert_eq!(
            e,
            vec![RdbEntry {
                db: 0,
                key: b"hash".to_vec(),
                value: RdbValue::Hash(vec![
                    (b"f1".to_vec(), b"v1".to_vec()),
                    (b"f2".to_vec(), b"v2".to_vec()),
                ]),
                expire_ms: None,
            }]
        );

        // list-quicklist.rdb (v8, RDB_TYPE_LIST_QUICKLIST=14 + int string) -> [7], x=7
        let list_ql = "524544495330303038fa0972656469732d76657205342e302e39fa0a72656469732d62697473c040fa056374696d65c29f062661fa08757365642d6d656dc280920700fa0c616f662d707265616d626c65c000fe00fb02000e046c697374010d0d0000000a000000010000f8ff000178c007ff3572f8541ac4d740";
        let (e, _) = decode_rdb(&unhex(list_ql)).expect("list-quicklist must load");
        assert_eq!(
            e,
            vec![
                RdbEntry {
                    db: 0,
                    key: b"list".to_vec(),
                    value: RdbValue::List(vec![b"7".to_vec()]),
                    expire_ms: None,
                },
                RdbEntry {
                    db: 0,
                    key: b"x".to_vec(),
                    value: RdbValue::String(b"7".to_vec()),
                    expire_ms: None,
                },
            ]
        );
    }

    #[test]
    fn rdb_missing_file_returns_empty() {
        let path = std::path::Path::new("/tmp/fr_persist_nonexistent_test_file.rdb");
        let (entries, _) = super::read_rdb_file(path).expect("read missing");
        assert!(entries.is_empty());
    }

    #[test]
    fn write_rdb_file_with_functions_persists_libraries_to_disk() {
        let dir = std::env::temp_dir().join("fr_persist_rdb_functions_disk_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("functions.rdb");
        let lib = b"#!lua name=disklib\nredis.register_function('d', function() return 1 end)";
        let entries = vec![RdbEntry {
            db: 0,
            key: b"k".to_vec(),
            value: RdbValue::String(b"v".to_vec()),
            expire_ms: None,
        }];
        super::write_rdb_file_with_functions(&path, &entries, &[], &[lib.as_slice()])
            .expect("write rdb with functions");

        let (read_entries, _aux, functions) =
            super::read_rdb_file_with_functions(&path).expect("read back");
        assert_eq!(read_entries, entries);
        assert_eq!(functions, vec![lib.to_vec()]);

        let _ = std::fs::remove_file(&path);
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

    /// (frankenredis-wt4eo) A decoded type-21 (Redis-compatible) stream
    /// carries the original STREAM_LISTPACKS_3 payload as transient
    /// `RdbStreamMetadata` for byte-identical re-encode; the live store never
    /// reads it (it rebuilds from the stream DATA). Round-trip tests assert on
    /// the data, so normalize that transient metadata away.
    fn strip_stream_metadata(mut entries: Vec<RdbEntry>) -> Vec<RdbEntry> {
        for entry in &mut entries {
            if let RdbValue::Stream(_, _, _, metadata, _, _) = &mut entry.value {
                *metadata = None;
            }
        }
        entries
    }

    #[test]
    fn stream_rdb_save_uses_upstream_compatible_type21() {
        // (frankenredis-wt4eo) A normal stream must SAVE as the Redis-compatible
        // STREAM_LISTPACKS_3 (type 21), NOT fr's legacy private type 15, so upstream
        // Redis can load fr's RDB files. The decoder only stashes
        // RdbStreamMetadata{upstream_type_byte:21} for an actual type-21 payload, so
        // its presence proves the upstream-compatible encoding was used.
        let entries = vec![RdbEntry {
            db: 0,
            key: b"s".to_vec(),
            value: RdbValue::Stream(
                vec![(1, 0, vec![(b"f".to_vec(), b"v".to_vec())])],
                Some((1, 0)),
                Vec::new(),
                None,
                Some(1),
                None,
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        match &decoded[0].value {
            RdbValue::Stream(_, _, _, Some(md), _, _) => {
                assert_eq!(
                    md.upstream_type_byte, UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3,
                    "stream must SAVE as type 21 (STREAM_LISTPACKS_3)"
                );
            }
            other => panic!("expected type-21 stream metadata on decode, got {other:?}"),
        }
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
                Some(2),
                None,
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
    }

    #[test]
    fn rdb_round_trip_stream_preserves_max_deleted_entry_id() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"mystream".to_vec(),
            value: RdbValue::Stream(
                vec![(1001, 0, vec![(b"name".to_vec(), b"Bob".to_vec())])],
                Some((1001, 0)),
                Vec::new(),
                None,
                Some(2),
                Some((1000, 0)),
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
    }

    #[test]
    fn rdb_round_trip_stream_no_watermark() {
        let entries = vec![RdbEntry {
            db: 0,
            key: b"emptystream".to_vec(),
            value: RdbValue::Stream(vec![], None, Vec::new(), None, Some(0), None),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
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
                Some(1),
                None,
            ),
            expire_ms: Some(9_999_999),
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
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
                    entries_read: None,
                    consumers: vec![
                        RdbStreamConsumer::named(b"alice".to_vec()),
                        RdbStreamConsumer::named(b"bob".to_vec()),
                    ],
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
                Some(2),
                None,
            ),
            expire_ms: None,
        }];
        let encoded = encode_rdb(&entries, &[]);
        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(strip_stream_metadata(decoded), entries);
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
                    entries_read: None,
                    consumers: vec![RdbStreamConsumer::named(b"alice".to_vec())],
                    pending: vec![RdbStreamPendingEntry {
                        entry_id_ms: 1000,
                        entry_id_seq: 0,
                        consumer: b"alice".to_vec(),
                        deliveries: 2,
                        last_delivered_ms: 5000,
                    }],
                }],
                None,
                Some(1),
                None,
            ),
            expire_ms: None,
        }];

        let encoded = encode_rdb(&entries, &[]);

        // Header + SELECTDB(2) + RESIZEDB(3) puts the first value type byte at 14.
        assert_eq!(encoded[14], UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3);

        let (decoded, _) = decode_rdb(&encoded).expect("decode");
        assert_eq!(decoded.len(), 1);
        let RdbValue::Stream(
            decoded_entries,
            decoded_watermark,
            decoded_groups,
            metadata,
            decoded_entries_added,
            _,
        ) = &decoded[0].value
        else {
            panic!("expected decoded stream");
        };
        let RdbValue::Stream(
            expected_entries,
            expected_watermark,
            expected_groups,
            _,
            expected_entries_added,
            _,
        ) = &entries[0].value
        else {
            panic!("expected source stream");
        };
        assert_eq!(decoded_entries, expected_entries);
        assert_eq!(decoded_watermark, expected_watermark);
        assert_eq!(decoded_groups, expected_groups);
        assert_eq!(decoded_entries_added, expected_entries_added);
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
                        entries_read: Some(1),
                        consumers: vec![RdbStreamConsumer::named(b"alice".to_vec())],
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
                        Some(2),
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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
                        entries_read: Some(1),
                        consumers: vec![RdbStreamConsumer {
                            name: b"alice".to_vec(),
                            seen_time_ms: 1100,
                            active_time_ms: Some(1200),
                        }],
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
                    Some(1),
                    None,
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
                Some(1),
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
                    Some(1),
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
        assert_eq!(strip_stream_metadata(decoded), entries);
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

        #[test]
        fn golden_rdb_resizedb_counts_multiple_databases() {
            let entries = vec![
                RdbEntry {
                    db: 7,
                    key: b"c".to_vec(),
                    value: RdbValue::String(b"vc".to_vec()),
                    expire_ms: Some(7000),
                },
                RdbEntry {
                    db: 2,
                    key: b"a".to_vec(),
                    value: RdbValue::String(b"va".to_vec()),
                    expire_ms: None,
                },
                RdbEntry {
                    db: 9,
                    key: b"d".to_vec(),
                    value: RdbValue::String(b"vd".to_vec()),
                    expire_ms: None,
                },
                RdbEntry {
                    db: 2,
                    key: b"b".to_vec(),
                    value: RdbValue::String(b"vb".to_vec()),
                    expire_ms: Some(2000),
                },
            ];
            let encoded = encode_rdb(&entries, &[]);
            let headers: Vec<[u8; 5]> = encoded
                .windows(5)
                .filter(|window| {
                    window[0] == RDB_OPCODE_SELECTDB && window[2] == RDB_OPCODE_RESIZEDB
                })
                .map(|window| [window[0], window[1], window[2], window[3], window[4]])
                .collect();

            assert_eq!(
                headers,
                vec![
                    [RDB_OPCODE_SELECTDB, 2, RDB_OPCODE_RESIZEDB, 2, 1],
                    [RDB_OPCODE_SELECTDB, 7, RDB_OPCODE_RESIZEDB, 1, 1],
                    [RDB_OPCODE_SELECTDB, 9, RDB_OPCODE_RESIZEDB, 1, 0],
                ],
            );
        }

        /// Golden test: RDB list type encoding uses the upstream compact type byte.
        #[test]
        fn golden_rdb_list_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"mylist".to_vec(),
                value: RdbValue::List(vec![b"a".to_vec(), b"b".to_vec()]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            assert_eq!(
                type_byte_for_key(&encoded, b"mylist"),
                Some(RDB_TYPE_LIST_QUICKLIST_2),
                "RDB list must use LIST_QUICKLIST_2 (0x12)"
            );
        }

        /// Golden test: RDB set type encoding uses the upstream compact type byte.
        #[test]
        fn golden_rdb_set_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myset".to_vec(),
                value: RdbValue::Set(vec![b"x".to_vec()]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            assert_eq!(
                type_byte_for_key(&encoded, b"myset"),
                Some(RDB_TYPE_SET_LISTPACK),
                "RDB non-integer set must use SET_LISTPACK (0x14)"
            );
        }

        /// Golden test: RDB hash type encoding uses the upstream compact type byte.
        #[test]
        fn golden_rdb_hash_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myhash".to_vec(),
                value: RdbValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            assert_eq!(
                type_byte_for_key(&encoded, b"myhash"),
                Some(RDB_TYPE_HASH_LISTPACK),
                "RDB hash must use HASH_LISTPACK (0x10)"
            );
        }

        /// Golden test: RDB sorted set type encoding uses the upstream compact type byte.
        #[test]
        fn golden_rdb_zset_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"myzset".to_vec(),
                value: RdbValue::SortedSet(vec![(b"member".to_vec(), 1.5)]),
                expire_ms: None,
            }];
            let encoded = encode_rdb(&entries, &[]);

            assert_eq!(
                type_byte_for_key(&encoded, b"myzset"),
                Some(RDB_TYPE_ZSET_LISTPACK),
                "RDB sorted set must use ZSET_LISTPACK (0x11)"
            );
        }

        /// Golden test: RDB stream type encoding uses correct type byte.
        #[test]
        fn golden_rdb_stream_type() {
            let entries = vec![RdbEntry {
                db: 0,
                key: b"mystream".to_vec(),
                value: RdbValue::Stream(vec![], None, Vec::new(), None, Some(0), None),
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
                prop::collection::vec(stream_consumer_strategy(), 0..=3),
                prop::collection::vec(stream_pending_entry_strategy(), 0..=3),
            )
                .prop_map(
                    |(name, last_delivered_id_ms, last_delivered_id_seq, consumers, pending)| {
                        RdbStreamConsumerGroup {
                            name,
                            last_delivered_id_ms,
                            last_delivered_id_seq,
                            entries_read: None,
                            consumers,
                            pending,
                        }
                    },
                )
        }

        fn stream_consumer_strategy() -> impl Strategy<Value = RdbStreamConsumer> {
            (
                non_empty_byte_vec_strategy(8),
                0_u64..=10_000,
                prop::option::of(0_u64..=10_000),
            )
                .prop_map(|(name, seen_time_ms, active_time_ms)| RdbStreamConsumer {
                    name,
                    seen_time_ms,
                    active_time_ms,
                })
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
                        let entries_added = u64::try_from(entries.len()).unwrap_or(u64::MAX);
                        RdbValue::Stream(
                            entries,
                            watermark,
                            groups,
                            None,
                            Some(entries_added),
                            None,
                        )
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

        fn normalize_entries_for_semantic_compare(mut entries: Vec<RdbEntry>) -> Vec<RdbEntry> {
            for entry in &mut entries {
                match &mut entry.value {
                    RdbValue::Set(members) => members.sort(),
                    RdbValue::Hash(fields) => fields.sort(),
                    RdbValue::SortedSet(members) => {
                        members.sort_by(|left, right| {
                            left.1
                                .partial_cmp(&right.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| left.0.cmp(&right.0))
                        });
                    }
                    // (frankenredis-wt4eo) Drop the transient lossless
                    // re-encode payload a decoded type-21 stream carries.
                    RdbValue::Stream(_, _, _, metadata, _, _) => *metadata = None,
                    _ => {}
                }
            }
            entries.sort_by(|left, right| {
                left.db
                    .cmp(&right.db)
                    .then_with(|| left.key.cmp(&right.key))
                    .then_with(|| left.expire_ms.cmp(&right.expire_ms))
            });
            entries
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

            // (frankenredis-cc aofreclen) The alloc-free length MUST exactly equal
            // the materialized-and-encoded length it replaces on the propagation path.
            #[test]
            fn aof_record_encoded_resp_len_matches_to_bytes(record in aof_record_strategy()) {
                prop_assert_eq!(record.encoded_resp_len(), record.to_resp_frame().to_bytes().len());
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
                prop_assert_eq!(
                    normalize_entries_for_semantic_compare(decoded_entries),
                    normalize_entries_for_semantic_compare(entries),
                );
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
                prop::collection::vec(stream_consumer_strategy(), 0..=3),
                prop::collection::vec(stream_pending_entry_strategy(), 0..=3),
            )
                .prop_map(
                    |(name, last_delivered_id_ms, last_delivered_id_seq, consumers, pending)| {
                        RdbStreamConsumerGroup {
                            name,
                            last_delivered_id_ms,
                            last_delivered_id_seq,
                            entries_read: None,
                            consumers,
                            pending,
                        }
                    },
                )
        }

        fn stream_consumer_strategy() -> impl Strategy<Value = RdbStreamConsumer> {
            (
                non_empty_byte_vec_strategy(8),
                0_u64..=10_000,
                prop::option::of(0_u64..=10_000),
            )
                .prop_map(|(name, seen_time_ms, active_time_ms)| RdbStreamConsumer {
                    name,
                    seen_time_ms,
                    active_time_ms,
                })
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
                        let entries_added = u64::try_from(entries.len()).unwrap_or(u64::MAX);
                        RdbValue::Stream(
                            entries,
                            watermark,
                            groups,
                            None,
                            Some(entries_added),
                            None,
                        )
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
