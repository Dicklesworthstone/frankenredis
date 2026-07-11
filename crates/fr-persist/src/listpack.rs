//! Upstream-compatible listpack decoder.
//!
//! Implements forward iteration over the Redis listpack binary format as
//! documented in `legacy_redis_code/redis/src/listpack.c`. Used by the
//! RDB stream decoder (br-frankenredis-hjub/qi6z) and by the DUMP/RESTORE
//! container-type support (br-frankenredis-hycu) to read listpack blobs
//! embedded inside bigger structures.
//!
//! The stream RDB encoder owns a small write-side subset for stream macro-node
//! listpacks; this module remains the shared read-side parser.
//!
//! (br-frankenredis-3g0p)

use std::error::Error;
use std::fmt;
use std::ops::Range;

/// A decoded listpack entry: integer or byte-string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListpackEntry {
    /// Integer value (any of the LP_ENCODING_*_INT variants).
    Integer(i64),
    /// Byte-string value (any of the LP_ENCODING_*_STR variants).
    String(Vec<u8>),
}

impl ListpackEntry {
    /// Convert the entry to its canonical byte-string form. Integers are
    /// formatted as decimal strings — this matches upstream callers
    /// (listpackGetValue returning an sds) and keeps the downstream
    /// stream-decoder logic simple.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            ListpackEntry::Integer(n) => crate::decimal_i64_bytes(*n),
            ListpackEntry::String(bytes) => bytes.clone(),
        }
    }

    /// Consuming form of [`Self::to_bytes`]. String entries can move their
    /// decoded payload out directly; integer entries still format to their
    /// canonical decimal byte string.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            ListpackEntry::Integer(n) => crate::decimal_i64_bytes(n),
            ListpackEntry::String(bytes) => bytes,
        }
    }
}

/// Redis-observable listpack value without copying string payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListpackValueSpan {
    /// Byte-string entry borrowed from the original listpack payload.
    String(Range<usize>),
    /// Integer entry rendered as Redis's decimal byte-string value.
    Integer(ListpackIntegerBytes),
}

/// Inline decimal representation for any i64 (`i64::MIN` is 20 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListpackIntegerBytes {
    bytes: [u8; 20],
    len: u8,
}

impl ListpackIntegerBytes {
    fn new(value: i64) -> Self {
        // (frankenredis-vqjz1) Render the magnitude with fr-protocol's itoa2
        // (two decimal digits per division via DIGIT_PAIRS) instead of a single-digit
        // `% 10` / `/= 10` loop — halves the divisions per integer entry on the
        // listpack decode path (RESTORE / DEBUG RELOAD). Byte-identical output
        // (i64::MIN magnitude is 19 digits, +sign = 20, fits scratch[20]).
        let (scratch, start) = crate::decimal_i64_scratch(value);
        let len = scratch.len() - start;
        let mut bytes = [0u8; 20];
        bytes[..len].copy_from_slice(&scratch[start..]);
        Self {
            bytes,
            len: len as u8,
        }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl ListpackValueSpan {
    fn integer(value: i64) -> Self {
        Self::Integer(ListpackIntegerBytes::new(value))
    }

    #[must_use]
    pub fn as_bytes<'a>(&'a self, listpack: &'a [u8]) -> &'a [u8] {
        match self {
            Self::String(range) => &listpack[range.clone()],
            Self::Integer(bytes) => bytes.as_slice(),
        }
    }
}

/// Decoder failure modes. Narrow set — callers either succeed or reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListpackError {
    /// Buffer shorter than the 6-byte header.
    ShortHeader,
    /// `total_bytes` in header exceeds the buffer length.
    TotalBytesOutOfRange,
    /// Buffer does not end with the 0xFF terminator at `total_bytes - 1`.
    MissingTerminator,
    /// Unknown encoding byte.
    InvalidEncoding(u8),
    /// `total_bytes` in header is smaller than the supplied buffer.
    TotalBytesMismatch,
    /// Entry body or backlen is truncated.
    TruncatedEntry,
    /// Backlen byte run is malformed or does not match the entry length.
    InvalidBacklen,
    /// String entry's declared length would overflow usize.
    StringLengthOverflow,
    /// Header element count is not the unknown sentinel and does not match the entries scanned.
    ElementCountMismatch,
    /// A sorted-set score entry did not parse as a finite/valid `f64` decimal.
    /// Only produced by [`decode_zset_listpack_pairs`], which folds the score
    /// parse the RDB zset-listpack arm used to do into the structural walk.
    InvalidScore,
}

impl fmt::Display for ListpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortHeader => f.write_str("listpack shorter than 6-byte header"),
            Self::TotalBytesOutOfRange => f.write_str("listpack total-bytes header exceeds buffer"),
            Self::MissingTerminator => f.write_str("listpack missing 0xFF terminator"),
            Self::InvalidEncoding(b) => write!(f, "listpack invalid encoding byte 0x{b:02x}"),
            Self::TotalBytesMismatch => {
                f.write_str("listpack total-bytes header does not match buffer length")
            }
            Self::TruncatedEntry => f.write_str("listpack entry body runs past end"),
            Self::InvalidBacklen => f.write_str("listpack backlen exceeds 5 bytes"),
            Self::StringLengthOverflow => f.write_str("listpack string length overflows usize"),
            Self::ElementCountMismatch => {
                f.write_str("listpack element count header does not match entries")
            }
            Self::InvalidScore => f.write_str("listpack zset score entry is not a valid f64"),
        }
    }
}

impl Error for ListpackError {}

/// Fixed listpack header size (4-byte total_bytes + 2-byte num_elements).
pub const LISTPACK_HEADER_SIZE: usize = 6;

/// Sentinel returned in the `num_elements` field when the real count
/// exceeds `u16::MAX`.
pub const LISTPACK_HDR_NUMELE_UNKNOWN: u16 = u16::MAX;

/// Listpack end-of-stream marker byte.
pub const LISTPACK_EOF: u8 = 0xFF;

/// Parse the listpack header returning (total_bytes, num_elements).
/// `num_elements == LISTPACK_HDR_NUMELE_UNKNOWN` means the decoder must
/// stop on the 0xFF terminator rather than trusting the count.
pub fn parse_header(data: &[u8]) -> Result<(u32, u16), ListpackError> {
    if data.len() < LISTPACK_HEADER_SIZE {
        return Err(ListpackError::ShortHeader);
    }
    let total_bytes = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let num_elements = u16::from_le_bytes([data[4], data[5]]);
    let total_len = total_bytes as usize;
    if total_len > data.len() {
        return Err(ListpackError::TotalBytesOutOfRange);
    }
    if total_len != data.len() {
        return Err(ListpackError::TotalBytesMismatch);
    }
    if data[total_len - 1] != LISTPACK_EOF {
        return Err(ListpackError::MissingTerminator);
    }
    Ok((total_bytes, num_elements))
}

/// Decode a single entry at `cursor`. Returns the decoded entry and the
/// total number of bytes the entry occupies (encoding + data + backlen).
/// A decoded listpack entry that has NOT yet materialized its string payload:
/// integers carry their `i64`, strings carry the byte `Range` into the source
/// listpack. This is the shared, allocation-free core of [`decode_entry`]; a
/// consumer that only needs to *read* a string entry (e.g. parse a zset score to
/// `f64`) can borrow the slice instead of forcing the `to_vec()` copy that
/// materializing a [`ListpackEntry::String`] would pay. (frankenredis zsetlpscore)
enum RawListpackValue {
    Integer(i64),
    String(Range<usize>),
}

/// Allocation-free entry decode: the exact byte-dispatch of [`decode_entry`] but
/// string entries return their `Range` rather than a copied `Vec`. Returns the
/// raw value and the total bytes the entry occupies (encoding + data + backlen).
/// [`decode_entry`] is a thin materializing wrapper over this, so both share one
/// parser and cannot drift.
fn decode_entry_raw(
    data: &[u8],
    cursor: usize,
) -> Result<(RawListpackValue, usize), ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    // 7-bit uint: 0xxxxxxx
    if first & 0x80 == 0 {
        let value = i64::from(first & 0x7F);
        let data_len = 1;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::Integer(value), entry_len));
    }
    // 6-bit str: 10xxxxxx, length in low 6 bits, string follows.
    if first & 0xC0 == 0x80 {
        let slen = (first & 0x3F) as usize;
        let start = cursor + 1;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 1 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::String(start..end), entry_len));
    }
    // 13-bit signed int: 110xxxxx + 1 byte.
    if first & 0xE0 == 0xC0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        let raw = (u16::from(first & 0x1F) << 8) | u16::from(second);
        // Sign-extend from 13 bits.
        let signed = if raw & 0x1000 != 0 {
            (raw as i64) - 0x2000
        } else {
            raw as i64
        };
        let data_len = 2;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::Integer(signed), entry_len));
    }
    // 12-bit str: 1110xxxx + 1 byte = length, then string.
    if first & 0xF0 == 0xE0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        let slen = ((u32::from(first & 0x0F) << 8) | u32::from(second)) as usize;
        let start = cursor + 2;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 2 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::String(start..end), entry_len));
    }
    // Remaining: 0xF0..=0xF4 / 0xFF.
    match first {
        0xF0 => {
            // 32-bit str: 11110000 + u32 LE length + string.
            if cursor + 5 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let slen = u32::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
            ]) as usize;
            let start = cursor + 5;
            let end = start
                .checked_add(slen)
                .ok_or(ListpackError::StringLengthOverflow)?;
            if end > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let data_len = 5 + slen;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::String(start..end), entry_len))
        }
        0xF1 => {
            // 16-bit signed int: 11110001 + u16 LE.
            if cursor + 3 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
            let data_len = 3;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::Integer(i64::from(raw)), entry_len))
        }
        0xF2 => {
            // 24-bit signed int: 11110010 + 3 bytes LE.
            if cursor + 4 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let bytes = [data[cursor + 1], data[cursor + 2], data[cursor + 3], 0];
            let raw_u32 = u32::from_le_bytes(bytes);
            // Sign-extend from 24 bits.
            let signed = if raw_u32 & 0x00_80_00_00 != 0 {
                (raw_u32 as i64) - 0x0100_0000
            } else {
                raw_u32 as i64
            };
            let data_len = 4;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::Integer(signed), entry_len))
        }
        0xF3 => {
            // 32-bit signed int: 11110011 + i32 LE.
            if cursor + 5 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i32::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
            ]);
            let data_len = 5;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::Integer(i64::from(raw)), entry_len))
        }
        0xF4 => {
            // 64-bit signed int: 11110100 + i64 LE.
            if cursor + 9 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i64::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
                data[cursor + 5],
                data[cursor + 6],
                data[cursor + 7],
                data[cursor + 8],
            ]);
            let data_len = 9;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::Integer(raw), entry_len))
        }
        _ => Err(ListpackError::InvalidEncoding(first)),
    }
}

fn decode_entry(data: &[u8], cursor: usize) -> Result<(ListpackEntry, usize), ListpackError> {
    let (raw, entry_len) = decode_entry_raw(data, cursor)?;
    let entry = match raw {
        RawListpackValue::Integer(value) => ListpackEntry::Integer(value),
        // Materialize the borrowed range into the owned payload — the single
        // `to_vec()` the pre-refactor `decode_entry` performed inline.
        RawListpackValue::String(range) => ListpackEntry::String(data[range].to_vec()),
    };
    Ok((entry, entry_len))
}

fn entry_len_with_backlen(
    data: &[u8],
    cursor: usize,
    data_len: usize,
) -> Result<usize, ListpackError> {
    let backlen_len = backlen_byte_count(data_len);
    let backlen_start = cursor
        .checked_add(data_len)
        .ok_or(ListpackError::TruncatedEntry)?;
    let backlen_end = backlen_start
        .checked_add(backlen_len)
        .ok_or(ListpackError::TruncatedEntry)?;
    if backlen_end > data.len() {
        return Err(ListpackError::TruncatedEntry);
    }

    // (cc_fr) Fast path for the single-byte backlen — `data_len <= 127`, i.e. EVERY
    // integer entry and every string <= ~126 bytes, the overwhelming majority of
    // listpack entries (hash fields, set/zset members, small list items). Upstream's
    // forward decode never re-decodes the backlen (it derives the byte count from
    // `data_len` via `lpEncodeBacklen` and skips); this keeps fr's per-entry backlen
    // VALIDATION but collapses the general reverse-7-bit varint loop to one compare.
    // Byte-identical: for `backlen_len == 1` the loop's `terminated && decoded ==
    // data_len` gate is exactly `byte & 0x80 == 0 && byte & 0x7F == data_len`, and
    // since `data_len <= 127` the high bit is clear, so that is `byte == data_len as
    // u8`. Same `InvalidBacklen` on mismatch; multi-byte backlens keep the loop.
    if backlen_len == 1 {
        if data[backlen_start] != data_len as u8 {
            return Err(ListpackError::InvalidBacklen);
        }
        return Ok(data_len + 1);
    }
    validate_multibyte_backlen(data, backlen_start, backlen_end, data_len)?;
    Ok(data_len + backlen_len)
}

/// Decode+validate a multi-byte listpack backlen (the little-endian 7-bit varint,
/// read in reverse) and confirm it re-encodes exactly `data_len`. Shared by the
/// production decoder (multi-byte arm) and the bench-only original walker.
fn validate_multibyte_backlen(
    data: &[u8],
    backlen_start: usize,
    backlen_end: usize,
    data_len: usize,
) -> Result<(), ListpackError> {
    let mut decoded = 0usize;
    let mut shift = 0u32;
    let mut terminated = false;
    for index in (backlen_start..backlen_end).rev() {
        let byte = data[index];
        let chunk = usize::from(byte & 0x7F)
            .checked_shl(shift)
            .ok_or(ListpackError::InvalidBacklen)?;
        decoded = decoded
            .checked_add(chunk)
            .ok_or(ListpackError::InvalidBacklen)?;
        if byte & 0x80 == 0 {
            if index != backlen_start {
                return Err(ListpackError::InvalidBacklen);
            }
            terminated = true;
            break;
        }
        shift += 7;
    }

    if !terminated || decoded != data_len {
        return Err(ListpackError::InvalidBacklen);
    }
    Ok(())
}

/// Bench-only baseline: the pre-fast-path `entry_len_with_backlen`, always running
/// the reverse-7-bit backlen decode loop (no single-byte shortcut). Byte-identical
/// result to `entry_len_with_backlen`; exists only so a same-binary A/B can isolate
/// the fast path. Not on any production path.
#[doc(hidden)]
pub fn entry_len_with_backlen_orig(
    data: &[u8],
    cursor: usize,
    data_len: usize,
) -> Result<usize, ListpackError> {
    let backlen_len = backlen_byte_count(data_len);
    let backlen_start = cursor
        .checked_add(data_len)
        .ok_or(ListpackError::TruncatedEntry)?;
    let backlen_end = backlen_start
        .checked_add(backlen_len)
        .ok_or(ListpackError::TruncatedEntry)?;
    if backlen_end > data.len() {
        return Err(ListpackError::TruncatedEntry);
    }
    validate_multibyte_backlen(data, backlen_start, backlen_end, data_len)?;
    Ok(data_len + backlen_len)
}

/// The encoding+payload byte count of the entry at `cursor` (no backlen, no value
/// materialization) — mirrors `decode_entry`'s `data_len` for each encoding. Used
/// by the bench walker to feed both backlen decoders identical `data_len` inputs.
#[doc(hidden)]
pub fn entry_data_len(data: &[u8], cursor: usize) -> Result<usize, ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;
    let data_len = if first & 0x80 == 0 {
        1
    } else if first & 0xC0 == 0x80 {
        1 + (first & 0x3F) as usize
    } else if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        2 + (((u32::from(first & 0x0F) << 8) | u32::from(second)) as usize)
    } else {
        match first {
            0xF0 => {
                if cursor + 5 > data.len() {
                    return Err(ListpackError::TruncatedEntry);
                }
                let slen = u32::from_le_bytes([
                    data[cursor + 1],
                    data[cursor + 2],
                    data[cursor + 3],
                    data[cursor + 4],
                ]) as usize;
                5 + slen
            }
            0xF1 => 3,
            0xF2 => 4,
            0xF3 => 5,
            0xF4 => 9,
            _ => return Err(ListpackError::InvalidEncoding(first)),
        }
    };
    Ok(data_len)
}

/// Bench-only: walk every entry of `data`, summing `entry_len_with_backlen`
/// (`orig=false`) vs `entry_len_with_backlen_orig` (`orig=true`). `entry_data_len`
/// (identical for both arms) supplies `data_len`, so the timing difference isolates
/// the backlen fast path. Returns the summed entry lengths (a `black_box` sink).
#[doc(hidden)]
pub fn bench_backlen_walk(data: &[u8], orig: bool) -> Result<usize, ListpackError> {
    let (total_bytes, _) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut sum = 0usize;
    while cursor < end {
        let data_len = entry_data_len(data, cursor)?;
        let consumed = if orig {
            entry_len_with_backlen_orig(data, cursor, data_len)?
        } else {
            entry_len_with_backlen(data, cursor, data_len)?
        };
        sum = sum.wrapping_add(consumed);
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
    }
    Ok(sum)
}

/// How many backlen bytes follow an entry whose encoding+data occupies
/// `data_len` bytes. Mirrors upstream `lpEncodeBacklen` branch table.
fn backlen_byte_count(data_len: usize) -> usize {
    match data_len {
        0..=127 => 1,
        128..=16_382 => 2,
        16_383..=2_097_150 => 3,
        2_097_151..=268_435_454 => 4,
        _ => 5,
    }
}

/// Forward-iterate a complete listpack blob and collect every entry.
///
/// Returns an error if the header or any entry is malformed. Succeeds
/// even when the header's num_elements is the LISTPACK_HDR_NUMELE_UNKNOWN
/// sentinel — the 0xFF terminator is authoritative.
pub fn decode_listpack(data: &[u8]) -> Result<Vec<ListpackEntry>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1; // terminator is at total_bytes - 1
    let mut cursor = LISTPACK_HEADER_SIZE;
    // The header's element count is exact whenever it isn't the UNKNOWN sentinel
    // (i.e. <= u16::MAX-1 elements — the overwhelmingly common compact case for
    // hash/set/zset/quicklist-node listpacks). Pre-size the result so the entries
    // are collected in one allocation instead of growing from empty
    // (~log2(n) realloc+copies per decoded listpack on the bulk RDB-load path).
    // The sentinel case (count > 65534) keeps the default and just grows.
    // Capacity never affects content => decoded entries are byte-identical.
    let mut entries = if num_elements == LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::new()
    } else {
        Vec::with_capacity(usize::from(num_elements))
    };
    while cursor < end {
        let (entry, consumed) = decode_entry(data, cursor)?;
        entries.push(entry);
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && entries.len() != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(entries)
}

/// Decode a `RDB_TYPE_ZSET_LISTPACK` payload (`m1, score1, m2, score2, …`)
/// straight into owned `(member, score)` pairs.
///
/// The win over `decode_listpack(..).into_iter()` + a pair loop is on the
/// **score** entries: upstream stores non-integer scores (`1.5`, `inf`, …) as
/// listpack STRING entries, so the old path let `decode_listpack` heap-allocate a
/// `Vec<u8>` for every such score only to `from_utf8` + `parse::<f64>` it and drop
/// the `Vec`. Here each score is read through the allocation-free
/// [`decode_entry_raw`] core — integer scores stay `n as f64` (CrimsonHawk's
/// shortcut, `788bbfd00`), string scores parse a borrowed slice — so no score
/// `Vec` is ever allocated. Members still materialize their owned bytes (the RESTORE
/// result outlives the transient decompressed listpack, so that copy is forced).
///
/// Byte-/bit-identical to the old path: same member bytes, and each score is the
/// same `n as f64` / `parse(same bytes)` `f64`. Structural validation mirrors
/// [`decode_listpack`] exactly (same per-entry checks, terminator, and element
/// count), and an odd element count is rejected just as the old
/// `decoded.len().is_multiple_of(2)` guard did. (frankenredis zsetlpscore)
pub fn decode_zset_listpack_pairs(data: &[u8]) -> Result<Vec<(Vec<u8>, f64)>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1; // terminator is at total_bytes - 1
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut pairs = if num_elements == LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::new()
    } else {
        Vec::with_capacity(usize::from(num_elements) / 2)
    };
    let mut entry_count = 0usize;
    let mut pending_member: Option<Vec<u8>> = None;
    while cursor < end {
        let (raw, consumed) = decode_entry_raw(data, cursor)?;
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
        match pending_member.take() {
            None => {
                // Member position: materialize the owned payload (integers render
                // to canonical decimal, matching `ListpackEntry::into_bytes`).
                pending_member = Some(match raw {
                    RawListpackValue::Integer(n) => crate::decimal_i64_bytes(n),
                    RawListpackValue::String(range) => data[range].to_vec(),
                });
            }
            Some(member) => {
                // Score position: read the f64 WITHOUT allocating the score string.
                let score = match raw {
                    RawListpackValue::Integer(n) => n as f64,
                    RawListpackValue::String(range) => std::str::from_utf8(&data[range])
                        .ok()
                        .and_then(|s| s.parse::<f64>().ok())
                        .ok_or(ListpackError::InvalidScore)?,
                };
                pairs.push((member, score));
            }
        }
        entry_count += 1;
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && entry_count != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    // A trailing member with no score = odd element count (the old path's
    // `is_multiple_of(2)` guard rejected this).
    if pending_member.is_some() {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(pairs)
}

/// Bench/test-only reference: the pre-change zset-listpack decode that
/// [`decode_zset_listpack_pairs`] replaces — `decode_listpack` (which allocates a
/// `Vec<u8>` per string entry, scores included) + a pair loop that parses then
/// drops each string score's `Vec`. Kept in-crate (like `entry_len_with_backlen_orig`)
/// so the same-binary A/B measures exactly what shipped. Result is identical to the
/// production path.
pub fn decode_zset_listpack_pairs_orig(
    data: &[u8],
) -> Result<Vec<(Vec<u8>, f64)>, ListpackError> {
    let decoded = decode_listpack(data)?;
    if !decoded.len().is_multiple_of(2) {
        return Err(ListpackError::ElementCountMismatch);
    }
    let mut members = Vec::with_capacity(decoded.len() / 2);
    let mut it = decoded.into_iter();
    while let Some(member) = it.next() {
        let score = match it.next().ok_or(ListpackError::ElementCountMismatch)? {
            ListpackEntry::Integer(n) => n as f64,
            ListpackEntry::String(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .ok_or(ListpackError::InvalidScore)?,
        };
        members.push((member.into_bytes(), score));
    }
    Ok(members)
}

fn decode_string_entry_range(
    data: &[u8],
    cursor: usize,
) -> Result<Option<(Range<usize>, usize)>, ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    if first & 0x80 == 0 {
        return Ok(None);
    }
    if first & 0xC0 == 0x80 {
        let slen = (first & 0x3F) as usize;
        let start = cursor + 1;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 1 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok(Some((start..end, entry_len)));
    }
    if first & 0xE0 == 0xC0 {
        return Ok(None);
    }
    if first & 0xF0 == 0xE0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        let slen = ((u32::from(first & 0x0F) << 8) | u32::from(second)) as usize;
        let start = cursor + 2;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 2 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok(Some((start..end, entry_len)));
    }
    match first {
        0xF0 => {
            if cursor + 5 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let slen = u32::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
            ]) as usize;
            let start = cursor + 5;
            let end = start
                .checked_add(slen)
                .ok_or(ListpackError::StringLengthOverflow)?;
            if end > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let data_len = 5 + slen;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok(Some((start..end, entry_len)))
        }
        0xF1..=0xF4 => Ok(None),
        _ => Err(ListpackError::InvalidEncoding(first)),
    }
}

fn decode_entry_value_span(
    data: &[u8],
    cursor: usize,
) -> Result<(ListpackValueSpan, usize), ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    if first & 0x80 == 0 {
        let value = i64::from(first & 0x7F);
        let data_len = 1;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((ListpackValueSpan::integer(value), entry_len));
    }
    if first & 0xC0 == 0x80 {
        let slen = (first & 0x3F) as usize;
        let start = cursor + 1;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 1 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((ListpackValueSpan::String(start..end), entry_len));
    }
    if first & 0xE0 == 0xC0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        let raw = (u16::from(first & 0x1F) << 8) | u16::from(second);
        let signed = if raw & 0x1000 != 0 {
            (raw as i64) - 0x2000
        } else {
            raw as i64
        };
        let data_len = 2;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((ListpackValueSpan::integer(signed), entry_len));
    }
    if first & 0xF0 == 0xE0 {
        let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
        let slen = ((u32::from(first & 0x0F) << 8) | u32::from(second)) as usize;
        let start = cursor + 2;
        let end = start
            .checked_add(slen)
            .ok_or(ListpackError::StringLengthOverflow)?;
        if end > data.len() {
            return Err(ListpackError::TruncatedEntry);
        }
        let data_len = 2 + slen;
        let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
        return Ok((ListpackValueSpan::String(start..end), entry_len));
    }
    match first {
        0xF0 => {
            if cursor + 5 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let slen = u32::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
            ]) as usize;
            let start = cursor + 5;
            let end = start
                .checked_add(slen)
                .ok_or(ListpackError::StringLengthOverflow)?;
            if end > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let data_len = 5 + slen;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((ListpackValueSpan::String(start..end), entry_len))
        }
        0xF1 => {
            if cursor + 3 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
            let data_len = 3;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((ListpackValueSpan::integer(i64::from(raw)), entry_len))
        }
        0xF2 => {
            if cursor + 4 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let bytes = [data[cursor + 1], data[cursor + 2], data[cursor + 3], 0];
            let raw_u32 = u32::from_le_bytes(bytes);
            let signed = if raw_u32 & 0x00_80_00_00 != 0 {
                (raw_u32 as i64) - 0x0100_0000
            } else {
                raw_u32 as i64
            };
            let data_len = 4;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((ListpackValueSpan::integer(signed), entry_len))
        }
        0xF3 => {
            if cursor + 5 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i32::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
            ]);
            let data_len = 5;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((ListpackValueSpan::integer(i64::from(raw)), entry_len))
        }
        0xF4 => {
            if cursor + 9 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i64::from_le_bytes([
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
                data[cursor + 4],
                data[cursor + 5],
                data[cursor + 6],
                data[cursor + 7],
                data[cursor + 8],
            ]);
            let data_len = 9;
            let entry_len = entry_len_with_backlen(data, cursor, data_len)?;
            Ok((ListpackValueSpan::integer(raw), entry_len))
        }
        _ => Err(ListpackError::InvalidEncoding(first)),
    }
}

/// Return byte ranges for a listpack whose entries are all string encodings.
///
/// Integer encodings are not lossy, but their Redis-observable value is the
/// decimal string form of the integer. Callers that need borrowed payload bytes
/// should fall back to [`decode_listpack`] when this returns `Ok(None)`.
pub fn decode_string_ranges_if_all_strings(
    data: &[u8],
) -> Result<Option<Vec<Range<usize>>>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut ranges = Vec::new();
    while cursor < end {
        let Some((range, consumed)) = decode_string_entry_range(data, cursor)? else {
            return Ok(None);
        };
        ranges.push(range);
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && ranges.len() != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(Some(ranges))
}

/// Return Redis-observable values while retaining string payload ranges.
///
/// String entries borrow from `data`; integer entries store their canonical
/// decimal byte-string form inline. This lets callers retain a listpack node
/// without allocating one `Vec<u8>` per element while preserving normal list
/// iteration semantics.
pub fn decode_value_spans(data: &[u8]) -> Result<Vec<ListpackValueSpan>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut values = Vec::new();
    while cursor < end {
        let (value, consumed) = decode_entry_value_span(data, cursor)?;
        values.push(value);
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && values.len() != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(values)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // (frankenredis-vqjz1) Lock the itoa2 magnitude rendering against the original
    // single-digit div-by-10 reference across digit boundaries + i64 extremes.
    #[test]
    fn listpack_integer_bytes_matches_single_digit_reference() {
        fn reference(value: i64) -> Vec<u8> {
            let mut scratch = [0u8; 20];
            let mut magnitude = value.unsigned_abs();
            let mut start = scratch.len();
            if magnitude == 0 {
                start -= 1;
                scratch[start] = b'0';
            } else {
                while magnitude != 0 {
                    start -= 1;
                    scratch[start] = b'0' + (magnitude % 10) as u8;
                    magnitude /= 10;
                }
            }
            if value < 0 {
                start -= 1;
                scratch[start] = b'-';
            }
            scratch[start..].to_vec()
        }
        let mut probes: Vec<i64> = vec![
            0,
            1,
            -1,
            9,
            -9,
            10,
            -10,
            99,
            -99,
            100,
            -100,
            i64::MAX,
            i64::MIN,
            i64::MAX - 1,
            i64::MIN + 1,
        ];
        let mut p: i64 = 1;
        while let Some(next) = p.checked_mul(10) {
            probes.push(p);
            probes.push(-p);
            p = next;
        }
        for &v in &probes {
            assert_eq!(
                ListpackIntegerBytes::new(v).as_slice(),
                reference(v).as_slice(),
                "decimal rendering of {v}"
            );
        }
    }

    /// Builds a minimal listpack byte sequence from a set of pre-encoded
    /// entry byte strings (each including encoding + data + backlen).
    fn assemble(entries: &[&[u8]]) -> Vec<u8> {
        let total_entries_bytes: usize = entries.iter().map(|e| e.len()).sum();
        let total_bytes = (LISTPACK_HEADER_SIZE + total_entries_bytes + 1) as u32;
        let num_elements = entries.len().min(u16::MAX as usize) as u16;
        let mut out = Vec::with_capacity(total_bytes as usize);
        out.extend_from_slice(&total_bytes.to_le_bytes());
        out.extend_from_slice(&num_elements.to_le_bytes());
        for e in entries {
            out.extend_from_slice(e);
        }
        out.push(LISTPACK_EOF);
        out
    }

    /// Build a 7-bit uint entry (encoding byte is the value itself) +
    /// 1-byte backlen.
    fn entry_7bit_uint(v: u8) -> Vec<u8> {
        assert!(v <= 0x7F);
        vec![v, 1]
    }

    /// Build a 6-bit str entry.
    fn entry_6bit_str(s: &[u8]) -> Vec<u8> {
        assert!(s.len() <= 63);
        let data_len = 1 + s.len();
        let backlen_len = backlen_byte_count(data_len);
        let mut out = Vec::with_capacity(data_len + backlen_len);
        out.push(0x80 | (s.len() as u8));
        out.extend_from_slice(s);
        // backlen: for data_len <= 127, one byte == data_len.
        assert!(data_len <= 127);
        out.push(data_len as u8);
        out
    }

    /// Build a 32-bit signed int entry.
    fn entry_32bit_int(v: i32) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.push(0xF3);
        out.extend_from_slice(&v.to_le_bytes());
        // 5-byte data → 1-byte backlen.
        out.push(5);
        out
    }

    /// Build a 13-bit signed int entry.
    fn entry_13bit_int(v: i16) -> Vec<u8> {
        assert!((-4096..=4095).contains(&v));
        let raw: u16 = if v < 0 {
            (v as i32 + 0x2000) as u16
        } else {
            v as u16
        };
        let first = 0xC0u8 | ((raw >> 8) as u8 & 0x1F);
        let second = (raw & 0xFF) as u8;
        vec![first, second, 2]
    }

    #[test]
    fn parse_header_reads_total_bytes_and_num_elements() {
        let lp = assemble(&[&entry_7bit_uint(3), &entry_7bit_uint(5)]);
        let (total, n) = parse_header(&lp).unwrap();
        assert_eq!(total, lp.len() as u32);
        assert_eq!(n, 2);
    }

    #[test]
    fn backlen_fast_path_matches_loop_for_every_data_len() {
        // (cc_fr) The single-byte-backlen fast path in `entry_len_with_backlen` MUST be
        // byte-identical to the original reverse-7-bit loop for every `data_len` — a
        // divergence would change RESTORE's accept/reject on corrupt listpacks. Cover the
        // 1-byte range, the 127/128 boundary where `backlen_len` flips 1→2, and the 2-byte
        // range. For each `data_len`, synthesize a well-formed entry (payload bytes + the
        // canonical backlen) and assert both decoders agree; then corrupt the terminating
        // backlen byte and assert both still agree (both reject).
        fn encode_backlen(data_len: usize) -> Vec<u8> {
            // Mirror upstream lpEncodeBacklen (only the widths this test exercises).
            if data_len <= 127 {
                vec![data_len as u8]
            } else {
                // 2-byte: buf[0] = l>>7, buf[1] = (l&127)|128; decoder reads in reverse.
                vec![(data_len >> 7) as u8, ((data_len & 127) | 128) as u8]
            }
        }
        for data_len in [1usize, 2, 5, 63, 64, 126, 127, 128, 129, 200, 500, 1000] {
            let mut buf = vec![0xEEu8; data_len]; // opaque payload; backlen fn ignores it
            buf.extend_from_slice(&encode_backlen(data_len));
            assert_eq!(
                entry_len_with_backlen(&buf, 0, data_len),
                entry_len_with_backlen_orig(&buf, 0, data_len),
                "well-formed data_len={data_len}"
            );

            // Corrupt the terminating (lowest-address) backlen byte so it no longer
            // encodes data_len; both paths must reject identically.
            let mut bad = buf.clone();
            let backlen_start = data_len;
            bad[backlen_start] ^= 0x01;
            assert_eq!(
                entry_len_with_backlen(&bad, 0, data_len),
                entry_len_with_backlen_orig(&bad, 0, data_len),
                "corrupt data_len={data_len}"
            );
        }
    }

    #[test]
    fn bench_backlen_walk_orig_and_new_agree_on_real_listpack() {
        // The bench's two arms must sum to the identical total on a mixed listpack
        // (short strings = 1-byte backlen, a 200-byte string = 2-byte backlen).
        let long = vec![b'x'; 200];
        let mut long_entry = Vec::new();
        // 12-bit str: 1110xxxx + 1 byte len, then payload; data_len = 2 + 200 = 202.
        long_entry.push(0xE0 | ((200u16 >> 8) as u8 & 0x0F));
        long_entry.push((200u16 & 0xFF) as u8);
        long_entry.extend_from_slice(&long);
        long_entry.push((202usize >> 7) as u8);
        long_entry.push(((202usize & 127) | 128) as u8);
        let lp = assemble(&[
            &entry_7bit_uint(7),
            &entry_6bit_str(b"hello"),
            &long_entry,
            &entry_32bit_int(-12345),
        ]);
        assert_eq!(
            bench_backlen_walk(&lp, true).unwrap(),
            bench_backlen_walk(&lp, false).unwrap()
        );
        // And the production decoder still round-trips the same listpack.
        assert_eq!(decode_listpack(&lp).unwrap().len(), 4);
    }

    #[test]
    fn empty_listpack_decodes_to_no_entries() {
        let lp = assemble(&[]);
        assert_eq!(decode_listpack(&lp).unwrap(), Vec::<ListpackEntry>::new());
    }

    #[test]
    fn decode_7bit_uint_entries() {
        let lp = assemble(&[
            &entry_7bit_uint(0),
            &entry_7bit_uint(42),
            &entry_7bit_uint(127),
        ]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(
            out,
            vec![
                ListpackEntry::Integer(0),
                ListpackEntry::Integer(42),
                ListpackEntry::Integer(127),
            ]
        );
    }

    #[test]
    fn decode_6bit_strings() {
        let lp = assemble(&[&entry_6bit_str(b"hello"), &entry_6bit_str(b"")]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(
            out,
            vec![
                ListpackEntry::String(b"hello".to_vec()),
                ListpackEntry::String(b"".to_vec()),
            ]
        );
    }

    #[test]
    fn decode_32bit_int_entries_signed() {
        let lp = assemble(&[&entry_32bit_int(100_000), &entry_32bit_int(-100_000)]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(
            out,
            vec![
                ListpackEntry::Integer(100_000),
                ListpackEntry::Integer(-100_000),
            ]
        );
    }

    #[test]
    fn decode_13bit_int_positive_and_negative() {
        let lp = assemble(&[
            &entry_13bit_int(4095),
            &entry_13bit_int(-4096),
            &entry_13bit_int(0),
        ]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(
            out,
            vec![
                ListpackEntry::Integer(4095),
                ListpackEntry::Integer(-4096),
                ListpackEntry::Integer(0),
            ]
        );
    }

    #[test]
    fn decode_12bit_and_32bit_str() {
        // 12-bit str encoding: 1110xxxx + byte length. Build a 100-byte
        // string (fits in 12 bits) and a 70_000-byte string (requires
        // 32-bit encoding).
        let s100 = vec![b'a'; 100];
        let mut e100 = Vec::new();
        e100.push(0xE0u8 | ((100u16 >> 8) as u8 & 0x0F));
        e100.push(100u8);
        e100.extend_from_slice(&s100);
        let data_len = 2 + 100;
        let backlen = backlen_byte_count(data_len);
        // data_len = 102 ≤ 127 → 1-byte backlen.
        assert_eq!(backlen, 1);
        e100.push(data_len as u8);

        let s70k = vec![b'b'; 70_000];
        let mut e70k = Vec::new();
        e70k.push(0xF0u8);
        e70k.extend_from_slice(&(70_000u32).to_le_bytes());
        e70k.extend_from_slice(&s70k);
        let data_len_big = 5 + 70_000;
        let backlen_big = backlen_byte_count(data_len_big);
        // data_len ~ 70_005 ≥ 16_383 → 3-byte backlen.
        assert_eq!(backlen_big, 3);
        // Encode 70_005 as 3-byte backlen per upstream lpEncodeBacklen.
        e70k.push((data_len_big >> 14) as u8);
        e70k.push(((data_len_big >> 7) as u8 & 0x7F) | 0x80);
        e70k.push((data_len_big as u8 & 0x7F) | 0x80);

        let lp = assemble(&[&e100, &e70k]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(out[0], ListpackEntry::String(s100));
        assert_eq!(out[1], ListpackEntry::String(s70k));
    }

    #[test]
    fn decode_16_24_64_bit_ints() {
        // 16-bit: 0xF1 + i16 LE + 1-byte backlen (data_len=3).
        let mut e16 = Vec::from([0xF1u8]);
        e16.extend_from_slice(&(12345_i16).to_le_bytes());
        e16.push(3);
        let mut e16n = Vec::from([0xF1u8]);
        e16n.extend_from_slice(&((-32_000_i16).to_le_bytes()));
        e16n.push(3);
        // 24-bit: 0xF2 + 3 bytes LE + 1-byte backlen (data_len=4).
        let mut e24 = Vec::from([0xF2u8]);
        let v24 = -1_000_000_i32;
        let bytes24 = v24.to_le_bytes();
        e24.extend_from_slice(&bytes24[0..3]);
        e24.push(4);
        // 64-bit: 0xF4 + i64 LE + 1-byte backlen (data_len=9).
        let mut e64 = Vec::from([0xF4u8]);
        e64.extend_from_slice(&(i64::MIN.to_le_bytes()));
        e64.push(9);

        let lp = assemble(&[&e16, &e16n, &e24, &e64]);
        let out = decode_listpack(&lp).unwrap();
        assert_eq!(
            out,
            vec![
                ListpackEntry::Integer(12_345),
                ListpackEntry::Integer(-32_000),
                ListpackEntry::Integer(-1_000_000),
                ListpackEntry::Integer(i64::MIN),
            ]
        );
    }

    #[test]
    fn invalid_terminator_rejected() {
        let mut lp = assemble(&[&entry_7bit_uint(3)]);
        *lp.last_mut().unwrap() = 0xAB;
        assert_eq!(decode_listpack(&lp), Err(ListpackError::MissingTerminator));
    }

    #[test]
    fn mismatched_backlen_rejected() {
        let mut lp = assemble(&[&entry_6bit_str(b"hello")]);
        let backlen_idx = lp.len() - 2;
        assert_eq!(lp[backlen_idx], 6);
        lp[backlen_idx] = 1;
        assert_eq!(decode_listpack(&lp), Err(ListpackError::InvalidBacklen));
    }

    #[test]
    fn short_header_rejected() {
        let lp = vec![0, 0, 0]; // < 6 bytes
        assert_eq!(decode_listpack(&lp), Err(ListpackError::ShortHeader));
    }

    #[test]
    fn total_bytes_exceeding_buffer_rejected() {
        let mut lp = assemble(&[&entry_7bit_uint(3)]);
        // Overwrite total_bytes with a wildly-high value.
        lp[0..4].copy_from_slice(&(1_000_000u32).to_le_bytes());
        assert_eq!(
            decode_listpack(&lp),
            Err(ListpackError::TotalBytesOutOfRange)
        );
    }

    #[test]
    fn total_bytes_smaller_than_buffer_rejected() {
        let mut lp = assemble(&[&entry_7bit_uint(3)]);
        lp.push(0);
        assert_eq!(decode_listpack(&lp), Err(ListpackError::TotalBytesMismatch));
    }

    #[test]
    fn element_count_mismatch_rejected_unless_unknown_sentinel() {
        let mut lp = assemble(&[&entry_7bit_uint(3), &entry_7bit_uint(5)]);
        lp[4..6].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            decode_listpack(&lp),
            Err(ListpackError::ElementCountMismatch)
        );

        lp[4..6].copy_from_slice(&LISTPACK_HDR_NUMELE_UNKNOWN.to_le_bytes());
        assert_eq!(
            decode_listpack(&lp).unwrap(),
            vec![ListpackEntry::Integer(3), ListpackEntry::Integer(5)]
        );
    }

    #[test]
    fn decode_string_ranges_borrows_string_payloads() {
        let first = entry_6bit_str(b"alpha");
        let second = entry_6bit_str(b"beta");
        let lp = assemble(&[&first, &second]);
        let ranges = decode_string_ranges_if_all_strings(&lp)
            .unwrap()
            .expect("all entries are strings");
        let borrowed: Vec<&[u8]> = ranges.iter().map(|range| &lp[range.clone()]).collect();
        assert_eq!(borrowed, vec![b"alpha".as_slice(), b"beta".as_slice()]);
    }

    #[test]
    fn decode_string_ranges_returns_none_for_integer_node() {
        let lp = assemble(&[&entry_6bit_str(b"alpha"), &entry_7bit_uint(42)]);
        assert_eq!(decode_string_ranges_if_all_strings(&lp).unwrap(), None);
    }

    #[test]
    fn decode_value_spans_borrows_strings_and_formats_ints() {
        let lp = assemble(&[
            &entry_6bit_str(b"alpha"),
            &entry_7bit_uint(42),
            &entry_13bit_int(-17),
            &entry_32bit_int(100_000),
            &entry_6bit_str(b"omega"),
        ]);
        let spans = decode_value_spans(&lp).unwrap();
        let values: Vec<&[u8]> = spans.iter().map(|span| span.as_bytes(&lp)).collect();
        assert_eq!(
            values,
            vec![b"alpha".as_slice(), b"42", b"-17", b"100000", b"omega",]
        );
        assert!(matches!(spans[0], ListpackValueSpan::String(_)));
        assert!(matches!(spans[1], ListpackValueSpan::Integer(_)));
    }

    #[test]
    fn to_bytes_converts_int_to_decimal_string() {
        assert_eq!(ListpackEntry::Integer(42).to_bytes(), b"42".to_vec());
        assert_eq!(ListpackEntry::Integer(-1).to_bytes(), b"-1".to_vec());
        assert_eq!(
            ListpackEntry::Integer(i64::MIN).to_bytes(),
            b"-9223372036854775808".to_vec()
        );
        assert_eq!(
            ListpackEntry::Integer(i64::MAX).to_bytes(),
            b"9223372036854775807".to_vec()
        );
        assert_eq!(
            ListpackEntry::String(b"hello".to_vec()).to_bytes(),
            b"hello".to_vec()
        );
    }

    #[test]
    fn into_bytes_moves_string_payload_and_formats_ints() {
        assert_eq!(ListpackEntry::Integer(42).into_bytes(), b"42".to_vec());
        assert_eq!(
            ListpackEntry::Integer(i64::MIN).into_bytes(),
            b"-9223372036854775808".to_vec()
        );
        assert_eq!(
            ListpackEntry::String(b"hello".to_vec()).into_bytes(),
            b"hello".to_vec()
        );
    }

    // ── zset-listpack pair decode: byte-/bit-exact vs the pre-change reference ──
    // (frankenredis zsetlpscore) `decode_zset_listpack_pairs` must be
    // indistinguishable from `decode_zset_listpack_pairs_orig` (decode_listpack +
    // pair-parse) on every accepted AND rejected input — the alloc elision is the
    // only difference.

    /// Compare pairs bit-exactly on the score (so -0.0 / inf / rounding all count).
    fn zpair_bits(pairs: &[(Vec<u8>, f64)]) -> Vec<(Vec<u8>, u64)> {
        pairs.iter().map(|(m, s)| (m.clone(), s.to_bits())).collect()
    }

    #[test]
    fn zset_listpack_pairs_matches_orig_and_is_bit_exact() {
        // Interleaved (member, score) covering every score encoding class:
        // integer scores (int entries) and fractional/inf scores (string entries),
        // plus an integer-encoded member.
        let lp = assemble(&[
            &entry_6bit_str(b"m000"),
            &entry_7bit_uint(0), // score 0 (int)
            &entry_6bit_str(b"m001"),
            &entry_6bit_str(b"1.5"), // score 1.5 (str)
            &entry_6bit_str(b"m002"),
            &entry_13bit_int(-42), // score -42 (int)
            &entry_6bit_str(b"m003"),
            &entry_6bit_str(b"inf"), // score +inf (str)
            &entry_6bit_str(b"m004"),
            &entry_32bit_int(1_000_000), // score 1e6 (int)
            &entry_6bit_str(b"m005"),
            &entry_6bit_str(b"-2.5"), // score -2.5 (str)
            &entry_7bit_uint(7),      // integer MEMBER
            &entry_6bit_str(b"3.14159"),
        ]);
        let new = decode_zset_listpack_pairs(&lp).expect("new decode");
        let orig = decode_zset_listpack_pairs_orig(&lp).expect("orig decode");
        assert_eq!(zpair_bits(&new), zpair_bits(&orig), "new vs orig diverged");
        assert_eq!(new[3].0, b"m003");
        assert!(new[3].1.is_infinite() && new[3].1 > 0.0);
        assert_eq!(new[6].0, b"7"); // int member 7 renders to "7"
        assert_eq!(new[6].1, 3.14159);
    }

    #[test]
    fn zset_listpack_pairs_empty_is_ok_empty() {
        let lp = assemble(&[]);
        assert!(decode_zset_listpack_pairs(&lp).unwrap().is_empty());
        assert!(decode_zset_listpack_pairs_orig(&lp).unwrap().is_empty());
    }

    #[test]
    fn zset_listpack_pairs_rejects_same_inputs_as_orig() {
        // Odd element count (dangling member, no score).
        let odd = assemble(&[
            &entry_6bit_str(b"m0"),
            &entry_7bit_uint(1),
            &entry_6bit_str(b"m1"),
        ]);
        assert!(decode_zset_listpack_pairs(&odd).is_err());
        assert!(decode_zset_listpack_pairs_orig(&odd).is_err());

        // Unparseable string score.
        let bad = assemble(&[&entry_6bit_str(b"m0"), &entry_6bit_str(b"not_a_number")]);
        assert!(decode_zset_listpack_pairs(&bad).is_err());
        assert!(decode_zset_listpack_pairs_orig(&bad).is_err());

        // Truncated blob (drop the terminator → header total_bytes mismatch).
        let mut trunc = assemble(&[&entry_6bit_str(b"m0"), &entry_7bit_uint(1)]);
        trunc.pop();
        assert!(decode_zset_listpack_pairs(&trunc).is_err());
        assert!(decode_zset_listpack_pairs_orig(&trunc).is_err());
    }

    #[test]
    fn zset_listpack_pairs_matches_orig_on_encoder_built_blob() {
        // Faithful blob via the production listpack encoder (int-encodes canonical
        // integer scores, string-encodes fractional ones) — the mix the rdb_codec
        // `build_mixed_zset_entries` bench uses (1/3 integer, 2/3 fractional).
        let mut refs: Vec<Vec<u8>> = Vec::new();
        for i in 0..200i64 {
            refs.push(format!("m{i:04}:tag").into_bytes());
            if i % 3 == 0 {
                refs.push(format!("{}", i - 100).into_bytes());
            } else {
                refs.push(format!("{}", (i as f64) * 1.5 + 0.125).into_bytes());
            }
        }
        let slices: Vec<&[u8]> = refs.iter().map(Vec::as_slice).collect();
        let lp = crate::encode_listpack_strings_blob(&slices).expect("encode zset lp");
        let new = decode_zset_listpack_pairs(&lp).expect("new");
        let orig = decode_zset_listpack_pairs_orig(&lp).expect("orig");
        assert_eq!(zpair_bits(&new), zpair_bits(&orig));
        assert_eq!(new.len(), 200);
    }
}
