//! Upstream-compatible listpack decoder.
//!
//! Implements forward iteration over the Redis listpack binary format as
//! documented in `legacy_redis_code/redis/src/listpack.c`. Used by the
//! RDB stream decoder (br-frankenredis-hjub/qi6z) and by the DUMP/RESTORE
//! container-type support (br-frankenredis-hycu) to read listpack blobs
//! embedded inside bigger structures.
//!
//! No writer is provided — our RDB/DUMP encoders still use the legacy
//! frankenredis-internal format for now. A writer lands in br-frankenredis-6zk9.
//!
//! (br-frankenredis-3g0p)

use std::error::Error;
use std::fmt;

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
            ListpackEntry::Integer(n) => n.to_string().into_bytes(),
            ListpackEntry::String(bytes) => bytes.clone(),
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
    /// Entry body or backlen points past the listpack end.
    TruncatedEntry,
    /// Backlen byte run exceeds the 5-byte maximum.
    InvalidBacklen,
    /// String entry's declared length would overflow usize.
    StringLengthOverflow,
}

impl fmt::Display for ListpackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortHeader => f.write_str("listpack shorter than 6-byte header"),
            Self::TotalBytesOutOfRange => f.write_str("listpack total-bytes header exceeds buffer"),
            Self::MissingTerminator => f.write_str("listpack missing 0xFF terminator"),
            Self::InvalidEncoding(b) => write!(f, "listpack invalid encoding byte 0x{b:02x}"),
            Self::TruncatedEntry => f.write_str("listpack entry body runs past end"),
            Self::InvalidBacklen => f.write_str("listpack backlen exceeds 5 bytes"),
            Self::StringLengthOverflow => f.write_str("listpack string length overflows usize"),
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
    if (total_bytes as usize) > data.len() {
        return Err(ListpackError::TotalBytesOutOfRange);
    }
    if data[(total_bytes as usize).saturating_sub(1)] != LISTPACK_EOF {
        return Err(ListpackError::MissingTerminator);
    }
    Ok((total_bytes, num_elements))
}

/// Decode a single entry at `cursor`. Returns the decoded entry and the
/// total number of bytes the entry occupies (encoding + data + backlen).
fn decode_entry(data: &[u8], cursor: usize) -> Result<(ListpackEntry, usize), ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    // 7-bit uint: 0xxxxxxx
    if first & 0x80 == 0 {
        let value = i64::from(first & 0x7F);
        let data_len = 1;
        let backlen_len = backlen_byte_count(data_len);
        return Ok((ListpackEntry::Integer(value), data_len + backlen_len));
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
        let bytes = data[start..end].to_vec();
        let data_len = 1 + slen;
        let backlen_len = backlen_byte_count(data_len);
        return Ok((ListpackEntry::String(bytes), data_len + backlen_len));
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
        let backlen_len = backlen_byte_count(data_len);
        return Ok((ListpackEntry::Integer(signed), data_len + backlen_len));
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
        let bytes = data[start..end].to_vec();
        let data_len = 2 + slen;
        let backlen_len = backlen_byte_count(data_len);
        return Ok((ListpackEntry::String(bytes), data_len + backlen_len));
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
            let bytes = data[start..end].to_vec();
            let data_len = 5 + slen;
            let backlen_len = backlen_byte_count(data_len);
            Ok((ListpackEntry::String(bytes), data_len + backlen_len))
        }
        0xF1 => {
            // 16-bit signed int: 11110001 + u16 LE.
            if cursor + 3 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
            let data_len = 3;
            let backlen_len = backlen_byte_count(data_len);
            Ok((
                ListpackEntry::Integer(i64::from(raw)),
                data_len + backlen_len,
            ))
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
            let backlen_len = backlen_byte_count(data_len);
            Ok((ListpackEntry::Integer(signed), data_len + backlen_len))
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
            let backlen_len = backlen_byte_count(data_len);
            Ok((
                ListpackEntry::Integer(i64::from(raw)),
                data_len + backlen_len,
            ))
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
            let backlen_len = backlen_byte_count(data_len);
            Ok((ListpackEntry::Integer(raw), data_len + backlen_len))
        }
        _ => Err(ListpackError::InvalidEncoding(first)),
    }
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
    let (total_bytes, _num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1; // terminator is at total_bytes - 1
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut entries = Vec::new();
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
    Ok(entries)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
    fn to_bytes_converts_int_to_decimal_string() {
        assert_eq!(ListpackEntry::Integer(42).to_bytes(), b"42".to_vec());
        assert_eq!(ListpackEntry::Integer(-1).to_bytes(), b"-1".to_vec());
        assert_eq!(
            ListpackEntry::String(b"hello".to_vec()).to_bytes(),
            b"hello".to_vec()
        );
    }
}
