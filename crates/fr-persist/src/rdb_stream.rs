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
//! bit 1) are dropped. Type-19/type-21 consumer-group payloads are reified
//! into `RdbStreamConsumerGroup` values with consumer-local PEL ownership.
//! Type-21 encoding (br-frankenredis-6zk9) emits one listpack macro-node per
//! live entry to avoid delta overflow and to keep field metadata local.
//!
//! (br-frankenredis-hjub, br-frankenredis-qi6z, br-frankenredis-6zk9)

use std::collections::BTreeMap;

use crate::listpack::{ListpackEntry, ListpackError, decode_listpack};
use crate::{
    RdbStreamConsumerGroup, RdbStreamMetadata, RdbStreamPendingEntry, RdbValue, StreamEntry,
};

use super::{rdb_decode_length, rdb_decode_string};

/// Upstream stream entry flags (matches upstream's `streamFlags`).
const STREAM_ITEM_FLAG_DELETED: i64 = 1;
const STREAM_ITEM_FLAG_SAMEFIELDS: i64 = 2;
const LISTPACK_HEADER_SIZE: usize = 6;
const LISTPACK_EOF: u8 = 0xFF;

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
    /// The stream listpack header live/deleted counts disagreed with records found.
    InconsistentEntryCount,
    /// The `lp_count` trailer disagreed with how many elements the entry consumed.
    InconsistentEntryTrailer,
    /// A consumer-local PEL referenced an ID absent from the group's global PEL.
    MissingGlobalPelEntry,
}

impl From<ListpackError> for UpstreamStreamError {
    fn from(e: ListpackError) -> Self {
        UpstreamStreamError::InvalidListpack(e)
    }
}

/// Encode an upstream Redis 7.2+ STREAM_LISTPACKS_3 stream object payload.
///
/// Returns `None` when the in-memory group shape cannot be represented as a
/// Redis stream consumer-group payload, currently when a pending entry names a
/// consumer absent from the group's consumer list.
pub(crate) fn encode_upstream_stream_listpacks3(
    entries: &[StreamEntry],
    watermark: Option<(u64, u64)>,
    groups: &[RdbStreamConsumerGroup],
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut sorted_entries = entries.to_vec();
    sorted_entries.sort_by_key(|entry| (entry.0, entry.1));

    super::rdb_encode_length(&mut buf, sorted_entries.len());
    for entry in &sorted_entries {
        super::rdb_encode_string(&mut buf, &stream_id_bytes(entry.0, entry.1));
        let listpack = encode_single_entry_listpack(entry)?;
        super::rdb_encode_string(&mut buf, &listpack);
    }

    super::rdb_encode_length(&mut buf, sorted_entries.len());
    let last_id = watermark
        .or_else(|| sorted_entries.last().map(|entry| (entry.0, entry.1)))
        .unwrap_or((0, 0));
    super::rdb_encode_length(&mut buf, usize::try_from(last_id.0).ok()?);
    super::rdb_encode_length(&mut buf, usize::try_from(last_id.1).ok()?);
    let first_id = sorted_entries
        .first()
        .map(|entry| (entry.0, entry.1))
        .unwrap_or((0, 0));
    super::rdb_encode_length(&mut buf, usize::try_from(first_id.0).ok()?);
    super::rdb_encode_length(&mut buf, usize::try_from(first_id.1).ok()?);
    super::rdb_encode_length(&mut buf, 0); // max_deleted_entry_id.ms
    super::rdb_encode_length(&mut buf, 0); // max_deleted_entry_id.seq
    super::rdb_encode_length(&mut buf, sorted_entries.len()); // entries_added

    super::rdb_encode_length(&mut buf, groups.len());
    for group in groups {
        encode_consumer_group(&mut buf, group)?;
    }

    Some(buf)
}

fn encode_single_entry_listpack(entry: &StreamEntry) -> Option<Vec<u8>> {
    let mut encoded_entries = Vec::new();
    encode_listpack_int(&mut encoded_entries, 1);
    encode_listpack_int(&mut encoded_entries, 0);
    encode_listpack_int(&mut encoded_entries, i64::try_from(entry.2.len()).ok()?);
    for (field, _) in &entry.2 {
        encode_listpack_bytes(&mut encoded_entries, field)?;
    }
    encode_listpack_int(&mut encoded_entries, 0);
    encode_listpack_int(&mut encoded_entries, STREAM_ITEM_FLAG_SAMEFIELDS);
    encode_listpack_int(&mut encoded_entries, 0);
    encode_listpack_int(&mut encoded_entries, 0);
    for (_, value) in &entry.2 {
        encode_listpack_bytes(&mut encoded_entries, value)?;
    }
    encode_listpack_int(
        &mut encoded_entries,
        i64::try_from(entry.2.len().checked_add(3)?).ok()?,
    );

    let total_bytes = LISTPACK_HEADER_SIZE
        .checked_add(encoded_entries.len())?
        .checked_add(1)?;
    let total_bytes = u32::try_from(total_bytes).ok()?;
    let capacity = usize::try_from(total_bytes).ok()?;
    let mut listpack = Vec::with_capacity(capacity);
    listpack.extend_from_slice(&total_bytes.to_le_bytes());
    let entry_count = 8usize.checked_add(entry.2.len().checked_mul(2)?)?;
    let entry_count = u16::try_from(entry_count).unwrap_or(u16::MAX);
    listpack.extend_from_slice(&entry_count.to_le_bytes());
    listpack.extend_from_slice(&encoded_entries);
    listpack.push(LISTPACK_EOF);
    Some(listpack)
}

fn encode_consumer_group(buf: &mut Vec<u8>, group: &RdbStreamConsumerGroup) -> Option<()> {
    super::rdb_encode_string(buf, &group.name);
    super::rdb_encode_length(buf, usize::try_from(group.last_delivered_id_ms).ok()?);
    super::rdb_encode_length(buf, usize::try_from(group.last_delivered_id_seq).ok()?);
    let entries_read = group.entries_read.unwrap_or(u64::MAX);
    super::rdb_encode_length(buf, usize::try_from(entries_read).ok()?);

    let mut pending_by_id: BTreeMap<(u64, u64), &RdbStreamPendingEntry> = BTreeMap::new();
    for pending in &group.pending {
        pending_by_id.insert((pending.entry_id_ms, pending.entry_id_seq), pending);
    }
    super::rdb_encode_length(buf, pending_by_id.len());
    for ((entry_id_ms, entry_id_seq), pending) in &pending_by_id {
        buf.extend_from_slice(&stream_id_bytes(*entry_id_ms, *entry_id_seq));
        buf.extend_from_slice(&pending.last_delivered_ms.to_le_bytes());
        super::rdb_encode_length(buf, usize::try_from(pending.deliveries).ok()?);
    }

    let mut pending_by_consumer: BTreeMap<&[u8], Vec<&RdbStreamPendingEntry>> = BTreeMap::new();
    for pending in &group.pending {
        pending_by_consumer
            .entry(pending.consumer.as_slice())
            .or_default()
            .push(pending);
    }
    for consumer in pending_by_consumer.keys() {
        if !group
            .consumers
            .iter()
            .any(|known| known.as_slice() == *consumer)
        {
            return None;
        }
    }

    super::rdb_encode_length(buf, group.consumers.len());
    for consumer in &group.consumers {
        super::rdb_encode_string(buf, consumer);
        let seen_time = pending_by_consumer
            .get(consumer.as_slice())
            .and_then(|pending| pending.iter().map(|entry| entry.last_delivered_ms).max())
            .unwrap_or(0);
        buf.extend_from_slice(&seen_time.to_le_bytes());
        buf.extend_from_slice(&seen_time.to_le_bytes());
        let pending = pending_by_consumer
            .get(consumer.as_slice())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        super::rdb_encode_length(buf, pending.len());
        for entry in pending {
            buf.extend_from_slice(&stream_id_bytes(entry.entry_id_ms, entry.entry_id_seq));
        }
    }
    Some(())
}

fn stream_id_bytes(ms: u64, seq: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&ms.to_be_bytes());
    bytes.extend_from_slice(&seq.to_be_bytes());
    bytes
}

fn encode_listpack_int(buf: &mut Vec<u8>, value: i64) {
    let start = buf.len();
    if (0..=127).contains(&value) {
        buf.push(value as u8);
    } else if let Ok(value) = i16::try_from(value) {
        buf.push(0xF1);
        buf.extend_from_slice(&value.to_le_bytes());
    } else if let Ok(value) = i32::try_from(value) {
        buf.push(0xF3);
        buf.extend_from_slice(&value.to_le_bytes());
    } else {
        buf.push(0xF4);
        buf.extend_from_slice(&value.to_le_bytes());
    }
    encode_listpack_backlen(buf, buf.len() - start);
}

fn encode_listpack_bytes(buf: &mut Vec<u8>, data: &[u8]) -> Option<()> {
    let start = buf.len();
    if data.len() < 64 {
        buf.push(0x80 | u8::try_from(data.len()).ok()?);
    } else if data.len() < 4096 {
        buf.push(0xE0 | (u8::try_from(data.len() >> 8).ok()? & 0x0F));
        buf.push((data.len() & 0xFF) as u8);
    } else {
        buf.push(0xF0);
        let len = u32::try_from(data.len()).ok()?;
        buf.extend_from_slice(&len.to_le_bytes());
    }
    buf.extend_from_slice(data);
    encode_listpack_backlen(buf, buf.len() - start);
    Some(())
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
        let master_ms = u64::from_be_bytes(
            nodekey[0..8]
                .try_into()
                .map_err(|_| UpstreamStreamError::InvalidNodekeyLength)?,
        );
        let master_seq = u64::from_be_bytes(
            nodekey[8..16]
                .try_into()
                .map_err(|_| UpstreamStreamError::InvalidNodekeyLength)?,
        );
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
    let mut groups = Vec::with_capacity(groups_count.min(256));
    for _ in 0..groups_count {
        let (name, c) =
            rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
        cursor += c;
        let (last_delivered_id_ms, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        let (last_delivered_id_seq, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        let entries_read = if is_v2_or_later {
            let (v, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
            if v == usize::MAX {
                None
            } else {
                Some(v as u64)
            }
        } else {
            None
        };
        let (pel_count, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        let mut global_pel: BTreeMap<(u64, u64), (u64, u64)> = BTreeMap::new();
        for _ in 0..pel_count {
            let (entry_id, c) = take_raw_stream_id(data, cursor)?;
            cursor += c;
            let delivery_time_ms = take_millisecond_time(data, cursor)?;
            cursor += 8;
            let (delivery_count, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
            global_pel.insert(entry_id, (delivery_time_ms, delivery_count as u64));
        }
        let (consumers_count, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        let mut consumers = Vec::with_capacity(consumers_count.min(256));
        let mut pending = Vec::with_capacity(pel_count.min(4096));
        for _ in 0..consumers_count {
            let (consumer_name, c) =
                rdb_decode_string(&data[cursor..]).ok_or(UpstreamStreamError::InvalidString)?;
            cursor += c;
            consumers.push(consumer_name.clone());
            if is_v2_or_later {
                let _seen_time_ms = take_millisecond_time(data, cursor)?;
                cursor += 8;
            }
            if is_v3 {
                let _active_time_ms = take_millisecond_time(data, cursor)?;
                cursor += 8;
            }
            let (cpel_count, c) =
                rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
            cursor += c;
            for _ in 0..cpel_count {
                let (entry_id, c) = take_raw_stream_id(data, cursor)?;
                cursor += c;
                let Some((last_delivered_ms, deliveries)) = global_pel.get(&entry_id) else {
                    return Err(UpstreamStreamError::MissingGlobalPelEntry);
                };
                pending.push(RdbStreamPendingEntry {
                    entry_id_ms: entry_id.0,
                    entry_id_seq: entry_id.1,
                    consumer: consumer_name.clone(),
                    deliveries: *deliveries,
                    last_delivered_ms: *last_delivered_ms,
                });
            }
        }
        groups.push(RdbStreamConsumerGroup {
            name,
            last_delivered_id_ms: last_delivered_id_ms as u64,
            last_delivered_id_seq: last_delivered_id_seq as u64,
            entries_read,
            consumers,
            pending,
        });
    }

    let watermark = Some((last_id_ms as u64, last_id_seq as u64));
    let metadata = RdbStreamMetadata {
        upstream_type_byte: type_byte,
        upstream_payload: data[..cursor].to_vec(),
    };
    let value = RdbValue::Stream(entries, watermark, groups, Some(metadata));
    Ok((value, cursor))
}

fn take_raw_stream_id(
    data: &[u8],
    cursor: usize,
) -> Result<((u64, u64), usize), UpstreamStreamError> {
    if cursor + 16 > data.len() {
        return Err(UpstreamStreamError::InvalidLength);
    }
    let id_ms = u64::from_be_bytes(
        data[cursor..cursor + 8]
            .try_into()
            .map_err(|_| UpstreamStreamError::InvalidLength)?,
    );
    let id_seq = u64::from_be_bytes(
        data[cursor + 8..cursor + 16]
            .try_into()
            .map_err(|_| UpstreamStreamError::InvalidLength)?,
    );
    Ok(((id_ms, id_seq), 16))
}

fn take_millisecond_time(data: &[u8], cursor: usize) -> Result<u64, UpstreamStreamError> {
    if cursor + 8 > data.len() {
        return Err(UpstreamStreamError::InvalidLength);
    }
    Ok(u64::from_le_bytes(
        data[cursor..cursor + 8]
            .try_into()
            .map_err(|_| UpstreamStreamError::InvalidLength)?,
    ))
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
    let declared_live_count = take_usize(lp, &mut idx)?;
    let declared_deleted_count = take_usize(lp, &mut idx)?;
    let declared_total_count = declared_live_count
        .checked_add(declared_deleted_count)
        .ok_or(UpstreamStreamError::InconsistentEntryCount)?;
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

    let mut decoded_total_count = 0usize;
    let mut decoded_live_count = 0usize;
    while idx < lp.len() {
        decoded_total_count = decoded_total_count
            .checked_add(1)
            .ok_or(UpstreamStreamError::InconsistentEntryCount)?;
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

        // lp_count trailer: total listpack elements from flags through the
        // last value. Upstream rejects streams when this count drifts because
        // reverse iteration uses it to seek the previous entry.
        let lp_count = take_int(lp, &mut idx)?;
        let expected_lp_count = expected_entry_lp_count(field_count, same_fields)?;
        if lp_count != expected_lp_count {
            return Err(UpstreamStreamError::InconsistentEntryTrailer);
        }

        if deleted {
            continue;
        }
        decoded_live_count = decoded_live_count
            .checked_add(1)
            .ok_or(UpstreamStreamError::InconsistentEntryCount)?;
        let ms = combine_u64_i64(master_ms, ms_delta);
        let seq = combine_u64_i64(master_seq, seq_delta);
        out.push((ms, seq, fields));
    }
    if decoded_total_count != declared_total_count || decoded_live_count != declared_live_count {
        return Err(UpstreamStreamError::InconsistentEntryCount);
    }
    Ok(())
}

fn expected_entry_lp_count(
    field_count: usize,
    same_fields: bool,
) -> Result<i64, UpstreamStreamError> {
    let fixed_fields = 3usize; // flags + ms_delta + seq_delta
    let total = if same_fields {
        field_count.checked_add(fixed_fields)
    } else {
        field_count
            .checked_mul(2)
            .and_then(|dynamic_fields| dynamic_fields.checked_add(fixed_fields + 1))
    };
    let total = total.ok_or(UpstreamStreamError::InvalidFieldCount)?;
    i64::try_from(total).map_err(|_| UpstreamStreamError::InvalidFieldCount)
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
    use crate::{
        UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2,
        UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, rdb_encode_length,
    };

    type StreamParts = (
        Vec<StreamEntry>,
        Option<(u64, u64)>,
        Vec<RdbStreamConsumerGroup>,
    );

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

    fn rdb_encode_raw_stream_id(buf: &mut Vec<u8>, ms: u64, seq: u64) {
        buf.extend_from_slice(&ms.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
    }

    fn rdb_encode_millisecond_time(buf: &mut Vec<u8>, ms: u64) {
        buf.extend_from_slice(&ms.to_le_bytes());
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
    /// lp_count=8.
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
            lp_u7(8), // lp_count trailer
        ];
        assemble_listpack(&entries)
    }

    /// Same shape as build_unique_fields_listpack, but with a corrupted
    /// lp_count trailer. Upstream's streamValidateListpackIntegrity rejects
    /// this as malformed because reverse iteration would seek to the wrong
    /// entry boundary.
    fn build_inconsistent_lp_count_listpack() -> Vec<u8> {
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
            lp_u7(10), // wrong: expected 8
        ];
        assemble_listpack(&entries)
    }

    /// Same shape as build_unique_fields_listpack, but with a corrupted
    /// live-entry count in the listpack header. Upstream's deep stream
    /// integrity pass walks exactly count+deleted records, so accepting
    /// surplus records here would load a malformed stream the oracle rejects.
    fn build_inconsistent_header_count_listpack() -> Vec<u8> {
        let entries: Vec<Vec<u8>> = vec![
            lp_u7(0),      // wrong: one live entry follows
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
            lp_u7(8), // lp_count trailer
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
            lp_u7(4),                                 // lp_count
            // Entry 2: deleted + same-fields.
            lp_u7((STREAM_ITEM_FLAG_SAMEFIELDS | STREAM_ITEM_FLAG_DELETED) as u8), // flags=3
            lp_u7(0),                                                              // ms_delta=0
            lp_u7(2),                                                              // seq_delta=2
            lp_str(b"X"), // value (still present for tombstone)
            lp_u7(4),     // lp_count
            // Entry 3: live, unique fields (flags=0), using i16 for a
            // larger seq delta.
            lp_u7(0),    // flags=0
            lp_u7(0),    // ms_delta=0
            lp_i16(300), // seq_delta=300
            lp_u7(1),    // per-entry field_count
            lp_str(b"only"),
            lp_str(b"B"),
            lp_u7(6), // lp_count
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

    fn build_type21_payload_with_consumer_group() -> Vec<u8> {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 0); // listpacks_count
        rdb_encode_length(&mut buf, 0); // stream length
        rdb_encode_length(&mut buf, 42); // last_id.ms
        rdb_encode_length(&mut buf, 7); // last_id.seq
        rdb_encode_length(&mut buf, 42); // first_id.ms
        rdb_encode_length(&mut buf, 7); // first_id.seq
        rdb_encode_length(&mut buf, 0); // max_deleted_id.ms
        rdb_encode_length(&mut buf, 0); // max_deleted_id.seq
        rdb_encode_length(&mut buf, 1); // entries_added
        rdb_encode_length(&mut buf, 1); // groups_count

        rdb_encode_raw_bytes(&mut buf, b"g");
        rdb_encode_length(&mut buf, 42); // group last_id.ms
        rdb_encode_length(&mut buf, 7); // group last_id.seq
        rdb_encode_length(&mut buf, 1); // entries_read

        rdb_encode_length(&mut buf, 1); // global PEL count
        rdb_encode_raw_stream_id(&mut buf, 42, 7);
        rdb_encode_millisecond_time(&mut buf, 1000);
        rdb_encode_length(&mut buf, 3); // delivery_count

        rdb_encode_length(&mut buf, 2); // consumers_count
        rdb_encode_raw_bytes(&mut buf, b"alice");
        rdb_encode_millisecond_time(&mut buf, 1100); // seen_time
        rdb_encode_millisecond_time(&mut buf, 1200); // active_time
        rdb_encode_length(&mut buf, 1); // alice PEL count
        rdb_encode_raw_stream_id(&mut buf, 42, 7);
        rdb_encode_raw_bytes(&mut buf, b"bob");
        rdb_encode_millisecond_time(&mut buf, 1300); // seen_time
        rdb_encode_millisecond_time(&mut buf, 1400); // active_time
        rdb_encode_length(&mut buf, 0); // bob PEL count

        buf
    }

    fn build_type19_payload_with_consumer_group() -> Vec<u8> {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 0); // listpacks_count
        rdb_encode_length(&mut buf, 0); // stream length
        rdb_encode_length(&mut buf, 42); // last_id.ms
        rdb_encode_length(&mut buf, 7); // last_id.seq
        rdb_encode_length(&mut buf, 42); // first_id.ms
        rdb_encode_length(&mut buf, 7); // first_id.seq
        rdb_encode_length(&mut buf, 0); // max_deleted_id.ms
        rdb_encode_length(&mut buf, 0); // max_deleted_id.seq
        rdb_encode_length(&mut buf, 1); // entries_added
        rdb_encode_length(&mut buf, 1); // groups_count

        rdb_encode_raw_bytes(&mut buf, b"g");
        rdb_encode_length(&mut buf, 42); // group last_id.ms
        rdb_encode_length(&mut buf, 7); // group last_id.seq
        rdb_encode_length(&mut buf, 1); // entries_read

        rdb_encode_length(&mut buf, 1); // global PEL count
        rdb_encode_raw_stream_id(&mut buf, 42, 7);
        rdb_encode_millisecond_time(&mut buf, 1000);
        rdb_encode_length(&mut buf, 3); // delivery_count

        rdb_encode_length(&mut buf, 1); // consumers_count
        rdb_encode_raw_bytes(&mut buf, b"alice");
        rdb_encode_millisecond_time(&mut buf, 1100); // seen_time
        rdb_encode_length(&mut buf, 1); // alice PEL count
        rdb_encode_raw_stream_id(&mut buf, 42, 7);

        buf
    }

    fn build_type21_payload_with_missing_global_pel() -> Vec<u8> {
        let mut buf = Vec::new();
        rdb_encode_length(&mut buf, 0); // listpacks_count
        rdb_encode_length(&mut buf, 0); // stream length
        rdb_encode_length(&mut buf, 42); // last_id.ms
        rdb_encode_length(&mut buf, 7); // last_id.seq
        rdb_encode_length(&mut buf, 42); // first_id.ms
        rdb_encode_length(&mut buf, 7); // first_id.seq
        rdb_encode_length(&mut buf, 0); // max_deleted_id.ms
        rdb_encode_length(&mut buf, 0); // max_deleted_id.seq
        rdb_encode_length(&mut buf, 1); // entries_added
        rdb_encode_length(&mut buf, 1); // groups_count

        rdb_encode_raw_bytes(&mut buf, b"g");
        rdb_encode_length(&mut buf, 42); // group last_id.ms
        rdb_encode_length(&mut buf, 7); // group last_id.seq
        rdb_encode_length(&mut buf, 1); // entries_read
        rdb_encode_length(&mut buf, 0); // global PEL count

        rdb_encode_length(&mut buf, 1); // consumers_count
        rdb_encode_raw_bytes(&mut buf, b"alice");
        rdb_encode_millisecond_time(&mut buf, 1100); // seen_time
        rdb_encode_millisecond_time(&mut buf, 1200); // active_time
        rdb_encode_length(&mut buf, 1); // alice PEL count
        rdb_encode_raw_stream_id(&mut buf, 42, 7);

        buf
    }

    fn stream_parts(value: RdbValue) -> Option<StreamParts> {
        match value {
            RdbValue::Stream(entries, watermark, groups, _) => Some((entries, watermark, groups)),
            _ => None,
        }
    }

    #[test]
    fn encode_type21_round_trips_entries_and_consumer_groups() {
        let entries = vec![
            (
                1001,
                1,
                vec![
                    (b"name".to_vec(), b"Bob".to_vec()),
                    (b"age".to_vec(), b"31".to_vec()),
                ],
            ),
            (
                1000,
                0,
                vec![
                    (b"name".to_vec(), b"Alice".to_vec()),
                    (b"age".to_vec(), b"30".to_vec()),
                ],
            ),
        ];
        let groups = vec![RdbStreamConsumerGroup {
            name: b"group".to_vec(),
            last_delivered_id_ms: 1001,
            last_delivered_id_seq: 1,
            entries_read: Some(2),
            consumers: vec![b"alice".to_vec(), b"bob".to_vec()],
            pending: vec![
                RdbStreamPendingEntry {
                    entry_id_ms: 1001,
                    entry_id_seq: 1,
                    consumer: b"bob".to_vec(),
                    deliveries: 2,
                    last_delivered_ms: 6000,
                },
                RdbStreamPendingEntry {
                    entry_id_ms: 1000,
                    entry_id_seq: 0,
                    consumer: b"alice".to_vec(),
                    deliveries: 1,
                    last_delivered_ms: 5000,
                },
            ],
        }];

        let payload = encode_upstream_stream_listpacks3(&entries, Some((1001, 1)), &groups)
            .expect("encode type21 payload");
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                .expect("decode encoded payload");
        assert_eq!(consumed, payload.len());

        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((decoded_entries, watermark, decoded_groups)) = stream else {
            return;
        };
        assert_eq!(
            decoded_entries,
            vec![
                (
                    1000,
                    0,
                    vec![
                        (b"name".to_vec(), b"Alice".to_vec()),
                        (b"age".to_vec(), b"30".to_vec()),
                    ],
                ),
                (
                    1001,
                    1,
                    vec![
                        (b"name".to_vec(), b"Bob".to_vec()),
                        (b"age".to_vec(), b"31".to_vec()),
                    ],
                ),
            ]
        );
        assert_eq!(watermark, Some((1001, 1)));
        assert_eq!(
            decoded_groups,
            vec![RdbStreamConsumerGroup {
                name: b"group".to_vec(),
                last_delivered_id_ms: 1001,
                last_delivered_id_seq: 1,
                entries_read: Some(2),
                consumers: vec![b"alice".to_vec(), b"bob".to_vec()],
                pending: vec![
                    RdbStreamPendingEntry {
                        entry_id_ms: 1000,
                        entry_id_seq: 0,
                        consumer: b"alice".to_vec(),
                        deliveries: 1,
                        last_delivered_ms: 5000,
                    },
                    RdbStreamPendingEntry {
                        entry_id_ms: 1001,
                        entry_id_seq: 1,
                        consumer: b"bob".to_vec(),
                        deliveries: 2,
                        last_delivered_ms: 6000,
                    },
                ],
            }]
        );
    }

    #[test]
    fn encode_type21_declines_pending_consumer_missing_from_group() {
        let entries = vec![(1000, 0, vec![(b"field".to_vec(), b"value".to_vec())])];
        let groups = vec![RdbStreamConsumerGroup {
            name: b"group".to_vec(),
            last_delivered_id_ms: 1000,
            last_delivered_id_seq: 0,
            entries_read: None,
            consumers: vec![b"alice".to_vec()],
            pending: vec![RdbStreamPendingEntry {
                entry_id_ms: 1000,
                entry_id_seq: 0,
                consumer: b"bob".to_vec(),
                deliveries: 1,
                last_delivered_ms: 5000,
            }],
        }];

        assert!(encode_upstream_stream_listpacks3(&entries, Some((1000, 0)), &groups).is_none());
    }

    #[test]
    fn encode_type21_round_trips_twenty_stream_fixtures() {
        for fixture in 0..20_u64 {
            let entry_count = usize::try_from((fixture % 4) + 1).expect("small count");
            let field_count = usize::try_from((fixture % 3) + 1).expect("small count");
            let entries: Vec<StreamEntry> = (0..entry_count)
                .rev()
                .map(|offset| {
                    let ms = 10_000 + fixture;
                    let seq = offset as u64;
                    let fields = (0..field_count)
                        .map(|field| {
                            (
                                format!("f{field}").into_bytes(),
                                format!("fixture-{fixture}-{offset}-{field}").into_bytes(),
                            )
                        })
                        .collect();
                    (ms, seq, fields)
                })
                .collect();
            let mut expected_entries = entries.clone();
            expected_entries.sort_by_key(|entry| (entry.0, entry.1));
            let watermark = expected_entries
                .last()
                .map(|entry| (entry.0, entry.1))
                .expect("fixture has entries");

            let groups = if fixture % 2 == 0 {
                let pending_id = expected_entries[0].clone();
                vec![RdbStreamConsumerGroup {
                    name: format!("group-{fixture}").into_bytes(),
                    last_delivered_id_ms: watermark.0,
                    last_delivered_id_seq: watermark.1,
                    entries_read: None,
                    consumers: vec![b"consumer".to_vec()],
                    pending: vec![RdbStreamPendingEntry {
                        entry_id_ms: pending_id.0,
                        entry_id_seq: pending_id.1,
                        consumer: b"consumer".to_vec(),
                        deliveries: fixture + 1,
                        last_delivered_ms: 50_000 + fixture,
                    }],
                }]
            } else {
                Vec::new()
            };

            let payload = encode_upstream_stream_listpacks3(&entries, Some(watermark), &groups)
                .expect("encode fixture");
            let (value, consumed) =
                decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                    .expect("decode fixture");
            assert_eq!(consumed, payload.len());

            let stream = stream_parts(value);
            assert!(stream.is_some(), "expected Stream for fixture {fixture}");
            let Some((decoded_entries, decoded_watermark, decoded_groups)) = stream else {
                return;
            };
            assert_eq!(decoded_entries, expected_entries, "fixture {fixture}");
            assert_eq!(decoded_watermark, Some(watermark), "fixture {fixture}");
            assert_eq!(decoded_groups, groups, "fixture {fixture}");
        }
    }

    #[test]
    fn decode_empty_type15_returns_skeleton_stream_with_watermark() {
        let payload = build_empty_type15(12345, 7);
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
                .expect("decode skeleton");
        assert_eq!(consumed, payload.len());
        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((entries, watermark, groups)) = stream else {
            return;
        };
        assert!(entries.is_empty());
        assert!(groups.is_empty());
        assert_eq!(watermark, Some((12345, 7)));
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
        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((entries, watermark, groups)) = stream else {
            return;
        };
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

    #[test]
    fn decode_rejects_inconsistent_entry_trailer_count() {
        let lp = build_inconsistent_lp_count_listpack();
        let payload = build_type15_payload_with_listpack(&lp, 1000, 0);
        let err = decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
            .unwrap_err();
        assert_eq!(err, UpstreamStreamError::InconsistentEntryTrailer);
    }

    #[test]
    fn decode_rejects_inconsistent_header_entry_count() {
        let lp = build_inconsistent_header_count_listpack();
        let payload = build_type15_payload_with_listpack(&lp, 1000, 0);
        let err = decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
            .unwrap_err();
        assert_eq!(err, UpstreamStreamError::InconsistentEntryCount);
    }

    #[test]
    fn decode_samefields_drops_tombstones() {
        let lp = build_samefields_and_deleted_listpack();
        let payload = build_type15_payload_with_listpack(&lp, 2000, 100);
        let (value, _) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS, &payload)
                .expect("decode same-fields");
        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((entries, _, _)) = stream else {
            return;
        };
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

    #[test]
    fn decode_type21_reifies_consumer_groups_and_pel_ownership() {
        let payload = build_type21_payload_with_consumer_group();
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                .expect("decode type 21 consumer group");
        assert_eq!(consumed, payload.len());

        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((entries, watermark, groups)) = stream else {
            return;
        };
        assert!(entries.is_empty());
        assert_eq!(watermark, Some((42, 7)));
        assert_eq!(groups.len(), 1);

        let group = &groups[0];
        assert_eq!(group.name, b"g".to_vec());
        assert_eq!(group.last_delivered_id_ms, 42);
        assert_eq!(group.last_delivered_id_seq, 7);
        assert_eq!(group.consumers, vec![b"alice".to_vec(), b"bob".to_vec()]);
        assert_eq!(
            group.pending,
            vec![RdbStreamPendingEntry {
                entry_id_ms: 42,
                entry_id_seq: 7,
                consumer: b"alice".to_vec(),
                deliveries: 3,
                last_delivered_ms: 1000,
            }]
        );
    }

    #[test]
    fn decode_type19_reifies_consumer_groups_and_seen_time() {
        let payload = build_type19_payload_with_consumer_group();
        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2, &payload)
                .expect("decode type 19 consumer group");
        assert_eq!(consumed, payload.len());

        let stream = stream_parts(value);
        assert!(stream.is_some(), "expected Stream");
        let Some((entries, watermark, groups)) = stream else {
            return;
        };
        assert!(entries.is_empty());
        assert_eq!(watermark, Some((42, 7)));
        assert_eq!(groups.len(), 1);

        let group = &groups[0];
        assert_eq!(group.name, b"g".to_vec());
        assert_eq!(group.consumers, vec![b"alice".to_vec()]);
        assert_eq!(
            group.pending,
            vec![RdbStreamPendingEntry {
                entry_id_ms: 42,
                entry_id_seq: 7,
                consumer: b"alice".to_vec(),
                deliveries: 3,
                last_delivered_ms: 1000,
            }]
        );
    }

    #[test]
    fn decode_type21_rejects_consumer_pel_without_global_entry() {
        let payload = build_type21_payload_with_missing_global_pel();
        let err = decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
            .unwrap_err();
        assert_eq!(err, UpstreamStreamError::MissingGlobalPelEntry);
    }

    /// Lock in the SCG_INVALID_ENTRIES_READ contract: when fr-persist
    /// emits a type-21 consumer-group payload from in-memory state
    /// (i.e. without retained `RdbStreamMetadata` upstream payload), it
    /// must encode `entries_read` as the upstream sentinel `-1` (=
    /// `u64::MAX` on the wire), NOT as `pending.len()`. The sentinel
    /// signals to upstream's loadrdb path to fall back to lag-by-
    /// distance estimation instead of trusting a count we don't
    /// actually track. (br-frankenredis-3njd)
    #[test]
    fn encode_consumer_group_writes_scg_invalid_entries_read_sentinel() {
        // Single group with 2 pending entries — the OLD (wrong) encoder
        // would have written `2` for entries_read here.
        let entries: Vec<super::StreamEntry> = vec![(10, 0, vec![(b"f".to_vec(), b"v".to_vec())])];
        let groups = vec![RdbStreamConsumerGroup {
            name: b"g".to_vec(),
            last_delivered_id_ms: 10,
            last_delivered_id_seq: 0,
            entries_read: None,
            consumers: vec![b"alice".to_vec()],
            pending: vec![
                RdbStreamPendingEntry {
                    entry_id_ms: 10,
                    entry_id_seq: 0,
                    consumer: b"alice".to_vec(),
                    deliveries: 1,
                    last_delivered_ms: 100,
                },
                RdbStreamPendingEntry {
                    entry_id_ms: 11,
                    entry_id_seq: 0,
                    consumer: b"alice".to_vec(),
                    deliveries: 1,
                    last_delivered_ms: 101,
                },
            ],
        }];

        let payload = encode_upstream_stream_listpacks3(&entries, Some((11, 0)), &groups)
            .expect("encode type21 payload with consumer group");

        // Byte-scan for the SCG_INVALID_ENTRIES_READ sentinel encoding:
        // upstream `rdbSaveLen(u64::MAX)` falls into the `>UINT32_MAX`
        // branch and emits `0x81` followed by 8-byte big-endian
        // `0xFFFFFFFFFFFFFFFF`. That 9-byte sequence appears nowhere
        // else in a well-formed stream payload, so its presence
        // uniquely confirms the sentinel was emitted.
        let needle: [u8; 9] = [0x81, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(
            payload.windows(needle.len()).any(|w| w == needle),
            "expected SCG_INVALID_ENTRIES_READ sentinel (0x81 + 8x0xFF) somewhere in the \
             encoded payload; got {} bytes: {:?}",
            payload.len(),
            &payload[..payload.len().min(64)]
        );

        // OLD-behavior anti-test: the wrong encoder used to emit
        // `pending.len() = 2` as a 1-byte length (0x02). Confirm that
        // 0x02 isn't sitting at the consumer-group entries_read slot
        // anymore. The slot directly follows last_delivered_id_seq
        // (also 0x00 here for ms=10/seq=0 → encoded as 0x0A 0x00).
        // This is a sanity probe, not a strict invariant.
        let group_name_marker: &[u8] = &[0x01, b'g']; // rdb_encode_string("g")
        let group_start = payload
            .windows(2)
            .position(|w| w == group_name_marker)
            .expect("group name marker present");
        // After name (2 bytes) + last_id.ms (1 byte: 0x0A) + last_id.seq
        // (1 byte: 0x00), the next byte starts entries_read.
        let entries_read_byte = payload[group_start + 4];
        assert_eq!(
            entries_read_byte, 0x81,
            "entries_read slot should start with the 0x81 (64-bit length) marker, \
             not the old buggy 0x02 (= pending.len()); got 0x{entries_read_byte:02X}"
        );

        // Round-trip is still consumed end-to-end (decoder discards
        // entries_read so this still passes).
        let (_, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                .expect("decode payload with sentinel entries_read");
        assert_eq!(consumed, payload.len());
    }
}
