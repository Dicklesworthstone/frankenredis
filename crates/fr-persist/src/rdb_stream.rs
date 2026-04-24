//! Upstream-compatible RDB stream record decoder.
//!
//! Handles the type-byte families:
//!   * RDB_TYPE_STREAM_LISTPACKS       = 15  (Redis ≤ 6.2)
//!   * RDB_TYPE_STREAM_LISTPACKS_2     = 19  (+ first/max-deleted IDs + entries_added + per-consumer seen_time)
//!   * RDB_TYPE_STREAM_LISTPACKS_3     = 21  (+ per-consumer active_time)
//!
//! Entry decoding (br-frankenredis-hjub) is implemented: each radix-tree
//! listpack is unpacked per upstream's `t_stream.c` layout (master entry +
//! delta-encoded items with same-fields reuse) and returned as
//! `StreamEntry` tuples in `RdbValue::Stream`. Tombstoned entries (flag
//! bit 1) are dropped. Consumer-group population (groups/PEL) is
//! byte-parsed but not reified — that lands in br-frankenredis-qi6z.
//!
//! (br-frankenredis-hjub)

use crate::listpack::{ListpackEntry, ListpackError, decode_listpack};
use crate::{RdbValue, StreamEntry};

use super::{rdb_decode_length, rdb_decode_string};

/// Upstream stream entry flags (matches upstream's `streamFlags`).
const STREAM_ITEM_FLAG_DELETED: i64 = 1;
const STREAM_ITEM_FLAG_SAMEFIELDS: i64 = 2;

/// Upstream-layout decode failure modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamStreamError {
    /// Length-encoded integer could not be parsed.
    InvalidLength,
    /// rdb_decode_string returned None for a required string field.
    InvalidString,
    /// The nodekey (master ID) wasn't the expected 16-byte stream ID.
    InvalidNodekeyLength,
    /// Unexpected type byte (not 15/19/21).
    UnsupportedTypeByte(u8),
    /// The listpack blob inside a radix node failed to parse.
    InvalidListpack(ListpackError),
    /// A required listpack element was missing (short listpack for stream layout).
    ShortListpackEntries,
    /// A listpack element expected to be an integer was a string.
    ExpectedListpackInteger,
    /// A listpack element expected to be a byte-string was an integer.
    ExpectedListpackString,
    /// The master field count or per-entry field count is negative or > isize::MAX.
    InvalidFieldCount,
    /// The `lp_count` trailer disagreed with how many elements the entry consumed.
    InconsistentEntryTrailer,
}

impl From<ListpackError> for UpstreamStreamError {
    fn from(e: ListpackError) -> Self {
        UpstreamStreamError::InvalidListpack(e)
    }
}

/// Decode an upstream-format stream record starting at `data[0]`,
/// assuming the leading type byte has already been consumed and the
/// key has already been parsed by the caller. Returns the reconstructed
/// `RdbValue::Stream` and the number of bytes consumed.
pub(crate) fn decode_upstream_stream_skeleton(
    type_byte: u8,
    data: &[u8],
) -> Result<(RdbValue, usize), UpstreamStreamError> {
    let is_v2_or_later = match type_byte {
        crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS => false,
        crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2 => true,
        crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3 => true,
        other => return Err(UpstreamStreamError::UnsupportedTypeByte(other)),
    };
    let is_v3 = type_byte == crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3;

    let mut cursor = 0usize;
    let mut entries: Vec<StreamEntry> = Vec::new();

    // (1) Listpacks count.
    let (listpacks_count, c) =
        rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
    cursor += c;

    // (2) For each radix-tree pair: nodekey (16-byte streamID) + listpack blob.
    for _ in 0..listpacks_count {
        let (nodekey, c1) =
            rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
        if nodekey.len() != 16 {
            return Err(UpstreamStreamError::InvalidNodekeyLength);
        }
        let master_ms = u64::from_be_bytes(nodekey[0..8].try_into().unwrap());
        let master_seq = u64::from_be_bytes(nodekey[8..16].try_into().unwrap());
        cursor += c1;
        let (lp_bytes, c2) =
            rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
        cursor += c2;
        let lp = decode_listpack(&lp_bytes)?;
        decode_stream_listpack(&lp, master_ms, master_seq, &mut entries)?;
    }

    // (3) Stream length (total entry count).
    let (_length, c) =
        rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
    cursor += c;

    // (4) last_id.ms, last_id.seq (always present).
    let (last_id_ms, c) =
        rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
    cursor += c;
    let (last_id_seq, c) =
        rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
    cursor += c;

    // (5) v2/v3 extras: first_id, max_deleted_id, entries_added.
    if is_v2_or_later {
        for _ in 0..5 {
            let (_v, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
        }
    }

    // (6) Number of consumer groups.
    let (groups_count, c) =
        rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
    cursor += c;

    // (7) For each group: name, last-delivered-id (ms,seq), entries_read (v2+),
    //     PEL count + entries, consumer count + per-consumer fields.
    //     Byte-skip only — consumer-group reification is br-frankenredis-qi6z.
    for _ in 0..groups_count {
        let (_name, c) =
            rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
        cursor += c;
        for _ in 0..2 {
            let (_v, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
        }
        if is_v2_or_later {
            let (_v, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
        }
        let (pel_count, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        for _ in 0..pel_count {
            if cursor + 16 + 8 > data.len() {
                return Err(UpstreamStreamError::InvalidLength);
            }
            cursor += 16; // stream-id
            cursor += 8; // delivery_time_ms (u64 LE)
            let (_delivery_count, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
        }
        let (consumers_count, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        for _ in 0..consumers_count {
            let (_cname, c) =
                rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
            cursor += c;
            if is_v2_or_later {
                if cursor + 8 > data.len() {
                    return Err(UpstreamStreamError::InvalidLength);
                }
                cursor += 8;
            }
            if is_v3 {
                if cursor + 8 > data.len() {
                    return Err(UpstreamStreamError::InvalidLength);
                }
                cursor += 8;
            }
            let (cpel_count, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
            for _ in 0..cpel_count {
                if cursor + 16 > data.len() {
                    return Err(UpstreamStreamError::InvalidLength);
                }
                cursor += 16;
            }
        }
    }

    let watermark = Some((last_id_ms as u64, last_id_seq as u64));
    let value = RdbValue::Stream(entries, watermark, Vec::new());
    Ok((value, cursor))
}

/// Decode one macro-node listpack into (master_ms, master_seq)-relative
/// entries and append each live (non-tombstoned) entry to `out`.
///
/// Layout recap (see `legacy_redis_code/redis/src/t_stream.c`):
///
///   master: [count, deleted, master_field_count, *master_fields, 0]
///   per entry: [flags, ms_delta, seq_delta,
///               (field_count, *field_names)?,   ; when SAMEFIELDS is unset
///               *values,                        ; master_field_count of them
///               lp_count]
fn decode_stream_listpack(
    lp: &[ListpackEntry],
    master_ms: u64,
    master_seq: u64,
    out: &mut Vec<StreamEntry>,
) -> Result<(), UpstreamStreamError> {
    let mut idx = 0usize;
    let _count = take_int(lp, &mut idx)?;
    let _deleted = take_int(lp, &mut idx)?;
    let master_field_count = take_usize(lp, &mut idx)?;
    let mut master_fields: Vec<Vec<u8>> = Vec::with_capacity(master_field_count);
    for _ in 0..master_field_count {
        master_fields.push(take_string(lp, &mut idx)?);
    }
    // Master terminator: integer 0.
    let terminator = take_int(lp, &mut idx)?;
    if terminator != 0 {
        return Err(UpstreamStreamError::InconsistentEntryTrailer);
    }

    while idx < lp.len() {
        let flags = take_int(lp, &mut idx)?;
        let ms_delta = take_int(lp, &mut idx)?;
        let seq_delta = take_int(lp, &mut idx)?;
        let same_fields = (flags & STREAM_ITEM_FLAG_SAMEFIELDS) != 0;
        let deleted = (flags & STREAM_ITEM_FLAG_DELETED) != 0;

        let field_count = if same_fields {
            master_field_count
        } else {
            take_usize(lp, &mut idx)?
        };

        let mut fields: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(field_count);
        if same_fields {
            for master_name in master_fields.iter().take(field_count) {
                let value = take_string(lp, &mut idx)?;
                fields.push((master_name.clone(), value));
            }
        } else {
            for _ in 0..field_count {
                let name = take_string(lp, &mut idx)?;
                let value = take_string(lp, &mut idx)?;
                fields.push((name, value));
            }
        }

        // lp_count trailer: total listpack elements from (flags) through the
        // last value. We don't validate the exact number because our
        // forward walk already pinned it; we only confirm it's present and
        // non-negative.
        let lp_count = take_int(lp, &mut idx)?;
        if lp_count < 0 {
            return Err(UpstreamStreamError::InconsistentEntryTrailer);
        }

        if deleted {
            continue;
        }
        let ms = combine_u64_i64(master_ms, ms_delta);
        let seq = combine_u64_i64(master_seq, seq_delta);
        out.push((ms, seq, fields));
    }
    Ok(())
}

fn take_int(lp: &[ListpackEntry], idx: &mut usize) -> Result<i64, UpstreamStreamError> {
    let v = lp
        .get(*idx)
        .ok_or(UpstreamStreamError::ShortListpackEntries)?;
    *idx += 1;
    match v {
        ListpackEntry::Integer(n) => Ok(*n),
        ListpackEntry::String(_) => Err(UpstreamStreamError::ExpectedListpackInteger),
    }
}

fn take_usize(lp: &[ListpackEntry], idx: &mut usize) -> Result<usize, UpstreamStreamError> {
    let n = take_int(lp, idx)?;
    if n < 0 {
        return Err(UpstreamStreamError::InvalidFieldCount);
    }
    usize::try_from(n).map_err(|_| UpstreamStreamError::InvalidFieldCount)
}

fn take_string(lp: &[ListpackEntry], idx: &mut usize) -> Result<Vec<u8>, UpstreamStreamError> {
    let v = lp
        .get(*idx)
        .ok_or(UpstreamStreamError::ShortListpackEntries)?;
    *idx += 1;
    match v {
        ListpackEntry::String(bytes) => Ok(bytes.clone()),
        // Upstream writes field names + values via lpAppend; integer values
        // get packed as LP_ENCODING_*_INT but were byte-strings on the
        // write side (stream arg processing calls lpAppend, not
        // lpAppendInteger, for field/value pairs). So integers here
        // should not occur for user-visible fields — but in practice an
        // integer-looking value CAN be packed as an int. Match upstream's
        // listpackGetValue which returns a decimal-stringified integer.
        ListpackEntry::Integer(n) => Ok(n.to_string().into_bytes()),
    }
}

/// Apply a signed delta to an unsigned 64-bit base, wrapping on overflow.
/// Upstream deltas are non-negative in practice (entry IDs monotonically
/// increase within a macro node), so we use wrapping add for robustness
/// against corrupted inputs rather than silently truncating.
fn combine_u64_i64(base: u64, delta: i64) -> u64 {
    if delta >= 0 {
        base.wrapping_add(delta as u64)
    } else {
        base.wrapping_sub(delta.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listpack::{LISTPACK_EOF, LISTPACK_HEADER_SIZE};
    use crate::{UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, rdb_encode_length};

    // ── Listpack byte builders ──────────────────────────────────────
    //
    // These build upstream-compatible listpack bytes for test inputs.
    // See `listpack.rs` for decoder tests of these primitives.

    /// 7-bit unsigned integer listpack entry (value in 0..=127).
    /// Encoding byte IS the value; single-byte backlen = 1.
    fn lp_u7(value: u8) -> Vec<u8> {
        assert!(value <= 0x7F);
        vec![value, 1]
    }

    /// 16-bit signed integer listpack entry (3-byte body + 1 backlen byte).
    fn lp_i16(value: i16) -> Vec<u8> {
        let bytes = value.to_le_bytes();
        // data_len = 3 fits in the single-byte backlen range.
        vec![0xF1, bytes[0], bytes[1], 3]
    }

    /// 6-bit-length byte-string listpack entry (length in 0..=63). Produces
    /// `1 + len` body bytes followed by a single backlen byte equal to the
    /// data length.
    fn lp_str(bytes: &[u8]) -> Vec<u8> {
        assert!(bytes.len() <= 63);
        let data_len = 1 + bytes.len();
        assert!(data_len <= 127);
        let mut out = Vec::with_capacity(data_len + 1);
        out.push(0x80 | (bytes.len() as u8));
        out.extend_from_slice(bytes);
        out.push(data_len as u8);
        out
    }

    fn assemble_listpack(entries: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = entries.iter().flat_map(|e| e.iter().copied()).collect();
        let total_bytes = (LISTPACK_HEADER_SIZE + payload.len() + 1) as u32;
        let num_elements = entries.len().min(u16::MAX as usize) as u16;
        let mut out = Vec::with_capacity(total_bytes as usize);
        out.extend_from_slice(&total_bytes.to_le_bytes());
        out.extend_from_slice(&num_elements.to_le_bytes());
        out.extend_from_slice(&payload);
        out.push(LISTPACK_EOF);
        out
    }

    fn streamid_bytes(ms: u64, seq: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(&ms.to_be_bytes());
        v.extend_from_slice(&seq.to_be_bytes());
        v
    }

    // ── rdb_encode_string shim ──────────────────────────────────────
    //
    // The upstream type-15 stream envelope uses `rdbSaveRawString` for
    // nodekey and listpack bytes. Our `rdb_encode_string` already matches
    // that shape for lengths < 64 → plain length-prefixed bytes.
    //
    // Tests below use lengths well under that threshold.

    fn rdb_encode_raw_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
        rdb_encode_length(buf, bytes.len());
        buf.extend_from_slice(bytes);
    }

    /// Build the minimal-but-valid upstream type-15 payload for an
    /// empty stream (no listpacks, no groups) with given last-id.
    fn build_empty_type15(last_ms: u64, last_seq: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 0); // listpacks_count
        rdb_encode_length(&mut buf, 0); // stream length
        rdb_encode_length(&mut buf, last_ms as usize); // last_id.ms
        rdb_encode_length(&mut buf, last_seq as usize); // last_id.seq
        rdb_encode_length(&mut buf, 0); // groups_count
        buf
    }

    /// Master listpack with a single non-deleted, non-same-fields entry.
    ///
    /// Master fields: ["f1", "f2"]; then one entry with flags=0, ms_delta=5,
    /// seq_delta=0, field_count=2, fields=("f1","V1"), ("f2","V2"),
    /// lp_count=10.
    fn build_unique_fields_listpack() -> Vec<u8> {
        let entries: Vec<Vec<u8>> = vec![
            lp_u7(1),      // count = 1
            lp_u7(0),      // deleted = 0
            lp_u7(2),      // master_field_count = 2
            lp_str(b"f1"), // master field 1
            lp_str(b"f2"), // master field 2
            lp_u7(0),      // master terminator
            lp_u7(0),      // entry.flags
            lp_u7(5),      // ms_delta
            lp_u7(0),      // seq_delta
            lp_u7(2),      // per-entry field_count
            lp_str(b"f1"),
            lp_str(b"V1"),
            lp_str(b"f2"),
            lp_str(b"V2"),
            lp_u7(10), // lp_count trailer
        ];
        assemble_listpack(&entries)
    }

    /// Master listpack with two entries: one same-fields + one deleted.
    fn build_samefields_and_deleted_listpack() -> Vec<u8> {
        let entries: Vec<Vec<u8>> = vec![
            lp_u7(2),        // count = 2 (live entries)
            lp_u7(1),        // deleted = 1
            lp_u7(1),        // master_field_count = 1
            lp_str(b"only"), // master field 1
            lp_u7(0),        // master terminator
            // Entry 1: same-fields live entry.
            lp_u7(STREAM_ITEM_FLAG_SAMEFIELDS as u8), // flags=2
            lp_u7(0),                                 // ms_delta=0
            lp_u7(1),                                 // seq_delta=1
            lp_str(b"A"),                             // value for master field 0
            lp_u7(6),                                 // lp_count
            // Entry 2: deleted + same-fields.
            lp_u7((STREAM_ITEM_FLAG_SAMEFIELDS | STREAM_ITEM_FLAG_DELETED) as u8), // flags=3
            lp_u7(0),                                                              // ms_delta=0
            lp_u7(2),                                                              // seq_delta=2
            lp_str(b"X"), // value (still present for tombstone)
            lp_u7(6),     // lp_count
            // Entry 3: live, unique fields (flags=0), using i16 for a
            // larger seq delta.
            lp_u7(0),    // flags=0
            lp_u7(0),    // ms_delta=0
            lp_i16(300), // seq_delta=300
            lp_u7(1),    // per-entry field_count
            lp_str(b"only"),
            lp_str(b"B"),
            lp_u7(7), // lp_count
        ];
        assemble_listpack(&entries)
    }

    fn build_type15_payload_with_listpack(
        lp_bytes: &[u8],
        master_ms: u64,
        master_seq: u64,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 1); // one listpack pair
        rdb_encode_raw_bytes(&mut buf, &streamid_bytes(master_ms, master_seq));
        rdb_encode_raw_bytes(&mut buf, lp_bytes);
        rdb_encode_length(&mut buf, 1); // length
        rdb_encode_length(&mut buf, master_ms as usize); // last_id.ms
        rdb_encode_length(&mut buf, master_seq as usize); // last_id.seq
        rdb_encode_length(&mut buf, 0); // groups_count
        buf
    }

    #[test]
    fn decode_empty_type15_returns_skeleton_stream_with_watermark() {
        let payload = build_empty_type15(12345, 7);
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
                .expect("decode skeleton");
        assert_eq!(consumed, payload.len());
        match value {
            RdbValue::Stream(entries, watermark, groups) => {
                assert!(entries.is_empty());
                assert!(groups.is_empty());
                assert_eq!(watermark, Some((12345, 7)));
            }
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_unsupported_type_byte() {
        let payload = build_empty_type15(0, 0);
        let err = decode_upstream_stream_skeleton(22, &payload).unwrap_err();
        assert_eq!(err, UpstreamStreamError::UnsupportedTypeByte(22));
    }

    #[test]
    fn decode_rejects_nodekey_of_wrong_length() {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 1); // one listpack pair
        // nodekey with length 10 instead of 16.
        rdb_encode_length(&mut buf, 10);
        buf.extend_from_slice(&[0u8; 10]);
        let err =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &buf).unwrap_err();
        assert_eq!(err, UpstreamStreamError::InvalidNodekeyLength);
    }

    #[test]
    fn decode_single_unique_fields_entry() {
        let lp = build_unique_fields_listpack();
        let payload = build_type15_payload_with_listpack(&lp, 1000, 0);
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
                .expect("decode entry");
        assert_eq!(consumed, payload.len());
        match value {
            RdbValue::Stream(entries, watermark, groups) => {
                assert!(groups.is_empty());
                assert_eq!(watermark, Some((1000, 0)));
                assert_eq!(entries.len(), 1);
                let (ms, seq, fields) = &entries[0];
                assert_eq!(*ms, 1005);
                assert_eq!(*seq, 0);
                assert_eq!(
                    fields,
                    &vec![
                        (b"f1".to_vec(), b"V1".to_vec()),
                        (b"f2".to_vec(), b"V2".to_vec()),
                    ]
                );
            }
            other => panic!("expected Stream, got {other:?}"),
        }
    }

    #[test]
    fn decode_samefields_drops_tombstones() {
        let lp = build_samefields_and_deleted_listpack();
        let payload = build_type15_payload_with_listpack(&lp, 2000, 100);
        let (value, _) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
                .expect("decode same-fields");
        match value {
            RdbValue::Stream(entries, _, _) => {
                assert_eq!(entries.len(), 2, "tombstone (flag=3) must be skipped");
                let (ms0, seq0, fields0) = &entries[0];
                assert_eq!(*ms0, 2000);
                assert_eq!(*seq0, 101);
                assert_eq!(fields0, &vec![(b"only".to_vec(), b"A".to_vec())]);
                let (ms1, seq1, fields1) = &entries[1];
                assert_eq!(*ms1, 2000);
                assert_eq!(*seq1, 400);
                assert_eq!(fields1, &vec![(b"only".to_vec(), b"B".to_vec())]);
            }
            other => panic!("expected Stream, got {other:?}"),
        }
    }
}
