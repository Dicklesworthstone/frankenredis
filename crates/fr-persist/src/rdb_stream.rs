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
//! Type-21 encoding (br-frankenredis-6zk9) groups live entries into listpack
//! macro-nodes exactly as upstream `streamAppendItem` does — bounded by
//! `stream-node-max-bytes` (4096) and `stream-node-max-entries` (100), with a
//! per-node master entry and delta+SAMEFIELDS-compressed members — so DUMP/RDB
//! bytes match what sequential XADD would have produced (frankenredis-ren6y).
//!
//! (br-frankenredis-hjub, br-frankenredis-qi6z, br-frankenredis-6zk9)

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::listpack::{ListpackError, RawKind, RawListpackValue, decode_raw_values};
use crate::{
    BorrowedStreamEntry, EncodableStreamEntry, RdbStreamConsumer, RdbStreamConsumerGroup,
    RdbStreamMetadata, RdbStreamPendingEntry, RdbValue, StreamEntry,
};

use super::{rdb_decode_length, rdb_decode_string};

/// Upstream stream entry flags (matches upstream's `streamFlags`).
const STREAM_ITEM_FLAG_NONE: i64 = 0;
const STREAM_ITEM_FLAG_DELETED: i64 = 1;
const STREAM_ITEM_FLAG_SAMEFIELDS: i64 = 2;
const LISTPACK_HEADER_SIZE: usize = 6;
const LISTPACK_EOF: u8 = 0xFF;
/// Upstream `lpGetNumElements` sentinel: a listpack with >= this many elements
/// stores the count as "unknown" and forces a full scan on read.
const LISTPACK_NUMELE_UNKNOWN: usize = 0xFFFF;
/// Defaults of `stream-node-max-bytes` / `stream-node-max-entries`. These bound
/// each radix-tree macro-node exactly as upstream `streamAppendItem` does, so a
/// stream rebuilt from these entries lands on the same node boundaries — and
/// therefore the same DUMP/RDB bytes — that sequential XADD would have produced.
const STREAM_NODE_MAX_BYTES: usize = 4096;
const STREAM_NODE_MAX_ENTRIES: u64 = 100;
/// Hard ceiling upstream applies when `stream-node-max-bytes` is 0/huge.
const STREAM_LISTPACK_MAX_SIZE: usize = 1 << 30;

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
pub(crate) fn encode_upstream_stream_listpacks3<F, V>(
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
    encode_upstream_stream_listpacks3_impl::<true, _, _>(
        entries,
        watermark,
        groups,
        entries_added,
        max_deleted,
    )
}

#[cfg(feature = "bench-reference")]
pub(crate) fn bench_encode_upstream_stream_listpacks3<const DIRECT_IDS: bool, F, V>(
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
    encode_upstream_stream_listpacks3_impl::<DIRECT_IDS, _, _>(
        entries,
        watermark,
        groups,
        entries_added,
        max_deleted,
    )
}

#[cfg_attr(feature = "bench-reference", inline(never))]
fn encode_upstream_stream_listpacks3_impl<const DIRECT_IDS: bool, F, V>(
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
    let mut buf = Vec::new();
    // fr-store's PackedStreamLog already yields entries in id order, so the
    // common DUMP/RDB-save path is already sorted — avoid the `to_vec()` + sort.
    // (The DUMP caller also passes borrowed `&[u8]` field/value slices, so even
    // the fallback clone copies only slice pointers, never the field bytes.)
    let sorted_storage: Vec<EncodableStreamEntry<F, V>>;
    let sorted_entries: &[EncodableStreamEntry<F, V>] = if entries
        .windows(2)
        .all(|w| (w[0].0, w[0].1) <= (w[1].0, w[1].1))
    {
        entries
    } else {
        let mut owned = entries.to_vec();
        owned.sort_by_key(|entry| (entry.0, entry.1));
        sorted_storage = owned;
        &sorted_storage
    };

    // Group entries into listpack macro-nodes mirroring upstream
    // `streamAppendItem`'s split rules, then emit one master-entry listpack per
    // node (subsequent entries delta+SAMEFIELDS compressed against the master).
    // This reproduces the exact node layout — and DUMP/RDB bytes — that
    // sequential XADD would have built, instead of one listpack per entry.
    let nodes = pack_stream_nodes(sorted_entries)?;

    super::rdb_encode_length(&mut buf, nodes.len());
    for node in &nodes {
        encode_stream_id_string::<DIRECT_IDS>(&mut buf, node.master.0, node.master.1);
        super::rdb_encode_string(&mut buf, &node.listpack);
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
    let max_deleted = max_deleted.unwrap_or((0, 0));
    super::rdb_encode_length(&mut buf, usize::try_from(max_deleted.0).ok()?); // max_deleted_entry_id.ms
    super::rdb_encode_length(&mut buf, usize::try_from(max_deleted.1).ok()?); // max_deleted_entry_id.seq
    let entries_added = entries_added.unwrap_or(u64::try_from(sorted_entries.len()).ok()?);
    super::rdb_encode_length(&mut buf, usize::try_from(entries_added).ok()?);

    super::rdb_encode_length(&mut buf, groups.len());
    for group in groups {
        encode_consumer_group::<DIRECT_IDS>(&mut buf, group)?;
    }

    Some(buf)
}

/// A finished radix-tree macro-node: its master (first-entry) ID and the fully
/// serialized listpack blob holding the master entry plus all node members.
struct StreamNode {
    master: (u64, u64),
    listpack: Vec<u8>,
}

/// Accumulator for the node currently being filled.
struct NodeBuilder<'a> {
    master: (u64, u64),
    master_fields: Vec<&'a [u8]>,
    /// Encoded bytes of the member entries appended so far (without the master
    /// entry header, which is rebuilt at finalize when `count` is known).
    members: Vec<u8>,
    /// Number of member entries appended so far (== the master `count` field;
    /// `deleted` is always 0 here since fr-store keeps no listpack tombstones).
    count: u64,
    /// Running listpack element count, for the 16-bit header `num-elements`.
    num_elements: usize,
    /// Cached byte length of the master-entry header EXCEPT its leading `count`
    /// varint: `[deleted=0][numfields][field…][terminator=0]`. The master fields
    /// are fixed once the node opens, so this is computed once at node creation
    /// instead of re-encoding every field on every entry's split test (the old
    /// per-entry `master_entry_bytes` allocated a temp Vec and re-encoded all
    /// fields — O(entries×fields) for nothing). Only the `count` varint, which
    /// grows with each member, is re-measured per entry.
    master_rest_bytes: usize,
}

/// Byte length of `[deleted=0][numfields][field…][terminator=0]` — the
/// constant tail of the master-entry header (everything after its leading
/// `count` varint). Computed once per node; the per-entry split test adds only
/// the current `count` varint length on top.
fn master_rest_bytes(fields: &[&[u8]]) -> Option<usize> {
    let mut tmp = Vec::new();
    encode_listpack_int(&mut tmp, 0); // deleted
    encode_listpack_int(&mut tmp, i64::try_from(fields.len()).ok()?);
    for field in fields {
        encode_listpack_bytes(&mut tmp, field)?;
    }
    encode_listpack_int(&mut tmp, 0); // master zero terminator
    Some(tmp.len())
}

/// True when `entry`'s field names match the node master's, in order — the
/// condition upstream uses to set `STREAM_ITEM_FLAG_SAMEFIELDS`.
fn entry_same_fields<F: AsRef<[u8]>, V>(
    entry: &EncodableStreamEntry<F, V>,
    master_fields: &[&[u8]],
) -> bool {
    entry.2.len() == master_fields.len()
        && entry
            .2
            .iter()
            .zip(master_fields)
            .all(|((field, _), master)| same_field_name(field.as_ref(), master))
}

/// Field-name equality with an ADDRESS short-circuit.
///
/// This runs once per field per entry and is the SAMEFIELDS test, so on the shape
/// it exists to detect it is called with the same name over and over: a stream
/// saved out of `PackedStreamLog` hands every entry the same `field_dict` slice,
/// and comparing two-byte names by bytes is a libc `memcmp` CALL -- measured at
/// 16,000 of the 21,600 memcmp calls per 200-key stream DEBUG RELOAD, two per
/// entry.
///
/// Equal pointer and equal length is the same memory, hence the same bytes, so the
/// short-circuit can only skip work it would have proved. Anything else falls
/// through to the byte compare, so a caller whose names are separately owned is
/// unaffected apart from one extra pointer test.
#[inline(always)]
fn same_field_name(field: &[u8], master: &[u8]) -> bool {
    field.len() == master.len()
        && (std::ptr::eq(field.as_ptr(), master.as_ptr()) || field == master)
}

/// Append one member entry to `builder` in upstream's listpack item layout.
fn append_member<F: AsRef<[u8]>, V: AsRef<[u8]>>(
    builder: &mut NodeBuilder,
    entry: &EncodableStreamEntry<F, V>,
) -> Option<()> {
    let numfields = entry.2.len();
    let same_fields = entry_same_fields(entry, &builder.master_fields);
    let flags = if same_fields {
        STREAM_ITEM_FLAG_SAMEFIELDS
    } else {
        STREAM_ITEM_FLAG_NONE
    };
    let buf = &mut builder.members;
    encode_listpack_int(buf, flags);
    // ms/seq delta vs the master ID. Upstream computes these as wrapping u64
    // subtraction reinterpreted as i64, so seq deltas may be negative when a
    // later ms carries a smaller seq.
    encode_listpack_int(buf, entry.0.wrapping_sub(builder.master.0) as i64);
    encode_listpack_int(buf, entry.1.wrapping_sub(builder.master.1) as i64);
    if !same_fields {
        encode_listpack_int(buf, i64::try_from(numfields).ok()?);
    }
    for (field, value) in &entry.2 {
        if !same_fields {
            encode_listpack_bytes(buf, field.as_ref())?;
        }
        encode_listpack_bytes(buf, value.as_ref())?;
    }
    // lp-count: number of listpack pieces composing this entry (for reverse
    // traversal). 3 fixed (flags + ms + seq) + numfields values, plus the
    // num-fields element and the field names when fields aren't reused.
    let mut lp_count = numfields.checked_add(3)?;
    if !same_fields {
        lp_count = lp_count.checked_add(numfields.checked_add(1)?)?;
    }
    encode_listpack_int(buf, i64::try_from(lp_count).ok()?);

    builder.count = builder.count.checked_add(1)?;
    // Element accounting mirrors the pieces emitted above.
    let elements = if same_fields {
        numfields.checked_add(4)? // flags, ms, seq, lp-count + values
    } else {
        numfields.checked_mul(2)?.checked_add(5)? // flags, ms, seq, numfields, lp-count + field/value pairs
    };
    builder.num_elements = builder.num_elements.checked_add(elements)?;
    Some(())
}

/// Serialize a finished node into its on-disk listpack blob.
fn finalize_node(builder: &NodeBuilder) -> Option<StreamNode> {
    let mut body = Vec::new();
    encode_listpack_int(&mut body, i64::try_from(builder.count).ok()?);
    encode_listpack_int(&mut body, 0); // deleted
    encode_listpack_int(&mut body, i64::try_from(builder.master_fields.len()).ok()?);
    for field in &builder.master_fields {
        encode_listpack_bytes(&mut body, field)?;
    }
    encode_listpack_int(&mut body, 0); // master zero terminator
    body.extend_from_slice(&builder.members);

    let total_bytes = LISTPACK_HEADER_SIZE
        .checked_add(body.len())?
        .checked_add(1)?;
    let total_bytes = u32::try_from(total_bytes).ok()?;
    let mut listpack = Vec::with_capacity(usize::try_from(total_bytes).ok()?);
    listpack.extend_from_slice(&total_bytes.to_le_bytes());
    let num_elements = if builder.num_elements >= LISTPACK_NUMELE_UNKNOWN {
        LISTPACK_NUMELE_UNKNOWN as u16
    } else {
        builder.num_elements as u16
    };
    listpack.extend_from_slice(&num_elements.to_le_bytes());
    listpack.extend_from_slice(&body);
    listpack.push(LISTPACK_EOF);
    Some(StreamNode {
        master: builder.master,
        listpack,
    })
}

/// Group sorted stream entries into listpack macro-nodes, reproducing upstream
/// `streamAppendItem`'s incremental split decisions.
fn pack_stream_nodes<F: AsRef<[u8]>, V: AsRef<[u8]>>(
    entries: &[EncodableStreamEntry<F, V>],
) -> Option<Vec<StreamNode>> {
    let mut nodes = Vec::new();
    let mut current: Option<NodeBuilder> = None;
    // Reused scratch for measuring the master `count` varint each split test —
    // avoids a per-entry allocation while keeping `encode_listpack_int` as the
    // single source of truth for the byte length (no reimplemented length rule).
    let mut count_scratch: Vec<u8> = Vec::with_capacity(8);

    for entry in entries {
        let totelelen: usize = entry
            .2
            .iter()
            .map(|(field, value)| field.as_ref().len() + value.as_ref().len())
            .sum();

        let need_new_node = match &current {
            None => true,
            Some(builder) => {
                count_scratch.clear();
                encode_listpack_int(&mut count_scratch, i64::try_from(builder.count).ok()?);
                let master_entry = count_scratch.len() + builder.master_rest_bytes;
                let lp_bytes = LISTPACK_HEADER_SIZE + master_entry + builder.members.len() + 1; // EOF
                lp_bytes.saturating_add(totelelen)
                    >= STREAM_NODE_MAX_BYTES.min(STREAM_LISTPACK_MAX_SIZE)
                    || builder.count >= STREAM_NODE_MAX_ENTRIES
            }
        };

        if need_new_node {
            if let Some(builder) = current.take() {
                nodes.push(finalize_node(&builder)?);
            }
            let master_fields: Vec<&[u8]> =
                entry.2.iter().map(|(field, _)| field.as_ref()).collect();
            let num_elements = master_fields.len().checked_add(4)?; // count, deleted, numfields, fields, terminator
            let master_rest_bytes = master_rest_bytes(&master_fields)?;
            current = Some(NodeBuilder {
                master: (entry.0, entry.1),
                master_fields,
                members: Vec::new(),
                count: 0,
                num_elements,
                master_rest_bytes,
            });
        }

        let builder = current.as_mut()?;
        append_member(builder, entry)?;
    }

    if let Some(builder) = current.take() {
        nodes.push(finalize_node(&builder)?);
    }
    Some(nodes)
}

fn encode_consumer_group<const DIRECT_IDS: bool>(
    buf: &mut Vec<u8>,
    group: &RdbStreamConsumerGroup,
) -> Option<()> {
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
        append_stream_id::<DIRECT_IDS>(buf, *entry_id_ms, *entry_id_seq);
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
            .any(|known| known.name.as_slice() == *consumer)
        {
            return None;
        }
    }

    super::rdb_encode_length(buf, group.consumers.len());
    for consumer in &group.consumers {
        super::rdb_encode_string(buf, &consumer.name);
        // seen_time, then active_time — both mstime_t (i64 LE). `None` active
        // time is upstream's `-1` sentinel. (frankenredis-sq4ov)
        buf.extend_from_slice(&consumer.seen_time_ms.to_le_bytes());
        let active = consumer.active_time_ms.map_or(-1i64, |v| v as i64);
        buf.extend_from_slice(&active.to_le_bytes());
        let pending = pending_by_consumer
            .get(consumer.name.as_slice())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        super::rdb_encode_length(buf, pending.len());
        for entry in pending {
            append_stream_id::<DIRECT_IDS>(buf, entry.entry_id_ms, entry.entry_id_seq);
        }
    }
    Some(())
}

fn stream_id_bytes_reference(ms: u64, seq: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&ms.to_be_bytes());
    bytes.extend_from_slice(&seq.to_be_bytes());
    bytes
}

fn stream_id_bytes(ms: u64, seq: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&ms.to_be_bytes());
    bytes[8..].copy_from_slice(&seq.to_be_bytes());
    bytes
}

fn encode_stream_id_string<const DIRECT_IDS: bool>(buf: &mut Vec<u8>, ms: u64, seq: u64) {
    if DIRECT_IDS {
        super::rdb_encode_string(buf, &stream_id_bytes(ms, seq));
    } else {
        super::rdb_encode_string(buf, &stream_id_bytes_reference(ms, seq));
    }
}

fn append_stream_id<const DIRECT_IDS: bool>(buf: &mut Vec<u8>, ms: u64, seq: u64) {
    if DIRECT_IDS {
        buf.extend_from_slice(&ms.to_be_bytes());
        buf.extend_from_slice(&seq.to_be_bytes());
    } else {
        buf.extend_from_slice(&stream_id_bytes_reference(ms, seq));
    }
}

/// Encode `value` as a listpack integer element, byte-for-byte matching
/// upstream `lpEncodeIntegerGetType` across all six width buckets (7-bit, 13-,
/// 16-, 24-, 32-, 64-bit) plus the trailing backlen.
/// (BlackThrush 2026-08-26) `#[inline(always)]`, not `#[inline]`: the plain hint
/// is DECLINED by LLVM for bodies this size and leaves the call count untouched --
/// see 9d7be9b44, where it moved the ratio 0.1 pct and the profile not at all.
/// This is an out-of-line call once per listpack element on the RDB save path.
#[inline(always)]
fn encode_listpack_int(buf: &mut Vec<u8>, value: i64) {
    let start = buf.len();
    if (0..=127).contains(&value) {
        buf.push(value as u8);
    } else if (-4096..=4095).contains(&value) {
        let n = if value < 0 {
            (1i64 << 13) + value
        } else {
            value
        };
        buf.push(((n >> 8) as u8) | 0xC0);
        buf.push((n & 0xFF) as u8);
    } else if (-32768..=32767).contains(&value) {
        let n = if value < 0 {
            (1i64 << 16) + value
        } else {
            value
        };
        buf.push(0xF1);
        buf.push((n & 0xFF) as u8);
        buf.push((n >> 8) as u8);
    } else if (-8_388_608..=8_388_607).contains(&value) {
        let n = if value < 0 {
            (1i64 << 24) + value
        } else {
            value
        };
        buf.push(0xF2);
        buf.push((n & 0xFF) as u8);
        buf.push(((n >> 8) & 0xFF) as u8);
        buf.push((n >> 16) as u8);
    } else if (-2_147_483_648..=2_147_483_647).contains(&value) {
        let n = if value < 0 {
            (1i64 << 32) + value
        } else {
            value
        };
        buf.push(0xF3);
        buf.push((n & 0xFF) as u8);
        buf.push(((n >> 8) & 0xFF) as u8);
        buf.push(((n >> 16) & 0xFF) as u8);
        buf.push((n >> 24) as u8);
    } else {
        buf.push(0xF4);
        buf.extend_from_slice(&(value as u64).to_le_bytes());
    }
    encode_listpack_backlen(buf, buf.len() - start);
}

/// Mirror upstream `lpStringToInt64`: `Some(v)` iff `s` is the canonical decimal
/// form of an i64 that `lpAppend` would store as an integer rather than a
/// string (no leading zeros, optional single `-`, no `-0`, fits in i64).
fn listpack_string_to_int64(s: &[u8]) -> Option<i64> {
    if s.is_empty() || s.len() >= 21 {
        return None;
    }
    if s.len() == 1 && s[0] == b'0' {
        return Some(0);
    }
    let (negative, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, s),
    };
    let (first, rest) = digits.split_first()?;
    if !(b'1'..=b'9').contains(first) {
        return None;
    }
    let mut v: u64 = (first - b'0') as u64;
    for &c in rest {
        if !c.is_ascii_digit() {
            return None;
        }
        let d = (c - b'0') as u64;
        v = v.checked_mul(10)?.checked_add(d)?;
    }
    if negative {
        // Allow magnitude up to 2^63 (i64::MIN); larger overflows.
        if v > (1u64 << 63) {
            return None;
        }
        Some(-(v as i128) as i64)
    } else {
        if v > i64::MAX as u64 {
            return None;
        }
        Some(v as i64)
    }
}

/// Append a stream field/value element exactly as upstream `lpAppend` does:
/// integer-encode when the bytes are a canonical i64, otherwise string-encode.
/// (BlackThrush 2026-08-26) `#[inline(always)]`, not `#[inline]`: the plain hint
/// is DECLINED by LLVM for bodies this size and leaves the call count untouched --
/// see 9d7be9b44, where it moved the ratio 0.1 pct and the profile not at all.
/// This is an out-of-line call once per listpack element on the RDB save path.
#[inline(always)]
fn encode_listpack_bytes(buf: &mut Vec<u8>, data: &[u8]) -> Option<()> {
    if let Some(value) = listpack_string_to_int64(data) {
        encode_listpack_int(buf, value);
        return Some(());
    }
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

/// (BlackThrush 2026-08-26) `#[inline]`: this is an out-of-line CALL 57,200 times
/// per 200-key stream DEBUG RELOAD -- once per listpack element -- for a body whose
/// common case is one compare and one `Vec::push`. Counted at 23 instructions per
/// call, where the work itself is a handful.
#[inline(always)]
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
    let (skeleton, cursor) = UpstreamStreamSkeleton::decode(type_byte, data)?;
    // The owned form: one allocation per field name and per value, exactly as
    // before this split. Callers that immediately rebuild a store representation
    // should use `UpstreamStreamSkeleton::entries` instead and skip all of it.
    let entries = skeleton.owned_entries()?;
    Ok((skeleton.into_rdb_value(entries), cursor))
}

/// An upstream stream record with its macro-node listpacks decompressed but its
/// ENTRIES not yet decoded.
///
/// Splitting the decode in two is what lets entries borrow: a stream entry's
/// field names and values are ranges inside a decompressed macro-node blob, and
/// the blobs used to be per-iteration locals, so the only way to return entries
/// was to copy every field onto the heap. Holding the blobs here instead means
/// [`Self::entries`] can hand out `Cow::Borrowed` slices that live exactly as
/// long as `self`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamStreamSkeleton {
    /// The record's declared LIVE entry count, used to size the decode's output
    /// vectors once for the whole record instead of once per node.
    stream_length: usize,
    /// `(master_ms, master_seq, decompressed listpack)` per radix-tree node.
    nodes: Vec<(u64, u64, Vec<u8>)>,
    watermark: Option<(u64, u64)>,
    groups: Vec<RdbStreamConsumerGroup>,
    metadata: RdbStreamMetadata,
    entries_added: u64,
    max_deleted: Option<(u64, u64)>,
}

impl UpstreamStreamSkeleton {
    /// Decode every entry in the record, borrowing field names and values
    /// directly out of the retained macro-node blobs.
    pub fn entries(&self) -> Result<Vec<BorrowedStreamEntry<'_>>, UpstreamStreamError> {
        self.decode_entries()
    }

    /// Every entry's fields in ONE vector, with a per-entry `(id, offset, len)`
    /// index alongside.
    ///
    /// `entries()` gives each entry its own `Vec<(F, F)>`: 40 heap allocations,
    /// 40 frees and 40 drops per 40-entry stream, for vectors that hold two
    /// borrowed slices each and are consumed immediately. The consumer wants
    /// `&[(F, F)]` per entry either way ([`PackedStreamLog::from_sorted_entries`]
    /// takes exactly that), so the fields can live end-to-end in one allocation
    /// and each entry can be a subslice.
    pub fn flat_entries(&self) -> Result<StreamOut<Cow<'_, [u8]>>, UpstreamStreamError> {
        let mut out = StreamOut::default();
        // ONE growth for the whole record. The per-NODE reserve inside
        // `decode_stream_listpack` still grows an ACCUMULATING vector once per
        // node, and a growth on an accumulating buffer copies the whole buffer --
        // measured at 10,760 Ir per 400-entry decode for three `reserve` calls,
        // 4.95 pct of this frame. The record's own header declares the live entry
        // count, so the final size is known before the first node is touched.
        // Capacity is a hint: a wrong one costs a growth, never content.
        out.reserve_for_record(self.stream_length);
        for (master_ms, master_seq, blob) in &self.nodes {
            let lp = decode_raw_values(blob)?;
            decode_stream_listpack::<_, true, false>(&lp, blob, *master_ms, *master_seq, &mut out)?;
        }
        Ok(out)
    }

    /// The record's DECLARED live entry count.
    ///
    /// Authoritative once validate_entries has returned Ok: the decode compares the
    /// number of live entries it walked against this and errors on a mismatch. That
    /// is what lets a retained (undecoded) stream answer len() without decoding.
    #[must_use]
    pub fn declared_len(&self) -> usize {
        self.stream_length
    }

    /// Walk and VALIDATE every entry without materialising any of it.
    ///
    /// Accepts exactly what [`Self::flat_entries`] accepts and rejects exactly what
    /// it rejects -- same walk, same trailer and count checks, same errors -- but
    /// pushes nothing. Prerequisite for a RESTORE that retains the macro-node blobs
    /// instead of decoding them: fr must keep rejecting corrupt payloads that
    /// default-redis accepts (9a6f6c487), so the validation cannot simply be
    /// dropped along with the materialisation.
    /// Returns the GREATEST entry id in the record, or None when it holds no live
    /// entries.
    ///
    /// Handing this back is what keeps a retained (undecoded) stream undecoded:
    /// RESTORE needs a last-generated-id when the record carries no watermark, and
    /// reading it off the built log would materialize the very thing retention
    /// exists to defer. The ids are already collected here for the duplicate check,
    /// so the maximum is free.
    pub fn validate_entries(&self) -> Result<Option<(u64, u64)>, UpstreamStreamError> {
        let mut sink: StreamOut<std::borrow::Cow<'_, [u8]>> = StreamOut::default();
        for (master_ms, master_seq, blob) in &self.nodes {
            let lp = decode_raw_values(blob)?;
            decode_stream_listpack::<_, true, true>(&lp, blob, *master_ms, *master_seq, &mut sink)?;
        }
        // DUPLICATE IDS, checked here because a deferred materialisation cannot.
        //
        // The RESTORE caller rejects a payload whose ids repeat -- today that
        // happens inside its build closure, where a non-monotonic payload falls
        // back to a per-entry `insert` and a repeat is an error. A stream held
        // UNDECODED has no such moment, so the check has to live with the rest of
        // the validation or fr would start accepting malformed payloads it rejects
        // now.
        //
        // Out-of-order but UNIQUE ids stay ACCEPTED, matching today's behaviour --
        // the fallback path tolerates reordering and only duplicates are fatal.
        // Sorting a copy of the ids is O(n log n) on 16-byte keys and touches none
        // of the field data, which is the whole point of not materialising.
        let mut ids: Vec<(u64, u64)> = sink.ids().collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(UpstreamStreamError::InconsistentEntryCount);
        }
        Ok(ids.last().copied())
    }

    /// The owned form, for callers that must outlive the blobs (`RdbValue::Stream`).
    ///
    /// This instantiates the SAME decoder with `F = Vec<u8>`, so the owned path
    /// allocates once per field exactly as it did before the borrowed split --
    /// it never builds a `Cow` intermediate only to convert and drop it. Going
    /// through `Cow` here cost the DEBUG RELOAD arm +0.75 pct, which is about 41
    /// extra allocations per op: one per entry plus the outer vector.
    pub fn owned_entries(&self) -> Result<Vec<StreamEntry>, UpstreamStreamError> {
        self.decode_entries()
    }

    fn decode_entries<'blob, F>(
        &'blob self,
    ) -> Result<Vec<EncodableStreamEntry<F, F>>, UpstreamStreamError>
    where
        F: From<Cow<'blob, [u8]>> + AsRef<[u8]>,
    {
        let mut out: StreamOut<F> = StreamOut::default();
        for (master_ms, master_seq, blob) in &self.nodes {
            let lp = decode_raw_values(blob)?;
            decode_stream_listpack::<_, false, false>(
                &lp,
                blob,
                *master_ms,
                *master_seq,
                &mut out,
            )?;
        }
        Ok(out.entries)
    }

    #[must_use]
    pub fn watermark(&self) -> Option<(u64, u64)> {
        self.watermark
    }

    #[must_use]
    pub fn entries_added(&self) -> u64 {
        self.entries_added
    }

    #[must_use]
    pub fn max_deleted(&self) -> Option<(u64, u64)> {
        self.max_deleted
    }

    /// The verbatim upstream record body this skeleton was decoded from, and its
    /// type byte. Re-encoding a loaded stream writes these back unchanged, which
    /// is both cheaper than re-deriving a payload from decoded entries and the
    /// more faithful round trip -- the same reason the blob-carrying variants
    /// exist on the save side.
    #[must_use]
    pub fn upstream_payload(&self) -> &[u8] {
        &self.metadata.upstream_payload
    }

    #[must_use]
    pub fn upstream_type_byte(&self) -> u8 {
        self.metadata.upstream_type_byte
    }

    /// The retained raw upstream record, kept for byte-exact replay.
    #[must_use]
    pub fn metadata(&self) -> &RdbStreamMetadata {
        &self.metadata
    }

    #[must_use]
    pub fn groups(&self) -> &[RdbStreamConsumerGroup] {
        &self.groups
    }

    #[must_use]
    pub fn into_groups(self) -> Vec<RdbStreamConsumerGroup> {
        self.groups
    }

    /// Consume the skeleton, pairing it with already-decoded owned entries.
    fn into_rdb_value(self, entries: Vec<StreamEntry>) -> RdbValue {
        RdbValue::Stream(
            entries,
            self.watermark,
            self.groups,
            Some(self.metadata),
            Some(self.entries_added),
            self.max_deleted,
        )
    }

    pub fn decode(type_byte: u8, data: &[u8]) -> Result<(Self, usize), UpstreamStreamError> {
        let is_v2_or_later = match type_byte {
            crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS => false,
            crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2 => true,
            crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3 => true,
            other => return Err(UpstreamStreamError::UnsupportedTypeByte(other)),
        };
        let is_v3 = type_byte == crate::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3;

        let mut cursor = 0usize;

        // (1) Listpacks count.
        let (listpacks_count, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        // Bounded by the declared count but capped: a corrupt length must not make us
        // reserve gigabytes before the first blob is even read.
        let mut nodes: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(listpacks_count.min(256));

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
            // (BlackThrush 2026-08-26) The blob is RETAINED, not decoded here. Entries
            // borrow their field names and values out of it, so it has to outlive them
            // -- decoding in this loop meant `lp_bytes` died at the end of the
            // iteration and every field had to be copied onto the heap to escape.
            nodes.push((master_ms, master_seq, lp_bytes));
        }

        // (3) Stream length (total entry count).
        let (stream_length, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;

        // (4) last_id.ms, last_id.seq (always present).
        let (last_id_ms, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;
        let (last_id_seq, c) =
            rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
        cursor += c;

        // (5) v2/v3 extras: first_id.ms, first_id.seq, max_deleted_id.ms,
        //     max_deleted_id.seq, entries_added (indices 0..5).
        let mut entries_added = u64::try_from(stream_length).unwrap_or(u64::MAX);
        let mut max_deleted_ms = 0u64;
        let mut max_deleted_seq = 0u64;
        if is_v2_or_later {
            for index in 0..5 {
                let (_v, c) =
                    rdb_decode_length(&data[cursor..]).ok_or(UpstreamStreamError::InvalidLength)?;
                match index {
                    2 => max_deleted_ms = u64::try_from(_v).unwrap_or(0),
                    3 => max_deleted_seq = u64::try_from(_v).unwrap_or(0),
                    4 => entries_added = u64::try_from(_v).unwrap_or(u64::MAX),
                    _ => {}
                }
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
                // seen_time (type 19+), then active_time (type 21+); both mstime_t.
                // An active_time of -1 is upstream's "never actively consumed"
                // sentinel. (frankenredis-sq4ov)
                let mut seen_time_ms = 0u64;
                let mut active_time_ms: Option<u64> = None;
                if is_v2_or_later {
                    seen_time_ms = take_millisecond_time(data, cursor)?;
                    cursor += 8;
                }
                if is_v3 {
                    let raw = take_millisecond_time(data, cursor)?;
                    cursor += 8;
                    active_time_ms = if raw as i64 == -1 { None } else { Some(raw) };
                }
                consumers.push(RdbStreamConsumer {
                    name: consumer_name.clone(),
                    seen_time_ms,
                    active_time_ms,
                });
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
        let max_deleted = if max_deleted_ms == 0 && max_deleted_seq == 0 {
            None
        } else {
            Some((max_deleted_ms, max_deleted_seq))
        };
        Ok((
            Self {
                stream_length,
                nodes,
                watermark,
                groups,
                metadata,
                entries_added,
                max_deleted,
            },
            cursor,
        ))
    }
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
/// Where a stream decode accumulates, in one of two shapes chosen by the caller.
///
/// `FLAT == false` is the historical shape: a `Vec<(F, F)>` per entry, which is
/// what `RdbValue::Stream` has to hold. `FLAT == true` puts every entry's fields
/// end-to-end in `fields` and records `(id, offset, len)` per entry, so a
/// 40-entry stream makes ONE allocation for its fields instead of forty.
///
/// Two shapes, ONE walk: the entry format is byte-exactness-critical and must not
/// be transcribed twice (the listpack size rule already taught that lesson). The
/// const flag lets the compiler delete whichever half a given instantiation does
/// not use, so the owned path's codegen is unchanged -- the same reason
/// `from_sorted_entries` gates its field cache on a const rather than a runtime
/// check.
pub struct StreamOut<F> {
    /// `FLAT == false` only.
    entries: Vec<EncodableStreamEntry<F, F>>,
    /// `FLAT == true` only: every entry's fields, back to back.
    fields: Vec<(F, F)>,
    /// `FLAT == true` only: `(ms, seq, offset into `fields`, field count)`.
    index: Vec<(u64, u64, u32, u32)>,
    /// `FLAT == true` only: total VALUE bytes, summed as they are decoded so the
    /// consumer can size its arena in one allocation. Free here -- the length is
    /// already in hand -- and a second walk to recover it would not be.
    value_bytes: usize,
}

impl<F> Default for StreamOut<F> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            fields: Vec::new(),
            index: Vec::new(),
            value_bytes: 0,
        }
    }
}

impl<F> StreamOut<F> {
    /// Pre-size both vectors from the record's declared entry count. `fields` is
    /// sized for two fields per entry, the overwhelmingly common stream shape; a
    /// wider schema simply grows once more.
    fn reserve_for_record(&mut self, entries: usize) {
        self.index.reserve(entries);
        self.fields.reserve(entries.saturating_mul(2));
    }

    /// Total field slots across all entries -- an exact UPPER BOUND on the number
    /// of distinct field names, for sizing a consumer's dictionary index.
    #[must_use]
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Bytes to reserve for a consumer that appends, per field, a varint field
    /// index, a varint value length and the value itself.
    ///
    /// `PackedStreamLog` grows its arena from EMPTY across every append, and a
    /// growth on a buffer that accumulates costs the whole buffer, not one
    /// element. Two bytes per field covers both varints for any realistic schema;
    /// capacity is a hint, so an underestimate costs one growth and never content.
    #[must_use]
    pub fn arena_hint(&self) -> usize {
        self.value_bytes
            .saturating_add(self.fields.len().saturating_mul(2))
    }

    /// Entry ids in decode order, for the caller's strictly-increasing check.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = (u64, u64)> + '_ {
        self.index.iter().map(|&(ms, seq, _, _)| (ms, seq))
    }

    /// `((ms, seq), fields)` per entry, each a subslice of the one `fields` vector.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = ((u64, u64), &[(F, F)])> + '_ {
        self.index.iter().map(|&(ms, seq, off, len)| {
            let start = off as usize;
            ((ms, seq), &self.fields[start..start + len as usize])
        })
    }
}

fn decode_stream_listpack<'blob, F, const FLAT: bool, const VALIDATE_ONLY: bool>(
    lp: &[RawListpackValue],
    blob: &'blob [u8],
    master_ms: u64,
    master_seq: u64,
    out: &mut StreamOut<F>,
) -> Result<(), UpstreamStreamError>
where
    F: From<Cow<'blob, [u8]>> + AsRef<[u8]>,
{
    let mut idx = 0usize;
    let declared_live_count = take_usize(lp, &mut idx)?;
    let declared_deleted_count = take_usize(lp, &mut idx)?;
    let declared_total_count = declared_live_count
        .checked_add(declared_deleted_count)
        .ok_or(UpstreamStreamError::InconsistentEntryCount)?;
    let master_field_count = take_usize(lp, &mut idx)?;
    let mut master_fields: Vec<Cow<'blob, [u8]>> = Vec::with_capacity(master_field_count);
    for _ in 0..master_field_count {
        master_fields.push(take_string(lp, blob, &mut idx)?);
    }
    // Master terminator: integer 0.
    let terminator = take_int(lp, &mut idx)?;
    if terminator != 0 {
        return Err(UpstreamStreamError::InconsistentEntryTrailer);
    }

    // One growth per NODE instead of one every few entries. The node's own header
    // declares how many live entries it holds and how many fields the master
    // carries, which is the exact size for the SAMEFIELDS shape and a good guess
    // otherwise. Capacity is a hint: a wrong one costs a growth, never content.
    if FLAT && !VALIDATE_ONLY {
        out.index.reserve(declared_live_count);
        out.fields
            .reserve(declared_live_count.saturating_mul(master_field_count.max(1)));
    }

    let mut decoded_total_count = 0usize;
    let mut decoded_live_count = 0usize;
    // Reused across entries; see the assignment inside the loop.
    let mut fields: Vec<(F, F)> = Vec::new();
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

        // FLAT appends into the shared vector and remembers where this entry
        // started, so a tombstone can roll back to it; the per-entry vector is
        // never allocated (`Vec::new` does not touch the heap and the const makes
        // its drop a no-op the compiler removes).
        let mark = out.fields.len();
        // Only the !FLAT arm ever owns a per-entry vector. Declaring it OUTSIDE the
        // entry loop and assigning here means the FLAT arm never touches it, so it
        // is constructed and dropped once for the whole node instead of leaving an
        // empty `Vec` to be dropped per entry -- 40 `drop_glue` calls per op that
        // the const branch could not remove on its own.
        if !FLAT {
            fields = Vec::with_capacity(field_count);
        }
        if same_fields {
            for master_name in master_fields.iter().take(field_count) {
                let value = take_string(lp, blob, &mut idx)?;
                // For the BORROWED instantiation, cloning a `Cow::Borrowed` copies
                // a slice reference, not bytes. This used to clone a `Vec<u8>` per
                // field per entry -- 80 heap allocations and 80 memcpys on a
                // 40-entry x 2-field stream -- whose only consumer is
                // `intern_field`, which maps every copy straight back to the same
                // dictionary index.
                let pair = (F::from(master_name.clone()), F::from(value));
                if VALIDATE_ONLY {
                    drop(pair);
                } else if FLAT {
                    out.value_bytes = out.value_bytes.saturating_add(pair.1.as_ref().len());
                    out.fields.push(pair);
                } else {
                    fields.push(pair);
                }
            }
        } else {
            for _ in 0..field_count {
                let name = take_string(lp, blob, &mut idx)?;
                let value = take_string(lp, blob, &mut idx)?;
                let pair = (F::from(name), F::from(value));
                if VALIDATE_ONLY {
                    drop(pair);
                } else if FLAT {
                    out.value_bytes = out.value_bytes.saturating_add(pair.1.as_ref().len());
                    out.fields.push(pair);
                } else {
                    fields.push(pair);
                }
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
            // Tombstone: its fields were consumed to keep the cursor aligned but
            // must not reach the output.
            if FLAT && !VALIDATE_ONLY {
                out.fields.truncate(mark);
            }
            continue;
        }
        decoded_live_count = decoded_live_count
            .checked_add(1)
            .ok_or(UpstreamStreamError::InconsistentEntryCount)?;
        let ms = combine_u64_i64(master_ms, ms_delta);
        let seq = combine_u64_i64(master_seq, seq_delta);
        if VALIDATE_ONLY {
            // Ids ARE recorded -- `validate_entries` needs them for the duplicate
            // check -- but no field bytes are. That is the whole saving: 16 bytes
            // per entry instead of every name and value.
            out.index.push((ms, seq, 0, 0));
        } else if FLAT {
            let len = u32::try_from(out.fields.len() - mark)
                .map_err(|_| UpstreamStreamError::InvalidFieldCount)?;
            let off = u32::try_from(mark).map_err(|_| UpstreamStreamError::InvalidFieldCount)?;
            out.index.push((ms, seq, off, len));
        } else {
            out.entries.push((ms, seq, std::mem::take(&mut fields)));
        }
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

fn take_int(lp: &[RawListpackValue], idx: &mut usize) -> Result<i64, UpstreamStreamError> {
    let v = lp
        .get(*idx)
        .ok_or(UpstreamStreamError::ShortListpackEntries)?;
    *idx += 1;
    match v.kind() {
        RawKind::Integer(n) => Ok(n),
        RawKind::String(_) => Err(UpstreamStreamError::ExpectedListpackInteger),
    }
}

fn take_usize(lp: &[RawListpackValue], idx: &mut usize) -> Result<usize, UpstreamStreamError> {
    let n = take_int(lp, idx)?;
    if n < 0 {
        return Err(UpstreamStreamError::InvalidFieldCount);
    }
    usize::try_from(n).map_err(|_| UpstreamStreamError::InvalidFieldCount)
}

fn take_string<'blob>(
    lp: &[RawListpackValue],
    blob: &'blob [u8],
    idx: &mut usize,
) -> Result<Cow<'blob, [u8]>, UpstreamStreamError> {
    let v = lp
        .get(*idx)
        .ok_or(UpstreamStreamError::ShortListpackEntries)?;
    *idx += 1;
    // Upstream writes field names + values via lpAppend; integer values get
    // packed as LP_ENCODING_*_INT but were byte-strings on the write side
    // (stream arg processing calls lpAppend, not lpAppendInteger, for
    // field/value pairs). So integers here should not occur for user-visible
    // fields -- but in practice an integer-looking value CAN be packed as an
    // int. Match upstream's listpackGetValue, which returns a
    // decimal-stringified integer; the Integer arm below is that case.
    //
    // ZERO allocations for the string case, which is every user-visible field and
    // value: the entry BORROWS the decompressed macro-node blob, which the
    // `UpstreamStreamSkeleton` keeps alive for exactly as long as the entries do.
    // Only the integer case owns, because the decimal rendering has to live
    // somewhere -- and upstream writes field/value pairs with `lpAppend`, not
    // `lpAppendInteger`, so it is the rare arm.
    Ok(match v.kind() {
        RawKind::Integer(n) => Cow::Owned(crate::decimal_i64_bytes(n)),
        RawKind::String(range) => Cow::Borrowed(
            blob.get(range.start as usize..range.end as usize)
                .ok_or(UpstreamStreamError::ShortListpackEntries)?,
        ),
    })
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

    /// The duplicate-id REJECT branch, exercised directly.
    ///
    /// Byte corruption is unlikely to manufacture a repeated id, so the oracle test
    /// above can pass without ever reaching this branch. An untested reject path in
    /// the validator is exactly what would let a deferred materialisation start
    /// accepting payloads RESTORE rejects today.
    #[test]
    fn validate_entries_rejects_duplicate_ids_and_accepts_reordered_unique_ones() {
        let dup: Vec<StreamEntry> = vec![
            (7, 1, vec![(b"f".to_vec(), b"a".to_vec())]),
            (7, 1, vec![(b"f".to_vec(), b"b".to_vec())]),
        ];
        if let Some(payload) =
            encode_upstream_stream_listpacks3(&dup, Some((7, 1)), &[], Some(2), None)
            && let Ok((skeleton, _)) =
                UpstreamStreamSkeleton::decode(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
        {
            assert!(
                skeleton.validate_entries().is_err(),
                "a payload with two entries at 7-1 must be rejected"
            );
        }

        // Out-of-order but UNIQUE must still be ACCEPTED -- today's fallback path
        // tolerates reordering and only duplicates are fatal. Tightening this would
        // be a silent compatibility regression on foreign payloads.
        let reordered: Vec<StreamEntry> = vec![
            (9, 1, vec![(b"f".to_vec(), b"a".to_vec())]),
            (3, 1, vec![(b"f".to_vec(), b"b".to_vec())]),
        ];
        if let Some(payload) =
            encode_upstream_stream_listpacks3(&reordered, Some((9, 1)), &[], Some(2), None)
            && let Ok((skeleton, _)) =
                UpstreamStreamSkeleton::decode(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
        {
            assert_eq!(
                skeleton.validate_entries().is_ok(),
                skeleton.flat_entries().is_ok(),
                "reordered-but-unique ids must be accepted exactly as before"
            );
        }
    }

    /// `validate_entries` must be a pure ORACLE for `flat_entries`: same accepts,
    /// same rejects, on the same bytes.
    ///
    /// A validate-only walk is only useful to a RESTORE that retains the macro-node
    /// blobs if it rejects EXACTLY what the materialising decode rejects. fr accepts
    /// FOREIGN payloads here and rejects corruption that default-redis waves through
    /// (9a6f6c487), so a validator that is even slightly more permissive would hand
    /// that property away silently.
    ///
    /// Corrupts one byte at a time across the whole record and asserts the two agree
    /// on every single one -- not just that both accept the clean payload.
    #[test]
    fn validate_entries_accepts_and_rejects_exactly_what_flat_entries_does() {
        let entries: Vec<StreamEntry> = (1..=24u64)
            .map(|i| {
                (
                    i,
                    1,
                    vec![
                        (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()),
                        (b"shared".to_vec(), vec![b'x'; (i % 7) as usize]),
                    ],
                )
            })
            .collect();
        let payload =
            encode_upstream_stream_listpacks3(&entries, Some((24, 1)), &[], Some(24), None)
                .expect("encodes");

        let clean = UpstreamStreamSkeleton::decode(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
            .expect("clean payload decodes")
            .0;
        assert!(
            clean.flat_entries().is_ok(),
            "clean payload must materialise"
        );
        assert!(
            clean.validate_entries().is_ok(),
            "clean payload must validate"
        );

        let mut agreed = 0_u32;
        let mut both_rejected = 0_u32;
        for offset in 0..payload.len() {
            for xor in [0x01_u8, 0xff] {
                let mut bad = payload.clone();
                bad[offset] ^= xor;
                let Ok((skeleton, _)) =
                    UpstreamStreamSkeleton::decode(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &bad)
                else {
                    // Rejected before either walk runs; nothing to compare.
                    continue;
                };
                // The reference predicate is what RESTORE ACTUALLY ACCEPTS, which is
                // `flat_entries` succeeding AND the ids being unique -- the caller's
                // build closure rejects a repeat. `validate_entries` folds both in,
                // so it is compared against both, not against `flat_entries` alone.
                let reference_ok = match skeleton.flat_entries() {
                    Err(_) => false,
                    Ok(flat) => {
                        let mut ids: Vec<(u64, u64)> = flat.ids().collect();
                        ids.sort_unstable();
                        !ids.windows(2).any(|w| w[0] == w[1])
                    }
                };
                let validate_ok = skeleton.validate_entries().is_ok();
                assert_eq!(
                    reference_ok, validate_ok,
                    "validator disagrees with RESTORE's acceptance at byte {offset} ^ {xor:#04x}: \
                     reference ok={reference_ok} validate_entries ok={validate_ok}"
                );
                let flat_ok = reference_ok;
                agreed += 1;
                if !flat_ok {
                    both_rejected += 1;
                }
            }
        }
        // The corpus must actually exercise the REJECT path, or agreement is vacuous.
        assert!(
            both_rejected > 0,
            "no corruption was rejected by either path -- the oracle test proves nothing"
        );
        assert!(agreed > 100, "too few comparable corruptions: {agreed}");
    }

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
            RdbValue::Stream(entries, watermark, groups, _, _, _) => {
                Some((entries, watermark, groups))
            }
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
            consumers: vec![
                RdbStreamConsumer::named(b"alice".to_vec()),
                RdbStreamConsumer::named(b"bob".to_vec()),
            ],
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

        let payload =
            encode_upstream_stream_listpacks3(&entries, Some((1001, 1)), &groups, None, None)
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
                consumers: vec![
                    RdbStreamConsumer::named(b"alice".to_vec()),
                    RdbStreamConsumer::named(b"bob".to_vec()),
                ],
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
    fn encode_type21_round_trips_max_deleted_entry_id() {
        let entries = vec![(1001, 0, vec![(b"name".to_vec(), b"bob".to_vec())])];
        let payload = encode_upstream_stream_listpacks3(
            &entries,
            Some((1001, 0)),
            &[],
            Some(2),
            Some((1000, 0)),
        )
        .expect("encode type21 payload");

        let (value, consumed) =
            decode_upstream_stream_skeleton(UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3, &payload)
                .expect("decode encoded payload");
        assert_eq!(consumed, payload.len());

        let RdbValue::Stream(
            decoded_entries,
            watermark,
            groups,
            _metadata,
            entries_added,
            max_deleted,
        ) = value
        else {
            panic!("expected decoded stream");
        };
        assert_eq!(decoded_entries, entries);
        assert_eq!(watermark, Some((1001, 0)));
        assert_eq!(groups, Vec::<RdbStreamConsumerGroup>::new());
        assert_eq!(entries_added, Some(2));
        assert_eq!(max_deleted, Some((1000, 0)));
    }

    #[test]
    fn encode_type21_declines_pending_consumer_missing_from_group() {
        let entries = vec![(1000, 0, vec![(b"field".to_vec(), b"value".to_vec())])];
        let groups = vec![RdbStreamConsumerGroup {
            name: b"group".to_vec(),
            last_delivered_id_ms: 1000,
            last_delivered_id_seq: 0,
            entries_read: None,
            consumers: vec![RdbStreamConsumer::named(b"alice".to_vec())],
            pending: vec![RdbStreamPendingEntry {
                entry_id_ms: 1000,
                entry_id_seq: 0,
                consumer: b"bob".to_vec(),
                deliveries: 1,
                last_delivered_ms: 5000,
            }],
        }];

        assert!(
            encode_upstream_stream_listpacks3(&entries, Some((1000, 0)), &groups, None, None)
                .is_none()
        );
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
                    consumers: vec![RdbStreamConsumer::named(b"consumer".to_vec())],
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

            let payload =
                encode_upstream_stream_listpacks3(&entries, Some(watermark), &groups, None, None)
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
        assert_eq!(
            group.consumers,
            vec![
                RdbStreamConsumer {
                    name: b"alice".to_vec(),
                    seen_time_ms: 1100,
                    active_time_ms: Some(1200),
                },
                RdbStreamConsumer {
                    name: b"bob".to_vec(),
                    seen_time_ms: 1300,
                    active_time_ms: Some(1400),
                },
            ]
        );
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
        assert_eq!(
            group.consumers,
            vec![RdbStreamConsumer {
                name: b"alice".to_vec(),
                seen_time_ms: 1100,
                active_time_ms: None,
            }]
        );
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
            consumers: vec![RdbStreamConsumer::named(b"alice".to_vec())],
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

        let payload =
            encode_upstream_stream_listpacks3(&entries, Some((11, 0)), &groups, None, None)
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
