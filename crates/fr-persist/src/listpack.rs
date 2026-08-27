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
///
/// (frankenredis-33832) SIZE IS THE COST OF THIS TYPE. `decode_value_spans`
/// pushes one of these per listpack entry, and callgrind line attribution put
/// `core::ptr::write` — the struct copy inside that push — at 26.4% of the
/// function, 40 instructions per element on a 40-byte struct, i.e. one
/// instruction per byte. Shrinking the struct is therefore a direct, linear
/// reduction of the hottest line on the RESTORE decode path, and it also shrinks
/// the retained `Vec<ListpackValueSpan>` that `packed_set` keeps for a
/// listpack-backed list, so it is an RSS win as well as an instruction win.
///
/// Two things bought the shrink from 40 bytes to 24:
///   - the string range is `u32`, not `usize`. A listpack's own header stores
///     `total_bytes` as a `u32`, so a range that needed more than 32 bits could
///     not describe a valid listpack in the first place.
///   - the integer variant no longer caches the `i64` it was rendered from.
///
/// Variants are deliberately not `pub`-constructible-in-practice: nothing
/// outside this module builds or matches one (verified repo-wide), every
/// consumer goes through [`Self::as_bytes`], so the layout is free to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListpackValueSpan {
    /// Byte-string entry borrowed from the original listpack payload.
    String(Range<u32>),
    /// Integer entry rendered as Redis's decimal byte-string value.
    Integer(ListpackIntegerBytes),
}

// Lock the shrink in. Without this the type could drift back to 40 bytes under a
// later edit and silently give back the win, with nothing failing.
// 32 rather than a tighter number on purpose: the exact size depends on whether
// the layout algorithm tucks the discriminant into the `Integer` variant's
// trailing padding (24) or gives it its own aligned slot (28), and pinning a
// guessed value would turn a layout detail into a build failure. 32 is below the
// 40 this type used to be, so the win cannot silently regress, and the
// accompanying test reports the actual size.
const _: () = assert!(
    std::mem::size_of::<ListpackValueSpan>() <= 32,
    "ListpackValueSpan must stay <= 32 bytes: decode_value_spans pays ~1 instruction \
     per byte of it per listpack entry (frankenredis-33832)"
);

/// Inline decimal representation for any i64 (`i64::MIN` is 20 bytes).
///
/// (frankenredis-33832) The `value: i64` this used to carry — added by w08xv so a
/// score consumer would not have to reparse the decimal text — is gone, along
/// with its `value()` / `ListpackValueSpan::as_i64()` accessors. They had NO
/// caller anywhere in the workspace: the sorted-set path that motivated them
/// takes its number from `decode_zset_spans_and_scores`, which reads the value
/// straight off `RawListpackValue::Integer` before a span is ever built, so it
/// never needed the cached copy. Carrying it cost 8 bytes on EVERY entry —
/// including the string entries that are the whole of a typical hash RESTORE —
/// to serve nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListpackIntegerBytes {
    /// The rendered decimal, RIGHT-aligned in the buffer, exactly as
    /// `decimal_i64_scratch` produced it. Bytes before `start` are the zeros the
    /// scratch was initialised with.
    bytes: [u8; 20],
    /// Index of the first rendered byte. The value always runs to the END of the
    /// buffer, so no separate length is needed.
    start: u8,
}

impl ListpackIntegerBytes {
    /// (frankenredis-qj6jn) KEEP THE DIGITS WHERE THE RENDERER PUT THEM.
    ///
    /// `decimal_i64_scratch` writes the decimal RIGHT-aligned into a zeroed `[u8; 20]` and hands
    /// back `(buffer, start)`. This used to then zero a SECOND `[u8; 20]` and `copy_from_slice`
    /// the digits to the front of it, so that `as_slice` could return `bytes[..len]`. In the
    /// disassembly that is two 20-byte zeroings, a call to `memcpy@GLIBC` and a further pair of
    /// 16-byte moves, per INTEGER listpack entry, to relocate at most 20 bytes that were already
    /// sitting in a buffer of exactly the right size.
    ///
    /// Storing the start index instead of a length removes all of it: the renderer's buffer IS
    /// the field, and `as_slice` slices from `start` to the end. Same bytes, same order, same
    /// derived `PartialEq` semantics — `decimal_i64_scratch` is deterministic and zero-fills, so
    /// one value still has exactly one representation.
    fn new(value: i64) -> Self {
        // Render straight into the field that will keep the bytes — see `decimal_i64_into`.
        let mut bytes = [0u8; 20];
        let start = crate::decimal_i64_into(&mut bytes, value) as u8;
        Self { bytes, start }
    }

    /// (frankenredis-qj6jn) `#[inline]`: this is a sub-slice, and it is taken ONCE PER DECODED
    /// ENTRY by every consumer of a span — the list restore fold, the hash/set/zset builders.
    /// Un-inlined it was its own callgrind frame at 2,400 instr/key, 8.00 per element, on a
    /// 300-integer list RESTORE, which is a call and a return around two loads and a subtraction.
    #[must_use]
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[usize::from(self.start)..]
    }
}

/// Narrow a decoded payload offset pair to the `u32` pair a span stores.
///
/// (frankenredis-33832) CHECKED, not an `as` cast, and the distinction matters.
/// A valid listpack cannot address beyond 32 bits — its own header stores
/// `total_bytes` as a `u32` — but `decode_entry_value_span` bounds its ranges
/// against `data.len()`, and a caller is free to hand this decoder a buffer
/// longer than `u32::MAX`. Under an `as` cast a crafted entry could then wrap to
/// a DIFFERENT, still in-bounds range, and `as_bytes` would hand back the wrong
/// bytes with no error anywhere — a wrong answer, which is strictly worse than a
/// rejection. This turns that case into the rejection it should be.
#[inline]
fn narrow_span(start: usize, end: usize) -> Result<Range<u32>, ListpackError> {
    let start = u32::try_from(start).map_err(|_| ListpackError::StringLengthOverflow)?;
    let end = u32::try_from(end).map_err(|_| ListpackError::StringLengthOverflow)?;
    Ok(start..end)
}

impl ListpackValueSpan {
    fn integer(value: i64) -> Self {
        Self::Integer(ListpackIntegerBytes::new(value))
    }

    #[must_use]
    #[inline]
    pub fn as_bytes<'a>(&'a self, listpack: &'a [u8]) -> &'a [u8] {
        match self {
            Self::String(range) => &listpack[range.start as usize..range.end as usize],
            Self::Integer(bytes) => bytes.as_slice(),
        }
    }

    /// The element's LENGTH, without materializing the element.
    ///
    /// (frankenredis-qj6jn) The list restore fold sums every element's length and never looks at
    /// the bytes; asking `as_bytes` for them costs a bounds-checked subslice — two comparisons and
    /// a pointer/len pair — per entry, to produce a slice whose only use is `.len()`. A string
    /// span already KNOWS its length as `end - start`, and an integer span's is `20 - start`.
    /// Callgrind charged the fold 33.07 instructions per element on an all-string RESTORE, where
    /// the work it must do is an add and a compare.
    #[must_use]
    #[inline]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::String(range) => (range.end - range.start) as usize,
            Self::Integer(bytes) => bytes.as_slice().len(),
        }
    }

    /// The element's FIRST byte, or `None` when the element is empty.
    ///
    /// (frankenredis-qj6jn) Same reason as [`Self::byte_len`]: the restore fold's canonical-decimal
    /// guard needs one byte, not a slice. A listpack a decoder produced always has its string
    /// ranges inside the blob, so the index is in bounds — but this uses `get`, because the span
    /// and the blob are separate arguments and nothing in the type system ties them together.
    #[must_use]
    #[inline]
    pub fn first_byte(&self, listpack: &[u8]) -> Option<u8> {
        match self {
            Self::String(range) => {
                if range.start < range.end {
                    listpack.get(range.start as usize).copied()
                } else {
                    None
                }
            }
            Self::Integer(bytes) => bytes.as_slice().first().copied(),
        }
    }

    /// True when this entry was STRING-encoded in the source listpack.
    ///
    /// (frankenredis-qj6jn) The restore fold's guard needs the encoding, and matching the variant
    /// from another crate would defeat the "nothing outside this module matches one" invariant the
    /// type's own comment relies on to keep its layout free to change.
    #[must_use]
    #[inline]
    pub fn is_string_encoded(&self) -> bool {
        matches!(self, Self::String(_))
    }

    /// The entry's integer value when it was integer-encoded in the listpack.
    /// Returns `None` for string-encoded entries.
    ///
    /// (frankenredis-33832) DERIVED by parsing the rendered decimal, not cached.
    /// w08xv originally kept the source `i64` in a field so a score consumer
    /// would not reparse — but that cost 8 bytes on EVERY span, including the
    /// string entries that make up a typical hash RESTORE, and no production
    /// caller ever took it: the sorted-set path reads its number off
    /// `RawListpackValue::Integer` inside [`decode_zset_spans_and_scores`],
    /// before a span exists. Deriving it keeps the accessor (and the round-trip
    /// invariant its tests pin) while letting the struct shrink.
    ///
    /// DO NOT put this on a hot path — use [`decode_zset_spans_and_scores`],
    /// which is what the field existed to serve in the first place.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::String(_) => None,
            Self::Integer(bytes) => std::str::from_utf8(bytes.as_slice())
                .ok()
                .and_then(|text| text.parse().ok()),
        }
    }
}

/// A compact listpack entry retained by a list quicklist node.
///
/// Unlike [`ListpackValueSpan`], integer decimal bytes live in the companion
/// arena returned by [`decode_retained_listpack_spans`].  The arena must remain
/// coupled to these spans: list nodes keep their decoded index after the decode
/// call returns, so a decode-local buffer would dangle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainedListpackValueSpan {
    String(Range<u32>),
    Integer(Range<u32>),
}

const _: () = assert!(
    std::mem::size_of::<RetainedListpackValueSpan>() <= 12,
    "retained listpack spans must stay compact (frankenredis-gvm6z)"
);

impl RetainedListpackValueSpan {
    #[must_use]
    #[inline]
    pub fn as_bytes<'a>(&self, listpack: &'a [u8], integer_bytes: &'a [u8]) -> &'a [u8] {
        let range = match self {
            Self::String(range) => return &listpack[range.start as usize..range.end as usize],
            Self::Integer(range) => range,
        };
        &integer_bytes[range.start as usize..range.end as usize]
    }

    #[must_use]
    #[inline]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::String(range) | Self::Integer(range) => (range.end - range.start) as usize,
        }
    }

    #[must_use]
    #[inline]
    pub fn first_byte(&self, listpack: &[u8], integer_bytes: &[u8]) -> Option<u8> {
        self.as_bytes(listpack, integer_bytes).first().copied()
    }

    #[must_use]
    #[inline]
    pub fn is_string_encoded(&self) -> bool {
        matches!(self, Self::String(_))
    }
}

/// Decoded spans and the separately retained decimal bytes for integer entries.
///
/// Keeping the arena beside the spans preserves the listpack payload's exact
/// length, which is part of the quicklist fill policy, while making the common
/// string span 12 bytes rather than the generic decoder's 32-byte enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedListpackSpans {
    entries: Vec<RetainedListpackValueSpan>,
    integer_bytes: Vec<u8>,
}

impl RetainedListpackSpans {
    #[must_use]
    pub fn entries(&self) -> &[RetainedListpackValueSpan] {
        &self.entries
    }

    #[must_use]
    pub fn integer_bytes(&self) -> &[u8] {
        &self.integer_bytes
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn into_parts(self) -> (Vec<RetainedListpackValueSpan>, Vec<u8>) {
        (self.entries, self.integer_bytes)
    }
}

/// Decoder failure modes. Narrow set — callers either succeed or reject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawListpackValue {
    /// The integer payload when this is an integer entry; unused otherwise.
    int: i64,
    /// Byte offset into the source listpack when this is a string entry.
    start: u32,
    /// String byte length, or [`Self::INT_LEN`] to mark the integer form.
    len: u32,
}

// Lock the size in. Nothing FAILS if this drifts -- it just silently gives back
// the win -- so assert it.
const _: () = assert!(std::mem::size_of::<RawListpackValue>() == 16);

/// A by-value view for matching. Constructing this is free and it is never
/// STORED; the vector holds the POD form above. Not `Copy` only because
/// `Range` is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawKind {
    Integer(i64),
    String(Range<u32>),
}

impl RawListpackValue {
    /// Sentinel length marking the integer form. A real string can never reach it:
    /// a listpack's own `total_bytes` is a `u32` and must also cover the header,
    /// this entry's encoding byte and its backlen.
    const INT_LEN: u32 = u32::MAX;

    #[inline(always)]
    #[must_use]
    pub fn integer(value: i64) -> Self {
        Self {
            int: value,
            start: 0,
            len: Self::INT_LEN,
        }
    }

    #[inline(always)]
    #[must_use]
    pub fn string(range: Range<u32>) -> Self {
        debug_assert!(range.end >= range.start, "listpack span must not invert");
        Self {
            int: 0,
            start: range.start,
            len: range.end - range.start,
        }
    }

    #[inline(always)]
    #[must_use]
    pub fn kind(self) -> RawKind {
        if self.len == Self::INT_LEN {
            RawKind::Integer(self.int)
        } else {
            RawKind::String(self.start..self.start + self.len)
        }
    }
}

/// Allocation-free entry decode: the exact byte-dispatch of [`decode_entry`] but
/// string entries return their `Range` rather than a copied `Vec`. Returns the
/// raw value and the total bytes the entry occupies (encoding + data + backlen).
/// [`decode_entry`] is a thin materializing wrapper over this, so both share one
/// parser and cannot drift.
/// Narrow an already-bounds-checked `[start, end)` to the `u32` range
/// `RawListpackValue::String` stores. A well-formed listpack cannot overflow this
/// (its header's `total_bytes` is itself a `u32`); a malformed one is rejected
/// here rather than silently truncated.
/// The inverse of [`u32_range`], for indexing the source bytes.
#[inline(always)]
fn usize_range(range: Range<u32>) -> Range<usize> {
    range.start as usize..range.end as usize
}

#[inline(always)]
fn u32_range(start: usize, end: usize) -> Result<Range<u32>, ListpackError> {
    let start = u32::try_from(start).map_err(|_| ListpackError::StringLengthOverflow)?;
    let end = u32::try_from(end).map_err(|_| ListpackError::StringLengthOverflow)?;
    Ok(start..end)
}

#[inline(always)]
fn decode_entry_raw(
    data: &[u8],
    cursor: usize,
) -> Result<(RawListpackValue, usize), ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    // 7-bit uint: 0xxxxxxx
    if first & 0x80 == 0 {
        let value = i64::from(first & 0x7F);
        let data_len = 1;
        let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::integer(value), entry_len));
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
        return Ok((RawListpackValue::string(u32_range(start, end)?), entry_len));
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
        let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
        return Ok((RawListpackValue::integer(signed), entry_len));
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
        return Ok((RawListpackValue::string(u32_range(start, end)?), entry_len));
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
            Ok((RawListpackValue::string(u32_range(start, end)?), entry_len))
        }
        0xF1 => {
            // 16-bit signed int: 11110001 + u16 LE.
            if cursor + 3 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
            let data_len = 3;
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::integer(i64::from(raw)), entry_len))
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::integer(signed), entry_len))
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::integer(i64::from(raw)), entry_len))
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            Ok((RawListpackValue::integer(raw), entry_len))
        }
        _ => Err(ListpackError::InvalidEncoding(first)),
    }
}

fn decode_entry(data: &[u8], cursor: usize) -> Result<(ListpackEntry, usize), ListpackError> {
    let (raw, entry_len) = decode_entry_raw(data, cursor)?;
    let entry = match raw.kind() {
        RawKind::Integer(value) => ListpackEntry::Integer(value),
        // Materialize the borrowed range into the owned payload — the single
        // `to_vec()` the pre-refactor `decode_entry` performed inline.
        RawKind::String(range) => ListpackEntry::String(data[usize_range(range)].to_vec()),
    };
    Ok((entry, entry_len))
}

/// The one-byte-backlen case of [`entry_len_with_backlen`], for call sites that KNOW `data_len` is
/// a compile-time constant at or below 127.
///
/// (frankenredis-qj6jn) Every INTEGER arm of the decoder passes a literal — 1, 2, 3, 4, 5 or 9 —
/// so out of line those constants bought nothing: `backlen_byte_count`'s five-way match ran, its
/// result was compared against 1, and the whole thing cost a call. Callgrind charged that frame
/// 8,700 instr/key, 29.00 per element, on a 300-integer list RESTORE, for what is on this path a
/// bounds check and one byte compare. Inlined with a constant argument it folds to exactly that.
///
/// STRING arms deliberately still call `entry_len_with_backlen`. Their `data_len` is `1 + slen`,
/// which folds nothing, and routing them here was MEASURED as a trade rather than a win: on the
/// same pair of binaries integer RESTORE read −9.59 pct and string RESTORE read +0.85 pct, the
/// string arms paying for a bigger decoder that bought them nothing. Splitting the call site
/// instead of the function body keeps the string path byte-identical.
///
/// The dropped `checked_add` is safe: with a one-byte backlen, `backlen_start + 1 > data.len()`
/// is `backlen_start >= data.len()`, and the overflow that guard caught (`backlen_start ==
/// usize::MAX`) is caught by that same test with the same `TruncatedEntry`. Error variants and
/// their precedence are unchanged.
#[inline]
fn entry_len_one_byte_backlen(
    data: &[u8],
    cursor: usize,
    data_len: usize,
) -> Result<usize, ListpackError> {
    debug_assert!(
        data_len <= 127,
        "entry_len_one_byte_backlen is only valid for a one-byte backlen"
    );
    let backlen_start = cursor
        .checked_add(data_len)
        .ok_or(ListpackError::TruncatedEntry)?;
    if backlen_start >= data.len() {
        return Err(ListpackError::TruncatedEntry);
    }
    if data[backlen_start] != data_len as u8 {
        return Err(ListpackError::InvalidBacklen);
    }
    Ok(data_len + 1)
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
    // A zset listpack is strictly alternating member/score entries. When the
    // header carries an exact count, reject an impossible odd shape before
    // walking entries or materializing any member buffers. The unknown-count
    // sentinel still needs the structural walk below.
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && !num_elements.is_multiple_of(2) {
        return Err(ListpackError::ElementCountMismatch);
    }
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
                pending_member = Some(match raw.kind() {
                    RawKind::Integer(n) => crate::decimal_i64_bytes(n),
                    RawKind::String(range) => data[usize_range(range)].to_vec(),
                });
            }
            Some(member) => {
                // Score position: read the f64 WITHOUT allocating the score string.
                let score = match raw.kind() {
                    RawKind::Integer(n) => n as f64,
                    RawKind::String(range) => std::str::from_utf8(&data[usize_range(range)])
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
pub fn decode_zset_listpack_pairs_orig(data: &[u8]) -> Result<Vec<(Vec<u8>, f64)>, ListpackError> {
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

// (frankenredis-qj6jn) Decode ONE entry straight into the caller's `Vec` and return
// only the consumed length.
//
// The previous form returned `Result<(ListpackValueSpan, usize), ListpackError>`.
// `ListpackValueSpan` is 28-32 bytes because its `Integer` variant carries the
// rendered decimal inline, so that tuple is ~40 bytes wrapped in a `Result` — and it
// was built in the callee's return slot, moved out into the caller's locals, then
// copied AGAIN by `values.push(value)`. `#[inline]` did not collapse it: the function
// has eight return points, each materialising the tuple separately.
//
// Measured on the DEBUG RELOAD decode path (200 keys x 40-field hash, callgrind slope,
// 80 entries/key), the old form spent 117 instr per listpack entry, of which the fat
// move accounted for ~56: `ptr::write` 16, `Result` plumbing 16, tuple construction 12
// and `Vec::push` 12. Pushing in place removes the return slot and the second copy —
// the span is constructed once, directly where it lives.
//
// Byte-for-byte equivalent to the old form, and the ORDER of fallible steps is
// preserved exactly so error PRECEDENCE is unchanged: `entry_len_with_backlen` is
// still evaluated before `narrow_span` in every string arm, and `out.push` is the last
// action on every success path, so a failing entry leaves nothing behind. (Even that
// is unobservable — every error path discards the whole `Vec` — but keeping it true
// means the reference oracle can compare pushes as well as results.)
#[inline]
fn decode_entry_value_span_into(
    data: &[u8],
    cursor: usize,
    out: &mut Vec<ListpackValueSpan>,
) -> Result<usize, ListpackError> {
    let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

    if first & 0x80 == 0 {
        let value = i64::from(first & 0x7F);
        let data_len = 1;
        let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
        out.push(ListpackValueSpan::integer(value));
        return Ok(entry_len);
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
        out.push(ListpackValueSpan::String(narrow_span(start, end)?));
        return Ok(entry_len);
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
        let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
        out.push(ListpackValueSpan::integer(signed));
        return Ok(entry_len);
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
        out.push(ListpackValueSpan::String(narrow_span(start, end)?));
        return Ok(entry_len);
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
            out.push(ListpackValueSpan::String(narrow_span(start, end)?));
            Ok(entry_len)
        }
        0xF1 => {
            if cursor + 3 > data.len() {
                return Err(ListpackError::TruncatedEntry);
            }
            let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
            let data_len = 3;
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            out.push(ListpackValueSpan::integer(i64::from(raw)));
            Ok(entry_len)
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            out.push(ListpackValueSpan::integer(signed));
            Ok(entry_len)
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            out.push(ListpackValueSpan::integer(i64::from(raw)));
            Ok(entry_len)
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
            let entry_len = entry_len_one_byte_backlen(data, cursor, data_len)?;
            out.push(ListpackValueSpan::integer(raw));
            Ok(entry_len)
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

/// Decode a listpack directly into the representation retained by a quicklist
/// node.
///
/// String entries keep ranges into the original payload. Integer entries are
/// rendered once into a side arena owned by the returned value. The caller must
/// retain that arena with the spans; extending `data` instead would change the
/// payload length used by the quicklist fill policy.
pub fn decode_retained_listpack_spans(data: &[u8]) -> Result<RetainedListpackSpans, ListpackError> {
    // The generic decoder already walks every entry once and preserves string
    // ranges plus the canonical decimal bytes for integer encodings.  Reusing
    // it here keeps the compact retained representation without paying a
    // second raw-entry decode on RESTORE's list path.
    let decoded = decode_value_spans(data)?;
    let mut entries = Vec::with_capacity(decoded.len());
    let mut integer_bytes = Vec::new();

    for span in decoded {
        let entry = match span {
            ListpackValueSpan::String(range) => RetainedListpackValueSpan::String(range),
            ListpackValueSpan::Integer(bytes) => {
                let start = integer_bytes.len();
                integer_bytes.extend_from_slice(bytes.as_slice());
                let end = integer_bytes.len();
                RetainedListpackValueSpan::Integer(narrow_span(start, end)?)
            }
        };
        entries.push(entry);
    }
    Ok(RetainedListpackSpans {
        entries,
        integer_bytes,
    })
}

/// Return Redis-observable values while retaining string payload ranges.
///
/// String entries borrow from `data`; integer entries store their canonical
/// decimal byte-string form inline. This lets callers retain a listpack node
/// without allocating one `Vec<u8>` per element while preserving normal list
/// iteration semantics.
pub fn decode_value_spans(data: &[u8]) -> Result<Vec<ListpackValueSpan>, ListpackError> {
    decode_value_spans_impl::<true>(data)
}

/// Same-binary A/B hook: `PRESIZE == true` is the production path (pre-size the spans `Vec` from the
/// header's element count); `PRESIZE == false` is the pre-change baseline that grows from empty.
#[doc(hidden)]
pub fn bench_decode_value_spans<const PRESIZE: bool>(
    data: &[u8],
) -> Result<Vec<ListpackValueSpan>, ListpackError> {
    decode_value_spans_impl::<PRESIZE>(data)
}

/// Decode a sorted-set listpack into (member span, score) pairs without ever
/// rendering a score to decimal. (frankenredis-w08xv)
///
/// A zset listpack alternates member, score. `decode_value_spans` must render an
/// integer-encoded entry to its canonical decimal bytes, because its callers are
/// byte-oriented and cannot know the entry was an integer. On this path the
/// SCORE is wanted as a number, so that rendering is the first half of a round
/// trip whose second half — parsing the text back to `f64` — was removed
/// separately. Rendering scores measured 13.1% of a 128-member zset RESTORE
/// (`ListpackValueSpan::integer` plus `write_u64_digits`) under callgrind.
///
/// MEMBERS still render, because an integer-encoded member is a legitimate
/// member whose Redis-observable form IS its decimal bytes. Only the score half
/// skips it, which is why this is a separate function rather than a flag on the
/// general decoder: a span that silently carried no bytes would be a correctness
/// hazard for any caller that later asked for them.
pub fn decode_zset_spans_and_scores(
    data: &[u8],
) -> Result<Vec<(ListpackValueSpan, f64)>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut pairs = if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::with_capacity(usize::from(num_elements) / 2)
    } else {
        Vec::new()
    };
    let mut count = 0usize;
    // (BlackThrush 2026-08-26) DECODE THE PAIR, NOT ONE ENTRY AND A MEMORY OF THE
    // LAST ONE. This loop used to carry the member in an
    // `Option<ListpackValueSpan>` and `.take()` it on every entry, so each pair
    // paid two moves of a 32-byte span (the type is capped at 32 by a
    // compile-time assert, so the `Option` is ~40) plus two discriminant writes
    // and a match, purely to remember which half of the pair it was on. A zset
    // listpack strictly alternates member, score, member, score -- that is the
    // shape of the data, so it can be the shape of the loop.
    //
    // Measured before the change: this function's SELF cost is 3,630 instructions
    // per zset RESTORE of 40 members, 45 per entry on top of `decode_entry_raw`.
    //
    // SAME ERRORS, same order of detection. A trailing member with no score used
    // to fall out of the `pending.is_some()` check after the loop; it now falls
    // out of the score decode, which reaches the same
    // `ElementCountMismatch`. Every other guard -- the per-entry bounds check
    // against `end`, the terminator check, the header count check -- is
    // unchanged and still runs per ENTRY, not per pair.
    while cursor < end {
        let (raw_member, consumed) = decode_entry_raw(data, cursor)?;
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
        // Member half: rendered exactly as the general decoder would.
        let member = match raw_member.kind() {
            RawKind::String(range) => ListpackValueSpan::String(range),
            RawKind::Integer(value) => ListpackValueSpan::integer(value),
        };

        // Score half. An odd element count lands here with nothing left to read.
        if cursor >= end {
            return Err(ListpackError::ElementCountMismatch);
        }
        let (raw_score, consumed) = decode_entry_raw(data, cursor)?;
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
        // An integer-encoded score is taken as a number and never rendered. A
        // string-encoded one is parsed, and that is the only branch that can
        // produce NaN.
        let score = match raw_score.kind() {
            #[allow(clippy::cast_precision_loss)]
            RawKind::Integer(value) => value as f64,
            RawKind::String(range) => {
                let text = std::str::from_utf8(&data[usize_range(range)])
                    .map_err(|_| ListpackError::TruncatedEntry)?;
                let parsed: f64 = text.parse().map_err(|_| ListpackError::TruncatedEntry)?;
                if parsed.is_nan() {
                    return Err(ListpackError::TruncatedEntry);
                }
                parsed
            }
        };
        pairs.push((member, score));
        count += 2;
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && count != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(pairs)
}

/// Bench-only reference: the pre-change zset span decode that
/// [`decode_zset_spans_and_scores`] replaces -- one entry per iteration, with the
/// member carried between iterations in an `Option<ListpackValueSpan>` and
/// `.take()`n on every entry.
///
/// Kept in-crate (like `decode_zset_listpack_pairs_orig` and
/// `entry_len_with_backlen_orig`) so the same-binary A/B measures exactly what
/// shipped: wall clock is unusable on this host and a server-level ratio needs a
/// quiet window, but a pure decode kernel measured by callgrind slope is
/// deterministic and load-immune. Result is identical to the production path;
/// `examples/zset_span_decode_ab.rs` asserts that before timing.
pub fn decode_zset_spans_and_scores_orig(
    data: &[u8],
) -> Result<Vec<(ListpackValueSpan, f64)>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut pairs = if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::with_capacity(usize::from(num_elements) / 2)
    } else {
        Vec::new()
    };
    let mut pending: Option<ListpackValueSpan> = None;
    let mut count = 0usize;
    while cursor < end {
        let (raw, consumed) = decode_entry_raw(data, cursor)?;
        count += 1;
        cursor = cursor
            .checked_add(consumed)
            .ok_or(ListpackError::TruncatedEntry)?;
        if cursor > end {
            return Err(ListpackError::TruncatedEntry);
        }
        match pending.take() {
            None => {
                pending = Some(match raw.kind() {
                    RawKind::String(range) => ListpackValueSpan::String(range),
                    RawKind::Integer(value) => ListpackValueSpan::integer(value),
                });
            }
            Some(member) => {
                let score = match raw.kind() {
                    #[allow(clippy::cast_precision_loss)]
                    RawKind::Integer(value) => value as f64,
                    RawKind::String(range) => {
                        let text = std::str::from_utf8(&data[usize_range(range)])
                            .map_err(|_| ListpackError::TruncatedEntry)?;
                        let parsed: f64 =
                            text.parse().map_err(|_| ListpackError::TruncatedEntry)?;
                        if parsed.is_nan() {
                            return Err(ListpackError::TruncatedEntry);
                        }
                        parsed
                    }
                };
                pairs.push((member, score));
            }
        }
    }
    if cursor != end {
        return Err(ListpackError::MissingTerminator);
    }
    if pending.is_some() {
        return Err(ListpackError::ElementCountMismatch);
    }
    if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN && count != usize::from(num_elements) {
        return Err(ListpackError::ElementCountMismatch);
    }
    Ok(pairs)
}

/// Decode every entry to its ALLOCATION-FREE raw form: integers stay `i64`,
/// strings stay a `Range` into `data`.
///
/// (BlackThrush 2026-08-26) For a consumer that reads a mix of integers and byte
/// strings, this is the right decode and the other two are both wrong.
/// `decode_listpack` allocates a `Vec<u8>` per string entry, which the caller then
/// clones again -- two allocations per field. `decode_value_spans` allocates
/// nothing per entry but RENDERS integers to decimal, so an integer read has to
/// parse the decimal back, and an upstream stream node listpack is mostly integers
/// (live and deleted counts, field counts, per-entry flags, ms and seq deltas,
/// lp_count). This keeps both halves cheap.
///
/// The caller materialises a string only where it actually needs owned bytes.
pub fn decode_raw_values(data: &[u8]) -> Result<Vec<RawListpackValue>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    let mut values = if num_elements != LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::with_capacity(usize::from(num_elements))
    } else {
        Vec::new()
    };
    while cursor < end {
        let (raw, consumed) = decode_entry_raw(data, cursor)?;
        values.push(raw);
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

fn decode_value_spans_impl<const PRESIZE: bool>(
    data: &[u8],
) -> Result<Vec<ListpackValueSpan>, ListpackError> {
    let (total_bytes, num_elements) = parse_header(data)?;
    let end = (total_bytes as usize) - 1;
    let mut cursor = LISTPACK_HEADER_SIZE;
    // Pre-size from the header's exact element count (the common compact case) so the spans are
    // collected in one allocation instead of growing from empty (~log2(n) realloc+copies per
    // decode) — this is the RESTORE hot path (hash/zset/set/list `*_from_listpack_spans`). The
    // UNKNOWN sentinel (> 65534 elements) keeps the default. Capacity never affects content, so
    // the decoded spans are byte-identical. Mirrors `decode_listpack`.
    let mut values = if PRESIZE && num_elements != LISTPACK_HDR_NUMELE_UNKNOWN {
        Vec::with_capacity(usize::from(num_elements))
    } else {
        Vec::new()
    };
    while cursor < end {
        let consumed = decode_entry_value_span_into(data, cursor, &mut values)?;
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

/// What a `RDB_TYPE_ZSET_LISTPACK` payload looks like, WITHOUT materializing a
/// single member.
///
/// (BlackThrush 2026-08-27) The load side needs three facts to decide whether a
/// zset can be kept in its on-disk form rather than decoded: how many members it
/// holds, how long its longest member is (the encoding-flag predicate --
/// `refresh_zset_encoding_flag_from_max_len`'s doc states scores never
/// participate), and whether any member repeats (the store's bulk builder demands
/// uniqueness). All three fall out of a walk that allocates ONE span vector and
/// zero member buffers, where `decode_zset_listpack_pairs` allocates one `Vec<u8>`
/// per member purely so the pairs can outlive the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZsetListpackShape {
    /// Number of (member, score) pairs.
    pub pair_count: usize,
    /// Longest member in Redis-observable bytes (an integer-encoded member counts
    /// its decimal rendering, which is what every reader sees).
    pub max_member_len: usize,
    /// True if some member appears twice. Such a payload is legal on the wire but
    /// cannot go through the unique-input bulk builder.
    pub has_duplicate_member: bool,
}

/// Cheap fixed-seed probe hash, the twin of the store's `restore_field_probe_hash`.
/// Correctness never depends on it: a collision only costs an extra byte compare.
#[inline]
fn zset_member_probe_hash(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = bytes.len() as u64;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(*chunk)).wrapping_mul(SEED);
    }
    if !rest.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rest.len()].copy_from_slice(rest);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(tail)).wrapping_mul(SEED);
    }
    h ^ (h >> 29)
}

/// Slot ceiling for the stack duplicate probe; above it the payload is not a
/// listpack-encodable zset under any default config, so the answer stops mattering
/// and the allocating set is fine.
const ZSET_STACK_DUP_MAX: usize = 128;

/// Duplicate-member probe over spans, allocation-free for the sizes that can be
/// retained. Mirrors `restore_items_have_duplicate_key`: a power-of-two table at
/// 2x the entry ceiling keeps linear probing at ~1.5 probes, and slots hold
/// `index + 1` so 0 stays the empty sentinel.
fn zset_member_spans_have_duplicate(data: &[u8], pairs: &[(ListpackValueSpan, f64)]) -> bool {
    if pairs.len() > ZSET_STACK_DUP_MAX {
        // Cold: such a payload cannot be listpack-encoded under any default
        // config, so it is never retained and this answer only steers the
        // fallback. std's hasher is fine here; the crate has no foldhash dep.
        let mut seen: std::collections::HashSet<&[u8]> =
            std::collections::HashSet::with_capacity(pairs.len());
        return !pairs
            .iter()
            .all(|(member, _)| seen.insert(member.as_bytes(data)));
    }
    const SLOTS: usize = ZSET_STACK_DUP_MAX * 2;
    const _: () = assert!(SLOTS.is_power_of_two());
    let mut table = [0u16; SLOTS];
    for (index, (member, _)) in pairs.iter().enumerate() {
        let bytes = member.as_bytes(data);
        let mut slot = (zset_member_probe_hash(bytes) as usize) & (SLOTS - 1);
        loop {
            let occupant = table[slot];
            if occupant == 0 {
                table[slot] = u16::try_from(index + 1).expect("index bounded by SLOTS/2");
                break;
            }
            if pairs[usize::from(occupant) - 1].0.as_bytes(data) == bytes {
                return true;
            }
            slot = (slot + 1) & (SLOTS - 1);
        }
    }
    false
}

/// Validate a `RDB_TYPE_ZSET_LISTPACK` payload and report its shape.
///
/// EXACTLY the acceptance of [`decode_zset_listpack_pairs`] -- same structural
/// walk, same terminator and element-count guards, same odd-count rejection, same
/// score parse (so an unparseable score is still `InvalidScore`) -- reached
/// through [`decode_zset_spans_and_scores`], which shares the raw-entry core with
/// it. What it does NOT do is allocate a member buffer per pair.
///
/// This is the zset twin of the hash listpack arm's `decode_value_spans` call:
/// carrying a blob past the decoder must not make the decoder accept payloads it
/// used to reject, or a corrupt RDB reads as decodable and every test asserting
/// rejection silently stops testing anything.
pub fn zset_listpack_shape(data: &[u8]) -> Result<ZsetListpackShape, ListpackError> {
    let pairs = decode_zset_spans_and_scores(data)?;
    let mut max_member_len = 0_usize;
    for (member, _) in &pairs {
        max_member_len = max_member_len.max(member.byte_len());
    }
    Ok(ZsetListpackShape {
        pair_count: pairs.len(),
        max_member_len,
        has_duplicate_member: zset_member_spans_have_duplicate(data, &pairs),
    })
}

/// What a `RDB_TYPE_SET_LISTPACK` payload looks like, WITHOUT materializing a
/// single member.
///
/// (BlackThrush 2026-08-27) The set twin of [`ZsetListpackShape`]. The load side
/// needs four facts to decide whether a set can be kept in its on-disk form:
/// how many members it holds, how long its longest member is (the
/// encoding-flag predicate), whether any member repeats (the store's bulk
/// builder demands uniqueness), and whether any member could be read as an
/// integer -- see [`SetListpackShape::has_possible_int_member`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetListpackShape {
    /// Number of members.
    pub member_count: usize,
    /// Longest member in Redis-observable bytes.
    pub max_member_len: usize,
    /// True if some member appears twice.
    pub has_duplicate_member: bool,
    /// True if ANY member might be read as a canonical integer by the store.
    ///
    /// DELIBERATELY OVER-INCLUSIVE, and that is the whole point. The store's
    /// `SetValue::try_bulk_unique_strings` bails the moment one member parses as an
    /// `i64`, sending the whole set down a different constructor; a retained set
    /// must materialize through the SAME constructor the eager path would have
    /// used, or the two routes could disagree. Rather than duplicate the store's
    /// hand-tuned `parse_i64` here (a predicate that can drift), this reports a
    /// strict SUPERSET of it: an integer-ENCODED listpack entry, or a string entry
    /// short enough to be an i64 and made only of digits with an optional leading
    /// `-`. Over-reporting costs a retention that does not happen; under-reporting
    /// would cost correctness.
    pub has_possible_int_member: bool,
}

/// Conservative "could the store read this as an integer?" test. See
/// [`SetListpackShape::has_possible_int_member`].
#[inline]
fn member_could_be_int(bytes: &[u8]) -> bool {
    let digits = match bytes.first() {
        Some(b'-') => &bytes[1..],
        Some(_) => bytes,
        None => return false,
    };
    // i64::MIN is 20 bytes with its sign; anything longer cannot parse.
    bytes.len() <= 20 && !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

/// Duplicate-member probe over spans, allocation-free for the sizes that can be
/// retained. Same table shape and reasoning as
/// [`zset_member_spans_have_duplicate`].
fn set_member_spans_have_duplicate(data: &[u8], spans: &[ListpackValueSpan]) -> bool {
    if spans.len() > ZSET_STACK_DUP_MAX {
        // Cold: such a payload cannot be listpack-encoded under any default config,
        // so it is never retained and this answer only steers the fallback.
        let mut seen: std::collections::HashSet<&[u8]> =
            std::collections::HashSet::with_capacity(spans.len());
        return !spans.iter().all(|m| seen.insert(m.as_bytes(data)));
    }
    const SLOTS: usize = ZSET_STACK_DUP_MAX * 2;
    const _: () = assert!(SLOTS.is_power_of_two());
    let mut table = [0u16; SLOTS];
    for (index, member) in spans.iter().enumerate() {
        let bytes = member.as_bytes(data);
        let mut slot = (zset_member_probe_hash(bytes) as usize) & (SLOTS - 1);
        loop {
            let occupant = table[slot];
            if occupant == 0 {
                table[slot] = u16::try_from(index + 1).expect("index bounded by SLOTS/2");
                break;
            }
            if spans[usize::from(occupant) - 1].as_bytes(data) == bytes {
                return true;
            }
            slot = (slot + 1) & (SLOTS - 1);
        }
    }
    false
}

/// Validate a `RDB_TYPE_SET_LISTPACK` payload and report its shape.
///
/// EXACTLY the acceptance the eager path applies -- it IS [`decode_value_spans`],
/// which is what `decode_listpack` validates with -- minus the per-member
/// `Vec<u8>`. Carrying a blob past the decoder must not make the decoder accept
/// payloads it used to reject.
pub fn set_listpack_shape(data: &[u8]) -> Result<SetListpackShape, ListpackError> {
    let spans = decode_value_spans(data)?;
    let mut max_member_len = 0_usize;
    let mut has_possible_int_member = false;
    for member in &spans {
        max_member_len = max_member_len.max(member.byte_len());
        has_possible_int_member |= match member {
            ListpackValueSpan::Integer(_) => true,
            ListpackValueSpan::String(_) => member_could_be_int(member.as_bytes(data)),
        };
    }
    Ok(SetListpackShape {
        member_count: spans.len(),
        max_member_len,
        has_duplicate_member: set_member_spans_have_duplicate(data, &spans),
        has_possible_int_member,
    })
}

#[cfg(test)]
mod tests {

    /// (frankenredis-qj6jn) `entry_len_one_byte_backlen` is the folded form the INTEGER decoder
    /// arms call with a literal width. It must agree with `entry_len_with_backlen` — a genuinely
    /// separate implementation, still on the string arms — for every `data_len` it is allowed to
    /// see, on both the accepting and the refusing paths.
    ///
    /// The ERROR VARIANT is pinned as well as the length, because the folded form drops a
    /// `checked_add` whose only job was to catch `backlen_start == usize::MAX`; that case must
    /// still come back as `TruncatedEntry` from the bounds test, and an equivalence like that is
    /// easy to assert and easy to get wrong.
    #[test]
    fn entry_len_one_byte_backlen_matches_the_general_form_qj6jn() {
        // Byte at index i is i-as-u8, so a correct one-byte backlen sits at exactly one placement
        // per width and every other placement exercises the InvalidBacklen arm.
        let data: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut checked = 0usize;
        for data_len in [0usize, 1, 2, 3, 4, 5, 9, 100, 126, 127] {
            let cursors: [usize; 12] =
                [0, 1, 2, 127, 128, 300, 597, 598, 599, 600, 601, usize::MAX];
            for cursor in cursors {
                assert_eq!(
                    entry_len_one_byte_backlen(&data, cursor, data_len),
                    entry_len_with_backlen(&data, cursor, data_len),
                    "diverged at cursor {cursor}, data_len {data_len}"
                );
                checked += 1;
            }
            assert_eq!(
                entry_len_one_byte_backlen(&[], 0, data_len),
                entry_len_with_backlen(&[], 0, data_len),
                "diverged on an empty buffer at data_len {data_len}"
            );
            checked += 1;
        }
        assert!(checked >= 130, "corpus shrank to {checked} cases");
    }

    /// (frankenredis-qj6jn) `ListpackIntegerBytes` now keeps the renderer's RIGHT-aligned buffer
    /// and a start index instead of relocating the digits to the front behind a length. The
    /// reference below is the OLD construction written out by hand — a second zeroed buffer plus
    /// a `copy_from_slice` — so this compares two independent expressions of the same rule rather
    /// than the implementation against itself.
    ///
    /// It also pins the two properties the change could have broken quietly: `as_slice` must be
    /// byte-identical, and the derived `PartialEq` must still mean "same value", which requires
    /// one value to have exactly ONE representation.
    #[test]
    fn listpack_integer_bytes_right_aligned_matches_the_relocating_form_qj6jn() {
        fn reference(value: i64) -> Vec<u8> {
            let (scratch, start) = crate::decimal_i64_scratch(value);
            let len = scratch.len() - start;
            let mut bytes = [0u8; 20];
            bytes[..len].copy_from_slice(&scratch[start..]);
            bytes[..len].to_vec()
        }

        let mut values: Vec<i64> = vec![
            0,
            i64::MIN,
            i64::MAX,
            i64::MIN + 1,
            i64::MAX - 1,
            -1,
            1,
            10,
            -10,
            99,
            100,
            -99,
            -100,
        ];
        for p in 0..19u32 {
            let base = 10i64.saturating_pow(p);
            values.extend([base - 1, base, base + 1, -base, -(base - 1)]);
        }
        values.extend(-2000i64..2000);
        for v in values {
            let made = ListpackIntegerBytes::new(v);
            assert_eq!(
                made.as_slice(),
                reference(v).as_slice(),
                "right-aligned form diverged from the relocating form for {v}"
            );
            assert_eq!(
                made.as_slice(),
                v.to_string().as_bytes(),
                "rendered decimal is not the canonical text for {v}"
            );
            // One value, one representation — otherwise the derived PartialEq would be
            // comparing padding rather than the number.
            assert_eq!(
                made,
                ListpackIntegerBytes::new(v),
                "representation is not stable"
            );
        }
        assert_ne!(
            ListpackIntegerBytes::new(1),
            ListpackIntegerBytes::new(10),
            "distinct values must not compare equal"
        );
    }
    use super::*;

    // ── (frankenredis-qj6jn) in-place span decode: equivalence oracle ──────────
    //
    // FROZEN copy of the pre-lever `decode_entry_value_span`: it builds the
    // `(ListpackValueSpan, usize)` tuple and returns it, exactly as production did
    // before the entry was decoded straight into the caller's `Vec`. It lives here,
    // not in the crate, so it can never be reached by shipped code and can never be
    // silently "fixed" to match a regression.
    //
    // This is the isomorphism proof for the lever: the two implementations are
    // independent code paths over the same input, so agreement on BOTH the decoded
    // spans and the exact error variant is real evidence, not a tautology. If a
    // later edit changes production's behaviour deliberately, this test fails and
    // forces the change to be argued rather than absorbed.
    fn reference_decode_entry_value_span(
        data: &[u8],
        cursor: usize,
    ) -> Result<(ListpackValueSpan, usize), ListpackError> {
        let first = *data.get(cursor).ok_or(ListpackError::TruncatedEntry)?;

        if first & 0x80 == 0 {
            let value = i64::from(first & 0x7F);
            let entry_len = entry_len_with_backlen(data, cursor, 1)?;
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
            let entry_len = entry_len_with_backlen(data, cursor, 1 + slen)?;
            return Ok((
                ListpackValueSpan::String(narrow_span(start, end)?),
                entry_len,
            ));
        }
        if first & 0xE0 == 0xC0 {
            let second = *data.get(cursor + 1).ok_or(ListpackError::TruncatedEntry)?;
            let raw = (u16::from(first & 0x1F) << 8) | u16::from(second);
            let signed = if raw & 0x1000 != 0 {
                (raw as i64) - 0x2000
            } else {
                raw as i64
            };
            let entry_len = entry_len_with_backlen(data, cursor, 2)?;
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
            let entry_len = entry_len_with_backlen(data, cursor, 2 + slen)?;
            return Ok((
                ListpackValueSpan::String(narrow_span(start, end)?),
                entry_len,
            ));
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
                let entry_len = entry_len_with_backlen(data, cursor, 5 + slen)?;
                Ok((
                    ListpackValueSpan::String(narrow_span(start, end)?),
                    entry_len,
                ))
            }
            0xF1 => {
                if cursor + 3 > data.len() {
                    return Err(ListpackError::TruncatedEntry);
                }
                let raw = i16::from_le_bytes([data[cursor + 1], data[cursor + 2]]);
                let entry_len = entry_len_with_backlen(data, cursor, 3)?;
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
                let entry_len = entry_len_with_backlen(data, cursor, 4)?;
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
                let entry_len = entry_len_with_backlen(data, cursor, 5)?;
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
                let entry_len = entry_len_with_backlen(data, cursor, 9)?;
                Ok((ListpackValueSpan::integer(raw), entry_len))
            }
            _ => Err(ListpackError::InvalidEncoding(first)),
        }
    }

    /// Run one entry through both implementations and require identical outcomes.
    ///
    /// Both sides are normalised to `Result<(Vec<span>, consumed), error>` so ONE
    /// comparison covers everything that could diverge: success-vs-failure, the exact
    /// error variant, the decoded span, the consumed length, AND how many spans were
    /// pushed. The reference pushes exactly one span on success and none on failure by
    /// construction, so comparing the whole vector is what pins the in-place arm to
    /// the same discipline — a rewrite that pushed before validating, or pushed twice,
    /// fails here even though its returned value would look correct.
    fn assert_entry_arms_agree(data: &[u8], cursor: usize) {
        let (want_res, want_spans) = match reference_decode_entry_value_span(data, cursor) {
            Ok((span, consumed)) => (Ok(consumed), vec![span]),
            Err(err) => (Err(err), Vec::new()),
        };
        let mut got_spans: Vec<ListpackValueSpan> = Vec::new();
        let got_res = decode_entry_value_span_into(data, cursor, &mut got_spans);
        assert_eq!(
            (want_res, want_spans),
            (got_res, got_spans),
            "span decode arms diverged at cursor {cursor}"
        );
    }

    /// Every listpack encoding, plus the boundary values of each integer width,
    /// decoded identically by both arms. Hardcoded encodings — not produced by the
    /// encoder under test — so the corpus cannot inherit a bug from it.
    #[test]
    fn inplace_span_decode_matches_reference_across_all_encodings() {
        // A 6-bit string header at its maximum length (63) plus the payload it
        // promises — the boundary between the 6-bit and 12-bit string encodings.
        let mut max6 = vec![0xBFu8];
        max6.extend(std::iter::repeat_n(b'q', 63));

        // (encoding bytes WITHOUT backlen, human label)
        let bodies: Vec<(Vec<u8>, &str)> = vec![
            (vec![0x00], "7-bit uint 0"),
            (vec![0x7F], "7-bit uint 127"),
            (vec![0x80], "6-bit string, empty"),
            (vec![0x83, b'a', b'b', b'c'], "6-bit string 'abc'"),
            (max6, "6-bit string at max length 63"),
            (vec![0xC0, 0x00], "13-bit int 0"),
            (vec![0xDF, 0xFF], "13-bit int -1"),
            (vec![0xD0, 0x00], "13-bit int, sign bit set"),
            (vec![0xE0, 0x00], "12-bit string, empty"),
            (vec![0xE0, 0x02, b'h', b'i'], "12-bit string 'hi'"),
            (vec![0xF1, 0x00, 0x80], "int16 i16::MIN"),
            (vec![0xF1, 0xFF, 0x7F], "int16 i16::MAX"),
            (vec![0xF2, 0x00, 0x00, 0x80], "int24 min"),
            (vec![0xF2, 0xFF, 0xFF, 0x7F], "int24 max"),
            (vec![0xF3, 0x00, 0x00, 0x00, 0x80], "int32 i32::MIN"),
            (vec![0xF3, 0xFF, 0xFF, 0xFF, 0x7F], "int32 i32::MAX"),
            (
                vec![0xF4, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80],
                "int64 i64::MIN",
            ),
            (
                vec![0xF4, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
                "int64 i64::MAX",
            ),
            (vec![0xF0, 0x00, 0x00, 0x00, 0x00], "32-bit string, empty"),
            (
                vec![0xF0, 0x01, 0x00, 0x00, 0x00, b'z'],
                "32-bit string 'z'",
            ),
        ];
        for (body, label) in &bodies {
            // Append the well-formed backlen so the entry is complete.
            let mut entry = body.clone();
            assert!(
                body.len() < 128,
                "test bodies stay in the 1-byte backlen range"
            );
            entry.push(body.len() as u8);
            assert_entry_arms_agree(&entry, 0);
            // Every fixture must actually DECODE — otherwise "the arms agree" would be
            // satisfied by both of them rejecting it, and the corpus would silently
            // stop covering the encoding it names.
            let mut got: Vec<ListpackValueSpan> = Vec::new();
            assert!(
                decode_entry_value_span_into(&entry, 0, &mut got).is_ok(),
                "{label} must decode, not be rejected"
            );
            assert_eq!(got.len(), 1, "{label} must yield exactly one span");
        }
    }

    /// NEGATIVE CASES. Each is malformed in a way that a naive in-place rewrite gets
    /// wrong: a rewrite that pushes the span BEFORE validating the backlen would
    /// leave a phantom entry; one that reorders `narrow_span` ahead of
    /// `entry_len_with_backlen` would report the wrong error variant; one that
    /// forgets a bounds test would panic instead of returning `Err`.
    #[test]
    fn inplace_span_decode_matches_reference_on_malformed_entries() {
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (vec![], "empty buffer"),
            (vec![0x00], "7-bit uint with the backlen byte missing"),
            (vec![0x00, 0x09], "7-bit uint with a WRONG backlen"),
            (vec![0x83, b'a'], "6-bit string truncated mid-payload"),
            (
                vec![0x83, b'a', b'b', b'c'],
                "6-bit string, backlen missing",
            ),
            (
                vec![0x83, b'a', b'b', b'c', 0x02],
                "6-bit string, wrong backlen",
            ),
            (vec![0xC0], "13-bit int missing its second byte"),
            (vec![0xE0], "12-bit string missing its length byte"),
            (vec![0xE0, 0xFF], "12-bit string length past the buffer"),
            (
                vec![0xF0, 0xFF, 0xFF, 0xFF, 0xFF],
                "32-bit string len u32::MAX",
            ),
            (vec![0xF0, 0x01, 0x00], "32-bit string header truncated"),
            (vec![0xF1, 0x00], "int16 truncated"),
            (vec![0xF2, 0x00, 0x00], "int24 truncated"),
            (vec![0xF3, 0x00, 0x00, 0x00], "int32 truncated"),
            (vec![0xF4, 0x00], "int64 truncated"),
            (vec![0xF5], "invalid encoding byte 0xF5"),
            (vec![0xFF], "the EOF terminator, decoded as an entry"),
        ];
        for (data, label) in &cases {
            assert_entry_arms_agree(data, 0);
            let mut sink: Vec<ListpackValueSpan> = Vec::new();
            assert!(
                decode_entry_value_span_into(data, 0, &mut sink).is_err(),
                "{label} must be rejected, not decoded"
            );
            assert!(sink.is_empty(), "{label} must leave the sink untouched");
        }
        // Reading past the end of a buffer must be an error, never a panic.
        let data = [0x83u8, b'a', b'b', b'c', 0x04];
        for cursor in 0..=data.len() + 2 {
            assert_entry_arms_agree(&data, cursor);
        }
    }

    proptest::proptest! {
        /// Arbitrary bytes at an arbitrary cursor: the two arms must never diverge,
        /// and neither may panic. This is what covers the error-PRECEDENCE orderings
        /// the hand-written cases above cannot enumerate.
        #[test]
        fn inplace_span_decode_matches_reference_on_arbitrary_bytes(
            data in proptest::collection::vec(proptest::num::u8::ANY, 0..64usize),
            cursor in 0..70usize,
        ) {
            assert_entry_arms_agree(&data, cursor);
        }

        /// Whole-listpack level: a real encoded listpack decodes to the same span
        /// vector through the production entry point as through the reference loop.
        #[test]
        fn inplace_value_spans_match_reference_loop_on_encoded_listpacks(
            entries in proptest::collection::vec(
                proptest::collection::vec(proptest::num::u8::ANY, 0..40usize),
                0..24usize,
            ),
        ) {
            let slices: Vec<&[u8]> = entries.iter().map(|e| e.as_slice()).collect();
            let Some(blob) = crate::encode_listpack_strings_blob(&slices) else {
                return Ok(());
            };
            // Reference: the pre-lever loop, verbatim.
            let mut want: Vec<ListpackValueSpan> = Vec::new();
            let (total_bytes, _) = parse_header(&blob).expect("encoder emits a valid header");
            let end = (total_bytes as usize) - 1;
            let mut cursor = LISTPACK_HEADER_SIZE;
            while cursor < end {
                let (value, consumed) =
                    reference_decode_entry_value_span(&blob, cursor).expect("valid entry");
                want.push(value);
                cursor += consumed;
            }
            let got = decode_value_spans(&blob).expect("valid listpack decodes");
            proptest::prop_assert_eq!(&got, &want);
            // And the spans still resolve to the bytes that went in.
            for (span, original) in got.iter().zip(entries.iter()) {
                proptest::prop_assert_eq!(span.as_bytes(&blob), original.as_slice());
            }
        }
    }

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

    /// (frankenredis-qj6jn) `byte_len` and `first_byte` answer from the span alone, where the list
    /// restore fold used to materialize a subslice through `as_bytes` and then ask it. They must
    /// agree with `as_bytes` for every span a decoder can produce — the EMPTY string entry
    /// included, which is the one case where `first_byte` must say `None` rather than index.
    ///
    /// The fixture is assembled from the encoding builders and read back through the real decoder,
    /// so the spans under test are the ones production would see rather than hand-built ones.
    #[test]
    fn span_byte_len_and_first_byte_match_as_bytes_qj6jn() {
        let parts: Vec<Vec<u8>> = vec![
            entry_6bit_str(b""),
            entry_6bit_str(b"a"),
            entry_6bit_str(b"0123"),
            entry_6bit_str(b"vvvvvvvvvv"),
            entry_6bit_str(&[b'z'; 60]),
            entry_7bit_uint(0),
            entry_7bit_uint(9),
            entry_7bit_uint(127),
            entry_13bit_int(-4096),
            entry_13bit_int(4095),
            entry_32bit_int(i32::MIN),
            entry_32bit_int(2_147_483_647),
        ];
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        let lp = assemble(&refs);
        let spans = decode_value_spans(&lp).expect("the fixture must decode");
        assert_eq!(spans.len(), parts.len(), "fixture lost an entry");
        for span in &spans {
            let via_slice = span.as_bytes(&lp);
            assert_eq!(span.byte_len(), via_slice.len(), "byte_len disagreed");
            assert_eq!(
                span.first_byte(&lp),
                via_slice.first().copied(),
                "first_byte disagreed for {:?}",
                String::from_utf8_lossy(&via_slice[..via_slice.len().min(12)])
            );
        }
        // Both encodings must actually be present, or this proves half of what it claims.
        assert!(
            spans.iter().any(ListpackValueSpan::is_string_encoded),
            "fixture produced no string span"
        );
        assert!(
            !spans.iter().all(ListpackValueSpan::is_string_encoded),
            "fixture produced no integer span"
        );
        // And the empty entry must be the one that exercises the None arm.
        assert!(
            spans.iter().any(|s| s.first_byte(&lp).is_none()),
            "fixture never exercised the empty-element arm"
        );
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

    /// (frankenredis-w08xv) The sorted-set RESTORE path takes an
    /// integer-encoded score straight off the span instead of parsing the
    /// decimal text this decoder rendered from it. That substitution is only
    /// sound if BOTH routes land on the same `f64`, including where an i64 no
    /// longer fits one exactly — `i64 as f64` and a correctly-rounded
    /// `str::parse::<f64>` both round to nearest, so they must agree even past
    /// 2^53. This pins that, and pins the rendered bytes alongside so a change
    /// to either half cannot silently drift from the other.
    #[test]
    fn integer_span_value_and_rendered_bytes_agree_as_f64() {
        for value in [
            0i64,
            1,
            -1,
            42,
            -42,
            9_007_199_254_740_992, // 2^53, exactly representable
            9_007_199_254_740_993, // 2^53 + 1, NOT exactly representable
            -9_007_199_254_740_993,
            i64::MAX,
            i64::MIN,
        ] {
            let span = ListpackValueSpan::integer(value);
            assert_eq!(span.as_i64(), Some(value), "as_i64 for {value}");

            let rendered = span.as_bytes(&[]);
            assert_eq!(
                rendered,
                value.to_string().as_bytes(),
                "rendered decimal for {value}"
            );

            let via_parse: f64 = std::str::from_utf8(rendered)
                .expect("decimal render is ascii")
                .parse()
                .expect("decimal render parses");
            #[allow(clippy::cast_precision_loss)]
            let via_value = value as f64;
            assert_eq!(
                via_value.to_bits(),
                via_parse.to_bits(),
                "the two score routes must produce the identical f64 for {value}"
            );
        }

        // A string-encoded entry has no integer to take, so the consumer must
        // fall back to parsing — that is the branch fractional scores use.
        let lp = assemble(&[&entry_6bit_str(b"1.5")]);
        let spans = decode_value_spans(&lp).expect("decodes");
        assert_eq!(spans[0].as_i64(), None, "string spans have no integer");
        assert_eq!(spans[0].as_bytes(&lp), b"1.5");
    }

    /// (frankenredis-w08xv) The zset decode must agree with the general one on
    /// every observable: the member bytes, the score value, and the rejections.
    /// It differs only in NOT rendering scores, so this compares it against
    /// `decode_value_spans` rather than against hand-written expectations —
    /// the general decoder is the reference the whole RESTORE path already uses.
    #[test]
    fn zset_span_decode_agrees_with_the_general_decoder() {
        // Integer-encoded scores (the path that skips rendering), a
        // string-encoded fractional score, and an integer-encoded MEMBER whose
        // decimal bytes must still be produced.
        let lp = assemble(&[
            &entry_6bit_str(b"alpha"),
            &entry_7bit_uint(7),
            &entry_7bit_uint(42),
            &entry_6bit_str(b"1.5"),
            &entry_6bit_str(b"omega"),
            &entry_13bit_int(-9),
        ]);

        let general = decode_value_spans(&lp).expect("general decode");
        let zset = decode_zset_spans_and_scores(&lp).expect("zset decode");
        assert_eq!(zset.len() * 2, general.len(), "one pair per two entries");

        for (i, (member, score)) in zset.iter().enumerate() {
            assert_eq!(
                member.as_bytes(&lp),
                general[i * 2].as_bytes(&lp),
                "member {i} must match the general decoder byte for byte"
            );
            let reference: f64 = std::str::from_utf8(general[i * 2 + 1].as_bytes(&lp))
                .expect("score text is ascii")
                .parse()
                .expect("score text parses");
            assert_eq!(
                score.to_bits(),
                reference.to_bits(),
                "score {i} must equal what parsing the rendered decimal gives"
            );
        }
        assert_eq!(zset[0].1, 7.0);
        assert_eq!(zset[1].1, 1.5);
        assert_eq!(zset[2].1, -9.0);
        assert_eq!(
            zset[1].0.as_bytes(&lp),
            b"42",
            "integer members still render"
        );

        // An ODD element count is not a valid zset listpack and must be rejected
        // rather than silently dropping the dangling member.
        let odd = assemble(&[
            &entry_6bit_str(b"alpha"),
            &entry_7bit_uint(7),
            &entry_6bit_str(b"x"),
        ]);
        assert!(
            decode_zset_spans_and_scores(&odd).is_err(),
            "odd element count must be rejected"
        );
    }

    #[test]
    fn decode_string_ranges_returns_none_for_integer_node() {
        let lp = assemble(&[&entry_6bit_str(b"alpha"), &entry_7bit_uint(42)]);
        assert_eq!(decode_string_ranges_if_all_strings(&lp).unwrap(), None);
    }

    /// (frankenredis-33832) The span's SIZE is its cost: `decode_value_spans`
    /// pushes one per listpack entry and pays ~1 instruction per byte of it in
    /// `ptr::write`. There is a `const _: () = assert!(..)` on the type that is
    /// the real gate; this test exists so the reason shows up in a test failure
    /// and not only in a compile error.
    #[test]
    fn listpack_value_span_stays_small() {
        assert!(
            std::mem::size_of::<ListpackValueSpan>() <= 32,
            "ListpackValueSpan grew to {} bytes; decode_value_spans pays about one \
             instruction per byte per listpack entry, so this is a direct RESTORE \
             regression (was 40 before frankenredis-33832)",
            std::mem::size_of::<ListpackValueSpan>()
        );
        // Report the achieved size so the measurement can be checked against it.
        println!(
            "ListpackValueSpan = {} bytes",
            std::mem::size_of::<ListpackValueSpan>()
        );
    }

    #[test]
    fn retained_listpack_spans_keep_integer_arena_with_compact_string_entries_gvm6z() {
        let blob = crate::encode_listpack_strings_blob(&[
            b"alpha".as_slice(),
            b"-17".as_slice(),
            b"-9223372036854775808".as_slice(),
            b"9223372036854775807".as_slice(),
            b"omega".as_slice(),
        ])
        .expect("fixture encodes");
        let retained = decode_retained_listpack_spans(&blob).expect("fixture decodes");
        let generic = decode_value_spans(&blob).expect("generic decoder accepts fixture");

        assert!(
            std::mem::size_of::<RetainedListpackValueSpan>() <= 12,
            "retained span is {} bytes",
            std::mem::size_of::<RetainedListpackValueSpan>()
        );
        assert_eq!(retained.entries().len(), generic.len());
        let values: Vec<&[u8]> = retained
            .entries()
            .iter()
            .map(|span| span.as_bytes(&blob, retained.integer_bytes()))
            .collect();
        let expected: Vec<&[u8]> = generic.iter().map(|span| span.as_bytes(&blob)).collect();
        assert_eq!(values, expected, "retained and generic decoders diverged");
        assert_eq!(
            retained.integer_bytes(),
            b"-17-92233720368547758089223372036854775807"
        );
        assert!(retained.entries()[0].is_string_encoded());
        assert!(!retained.entries()[1].is_string_encoded());
        assert_eq!(
            retained.entries()[2].byte_len(),
            20,
            "i64::MIN must retain every decimal digit"
        );
        assert_eq!(
            retained.entries()[3].byte_len(),
            19,
            "i64::MAX must retain every decimal digit"
        );
    }

    /// The `usize -> u32` narrowing must REJECT, never truncate.
    ///
    /// This is the one path a real listpack cannot reach — it needs a buffer
    /// longer than `u32::MAX`, which no test is going to allocate — so it is
    /// tested directly on the helper. Under a plain `as` cast the first case
    /// below would silently become `0..0` and `as_bytes` would return the wrong
    /// bytes with no error raised anywhere.
    #[test]
    fn narrow_span_rejects_offsets_beyond_u32_instead_of_truncating() {
        let over = u32::MAX as usize + 1;
        assert_eq!(
            narrow_span(0, over),
            Err(ListpackError::StringLengthOverflow),
            "an end offset past u32::MAX must be rejected, not wrapped to 0"
        );
        assert_eq!(
            narrow_span(over, over + 8),
            Err(ListpackError::StringLengthOverflow),
            "a start offset past u32::MAX must be rejected"
        );
        // The boundary itself is representable and must still be accepted, so the
        // guard cannot be "reject everything large" and pass the cases above.
        assert_eq!(
            narrow_span(0, u32::MAX as usize),
            Ok(0..u32::MAX),
            "u32::MAX is representable and must be accepted"
        );
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
        pairs
            .iter()
            .map(|(m, s)| (m.clone(), s.to_bits()))
            .collect()
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
            &entry_6bit_str(b"-2.5"),  // score -2.5 (str)
            &entry_7bit_uint(7),       // integer MEMBER
            &entry_6bit_str(b"1.375"), // exact binary fraction (11/8)
        ]);
        let new = decode_zset_listpack_pairs(&lp).expect("new decode");
        let orig = decode_zset_listpack_pairs_orig(&lp).expect("orig decode");
        assert_eq!(zpair_bits(&new), zpair_bits(&orig), "new vs orig diverged");
        assert_eq!(new[3].0, b"m003");
        assert!(new[3].1.is_infinite() && new[3].1 > 0.0);
        assert_eq!(new[6].0, b"7"); // int member 7 renders to "7"
        assert_eq!(new[6].1, 1.375);
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
        assert_eq!(
            decode_zset_listpack_pairs(&odd),
            Err(ListpackError::ElementCountMismatch),
            "a known odd header must fail before member materialization"
        );
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
    /// (frankenredis-gvm6z) THE NUMBER THAT CHOOSES THE DESIGN, and it needs no timed run.
    ///
    /// gvm6z wants `ListpackValueSpan` shrunk from <=32 bytes by making the integer variant
    /// indirect. The ledger row on it records that the two viable designs are decided by ONE
    /// quantity -- how often an entry is INTEGER-encoded -- because the cheap design
    /// (`Cow<[u8]>` from a raw `i64`) costs a heap allocation on every integer read while
    /// costing nothing on a string read.
    ///
    /// That quantity is a property of the DATA, not of the clock, so it is pinned here rather
    /// than measured on a busy host. fr's encoder probes every entry with
    /// `parse_listpack_integer`, so any element whose bytes parse as an integer is stored as
    /// one -- which makes the fraction swing from 0 to 100 pct on realistic shapes:
    ///
    ///     name list      0/6   integer   a `Cow` design pays NOTHING here
    ///     counter hash   3/6   integer   half the entries allocate per read
    ///     id list        6/6   integer   every read allocates
    ///
    /// So there is no single answer, and a design chosen from one shape is wrong for another.
    /// That is the finding: the `Cow` form is not safe to land as a blanket change, and the
    /// stored-side-buffer design is the one that is workload-independent.
    #[test]
    fn integer_entry_fraction_is_workload_dependent_gvm6z() {
        fn split(entries: &[&[u8]]) -> (usize, usize) {
            let blob = crate::encode_listpack_strings_blob(entries).expect("fixture encodes");
            let spans = decode_value_spans(&blob).expect("fixture decodes");
            assert_eq!(spans.len(), entries.len(), "one span per entry");
            let ints = spans
                .iter()
                .filter(|s| matches!(s, ListpackValueSpan::Integer(_)))
                .count();
            (ints, spans.len())
        }

        // A list of names: nothing parses as an integer.
        assert_eq!(
            split(&[b"alice", b"bob", b"carol", b"dave", b"erin", b"frank"]),
            (0, 6),
            "no entry of a name list is integer-encoded"
        );

        // A counter hash, field/value interleaved as the listpack stores it: the VALUES are
        // integers and the FIELDS are not, so exactly half.
        assert_eq!(
            split(&[b"hits", b"41", b"misses", b"7", b"evictions", b"0"]),
            (3, 6),
            "a counter hash integer-encodes its values and not its fields"
        );

        // A list of ids: every entry is an integer.
        assert_eq!(
            split(&[
                b"1",
                b"2",
                b"3",
                b"100",
                b"-9223372036854775808",
                b"9223372036854775807"
            ]),
            (6, 6),
            "every entry of an id list is integer-encoded"
        );

        // The negative case gvm6z names explicitly: i64::MIN renders to 20 bytes, and any
        // design that NARROWS the inline buffer instead of making it indirect truncates it.
        // Pinned here so that trap fails loudly whichever design is eventually taken.
        let blob = crate::encode_listpack_strings_blob(&[
            b"-9223372036854775808".as_slice(),
            b"9223372036854775807".as_slice(),
        ])
        .expect("extremes encode");
        let spans = decode_value_spans(&blob).expect("extremes decode");
        assert_eq!(spans[0].as_bytes(&blob), b"-9223372036854775808");
        assert_eq!(spans[1].as_bytes(&blob), b"9223372036854775807");
        assert_eq!(spans[0].byte_len(), 20, "i64::MIN renders to 20 bytes");
    }
}
