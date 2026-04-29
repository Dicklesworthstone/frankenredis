#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use fr_store::{
    Store, StoreError, StreamAutoClaimOptions, StreamClaimOptions, StreamField, StreamGroupInfo,
    StreamGroupReadCursor, StreamGroupReadOptions, StreamId, StreamPendingRecord,
    StreamPendingSummary,
};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};

const MAX_INPUT_LEN: usize = 4_096;
const MAX_OPS: usize = 64;
const MAX_BLOB_LEN: usize = 24;
const MAX_FIELDS: usize = 4;
const STREAM_KEY: &[u8] = b"fuzz:stream";
const WRONG_TYPE_KEY: &[u8] = b"fuzz:string";
const RESTORED_STREAM_KEY: &[u8] = b"fuzz:stream:restored";

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    ops: Vec<StreamOp>,
}

#[derive(Debug, Arbitrary)]
enum StreamOp {
    Add {
        fields: Vec<(Blob, Blob)>,
    },
    Del {
        limit: u8,
        from_back: bool,
    },
    Trim {
        max_len: u8,
    },
    GroupCreate {
        group: Blob,
        mkstream: bool,
        cursor: CursorHint,
    },
    GroupDestroy {
        group: Blob,
    },
    GroupSetId {
        group: Blob,
        cursor: CursorHint,
    },
    CreateConsumer {
        group: Blob,
        consumer: Blob,
    },
    ReadNew {
        group: Blob,
        consumer: Blob,
        count: Option<u8>,
        noack: bool,
    },
    ReadPending {
        group: Blob,
        consumer: Blob,
        count: Option<u8>,
        start: CursorHint,
    },
    Ack {
        group: Blob,
        limit: u8,
    },
    DelConsumer {
        group: Blob,
        consumer: Blob,
    },
    Claim {
        group: Blob,
        consumer: Blob,
        limit: u8,
        justid: bool,
        min_idle_ms: u8,
    },
    AutoClaim {
        group: Blob,
        consumer: Blob,
        count: u8,
        justid: bool,
        min_idle_ms: u8,
        start: CursorHint,
    },
    Info {
        group: Blob,
    },
    WrongType {
        kind: WrongTypeOp,
        group: Blob,
        consumer: Blob,
    },
    RoundTrip,
}

#[derive(Debug, Clone, Arbitrary)]
struct Blob(Vec<u8>);

#[derive(Debug, Clone, Copy, Arbitrary)]
enum CursorHint {
    Zero,
    FirstStream,
    MiddleStream,
    LastStream,
    FirstPending,
    LastPending,
    Max,
}

#[derive(Debug, Clone, Copy, Arbitrary)]
enum WrongTypeOp {
    GroupCreate,
    ReadNew,
    ReadPending,
    PendingSummary,
    PendingEntries,
    InfoGroups,
    CreateConsumer,
    InfoConsumers,
    DelConsumer,
    Ack,
    Claim,
    AutoClaim,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupSnapshot {
    name: Vec<u8>,
    info: StreamGroupInfo,
    consumers: Vec<(Vec<u8>, usize, u64)>,
    summary: StreamPendingSummary,
    pending: Vec<StreamPendingRecord>,
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut unstructured = Unstructured::new(data);
    let Ok(input) = FuzzInput::arbitrary(&mut unstructured) else {
        return;
    };

    fuzz_stream_groups(input);
});

fn fuzz_stream_groups(input: FuzzInput) {
    let mut store = Store::new();
    store.set(WRONG_TYPE_KEY.to_vec(), b"sentinel".to_vec(), None, 0);

    let mut now_ms = 1_u64;
    let mut next_stream_id = 1_u64;

    for (step_index, op) in input.ops.into_iter().take(MAX_OPS).enumerate() {
        apply_stream_op(&mut store, &mut next_stream_id, op, now_ms);
        assert_stream_group_invariants(&mut store, STREAM_KEY, now_ms);
        if step_index % 8 == 7 {
            assert_dump_restore_round_trip(&mut store, now_ms);
        }
        now_ms = now_ms.saturating_add(1 + (step_index % 3) as u64);
    }

    let _ = store.to_aof_commands(now_ms);
    assert_stream_group_invariants(&mut store, STREAM_KEY, now_ms);
    assert_dump_restore_round_trip(&mut store, now_ms);
}

fn apply_stream_op(store: &mut Store, next_stream_id: &mut u64, op: StreamOp, now_ms: u64) {
    match op {
        StreamOp::Add { fields } => {
            let id = (*next_stream_id, 0);
            *next_stream_id = (*next_stream_id).saturating_add(1);
            let fields = normalize_fields(fields);
            let _ = store.xadd(STREAM_KEY, id, &fields, now_ms);
        }
        StreamOp::Del { limit, from_back } => {
            let records = current_stream_records(store, STREAM_KEY, now_ms);
            let count = usize::from(limit % 4);
            if count == 0 || records.is_empty() {
                return;
            }
            let ids: Vec<StreamId> = if from_back {
                records
                    .iter()
                    .rev()
                    .take(count)
                    .map(|(id, _)| *id)
                    .collect()
            } else {
                records.iter().take(count).map(|(id, _)| *id).collect()
            };
            let _ = store.xdel(STREAM_KEY, &ids, now_ms);
        }
        StreamOp::Trim { max_len } => {
            // Store::xtrim now takes an `Option<usize>` LIMIT
            // parameter (Redis 6.2 dialect); pass None to preserve
            // the prior fuzz behavior (full trim down to max_len).
            let _ = store.xtrim(STREAM_KEY, usize::from(max_len % 8), None, now_ms);
        }
        StreamOp::GroupCreate {
            group,
            mkstream,
            cursor,
        } => {
            let group = group_name(&group);
            let start_id = resolve_cursor(store, STREAM_KEY, &group, None, cursor, now_ms);
            let _ = store.xgroup_create(STREAM_KEY, &group, start_id, mkstream, now_ms);
        }
        StreamOp::GroupDestroy { group } => {
            let group = group_name(&group);
            let _ = store.xgroup_destroy(STREAM_KEY, &group, now_ms);
        }
        StreamOp::GroupSetId { group, cursor } => {
            let group = group_name(&group);
            let id = resolve_cursor(store, STREAM_KEY, &group, None, cursor, now_ms);
            let _ = store.xgroup_setid(STREAM_KEY, &group, id, now_ms);
        }
        StreamOp::CreateConsumer { group, consumer } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let _ = store.xgroup_createconsumer(STREAM_KEY, &group, &consumer, now_ms);
        }
        StreamOp::ReadNew {
            group,
            consumer,
            count,
            noack,
        } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let _ = store.xreadgroup(
                STREAM_KEY,
                &group,
                &consumer,
                StreamGroupReadOptions {
                    cursor: StreamGroupReadCursor::NewEntries,
                    noack,
                    count: normalize_count(count),
                },
                now_ms,
            );
        }
        StreamOp::ReadPending {
            group,
            consumer,
            count,
            start,
        } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let start_id =
                resolve_cursor(store, STREAM_KEY, &group, Some(&consumer), start, now_ms);
            let _ = store.xreadgroup(
                STREAM_KEY,
                &group,
                &consumer,
                StreamGroupReadOptions {
                    cursor: StreamGroupReadCursor::Id(start_id),
                    noack: false,
                    count: normalize_count(count),
                },
                now_ms,
            );
        }
        StreamOp::Ack { group, limit } => {
            let group = group_name(&group);
            let ids: Vec<StreamId> =
                current_pending_entries(store, STREAM_KEY, &group, None, now_ms)
                    .into_iter()
                    .take(usize::from(limit % 4))
                    .map(|(id, _, _, _)| id)
                    .collect();
            let _ = store.xack(STREAM_KEY, &group, &ids, now_ms);
        }
        StreamOp::DelConsumer { group, consumer } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let _ = store.xgroup_delconsumer(STREAM_KEY, &group, &consumer, now_ms);
        }
        StreamOp::Claim {
            group,
            consumer,
            limit,
            justid,
            min_idle_ms,
        } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let ids: Vec<StreamId> =
                current_pending_entries(store, STREAM_KEY, &group, None, now_ms)
                    .into_iter()
                    .take(usize::from(limit % 4))
                    .map(|(id, _, _, _)| id)
                    .collect();
            let _ = store.xclaim(
                STREAM_KEY,
                &group,
                &consumer,
                &ids,
                StreamClaimOptions {
                    min_idle_time_ms: u64::from(min_idle_ms % 16),
                    idle_ms: None,
                    time_ms: None,
                    retry_count: None,
                    force: false,
                    justid,
                    last_id: None,
                },
                now_ms,
            );
        }
        StreamOp::AutoClaim {
            group,
            consumer,
            count,
            justid,
            min_idle_ms,
            start,
        } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let start_id = resolve_cursor(store, STREAM_KEY, &group, None, start, now_ms);
            let _ = store.xautoclaim(
                STREAM_KEY,
                &group,
                &consumer,
                start_id,
                StreamAutoClaimOptions {
                    min_idle_time_ms: u64::from(min_idle_ms % 16),
                    count: usize::from(count % 4),
                    justid,
                },
                now_ms,
            );
        }
        StreamOp::Info { group } => {
            let group = group_name(&group);
            let _ = store.xinfo_stream(STREAM_KEY, now_ms);
            let _ = store.xinfo_groups(STREAM_KEY, now_ms);
            let _ = store.xinfo_consumers(STREAM_KEY, &group, now_ms);
            let _ = store.xpending_summary(STREAM_KEY, &group, now_ms);
            let _ = store.xpending_entries(
                STREAM_KEY,
                &group,
                ((0, 0), (u64::MAX, u64::MAX)),
                usize::MAX,
                None,
                now_ms,
                0,
            );
        }
        StreamOp::WrongType {
            kind,
            group,
            consumer,
        } => {
            let group = group_name(&group);
            let consumer = consumer_name(&consumer);
            let before = current_stream_records(store, STREAM_KEY, now_ms);
            let before_groups = group_snapshots(store, STREAM_KEY, now_ms);
            let wrong_type_result = match kind {
                WrongTypeOp::GroupCreate => store
                    .xgroup_create(WRONG_TYPE_KEY, &group, (0, 0), true, now_ms)
                    .map(|_| ()),
                WrongTypeOp::ReadNew => store
                    .xreadgroup(
                        WRONG_TYPE_KEY,
                        &group,
                        &consumer,
                        StreamGroupReadOptions {
                            cursor: StreamGroupReadCursor::NewEntries,
                            noack: false,
                            count: Some(1),
                        },
                        now_ms,
                    )
                    .map(|_| ()),
                WrongTypeOp::ReadPending => store
                    .xreadgroup(
                        WRONG_TYPE_KEY,
                        &group,
                        &consumer,
                        StreamGroupReadOptions {
                            cursor: StreamGroupReadCursor::Id((0, 0)),
                            noack: false,
                            count: Some(1),
                        },
                        now_ms,
                    )
                    .map(|_| ()),
                WrongTypeOp::PendingSummary => store
                    .xpending_summary(WRONG_TYPE_KEY, &group, now_ms)
                    .map(|_| ()),
                WrongTypeOp::PendingEntries => store
                    .xpending_entries(
                        WRONG_TYPE_KEY,
                        &group,
                        ((0, 0), (u64::MAX, u64::MAX)),
                        8,
                        None,
                        now_ms,
                        0,
                    )
                    .map(|_| ()),
                WrongTypeOp::InfoGroups => store.xinfo_groups(WRONG_TYPE_KEY, now_ms).map(|_| ()),
                WrongTypeOp::CreateConsumer => store
                    .xgroup_createconsumer(WRONG_TYPE_KEY, &group, &consumer, now_ms)
                    .map(|_| ()),
                WrongTypeOp::InfoConsumers => store
                    .xinfo_consumers(WRONG_TYPE_KEY, &group, now_ms)
                    .map(|_| ()),
                WrongTypeOp::DelConsumer => store
                    .xgroup_delconsumer(WRONG_TYPE_KEY, &group, &consumer, now_ms)
                    .map(|_| ()),
                WrongTypeOp::Ack => store
                    .xack(WRONG_TYPE_KEY, &group, &[(0, 0)], now_ms)
                    .map(|_| ()),
                WrongTypeOp::Claim => store
                    .xclaim(
                        WRONG_TYPE_KEY,
                        &group,
                        &consumer,
                        &[(0, 0)],
                        StreamClaimOptions {
                            min_idle_time_ms: 0,
                            idle_ms: None,
                            time_ms: None,
                            retry_count: None,
                            force: false,
                            justid: false,
                            last_id: None,
                        },
                        now_ms,
                    )
                    .map(|_| ()),
                WrongTypeOp::AutoClaim => store
                    .xautoclaim(
                        WRONG_TYPE_KEY,
                        &group,
                        &consumer,
                        (0, 0),
                        StreamAutoClaimOptions {
                            min_idle_time_ms: 0,
                            count: 1,
                            justid: false,
                        },
                        now_ms,
                    )
                    .map(|_| ()),
            };
            assert_eq!(wrong_type_result, Err(StoreError::WrongType));
            assert_eq!(current_stream_records(store, STREAM_KEY, now_ms), before);
            assert_eq!(group_snapshots(store, STREAM_KEY, now_ms), before_groups);
        }
        StreamOp::RoundTrip => assert_dump_restore_round_trip(store, now_ms),
    }
}

fn assert_stream_group_invariants(store: &mut Store, key: &[u8], now_ms: u64) {
    let live_records = current_stream_records(store, key, now_ms);
    let live_ids: BTreeSet<StreamId> = live_records.iter().map(|(id, _)| *id).collect();
    let snapshots = group_snapshots(store, key, now_ms);

    for snapshot in snapshots {
        assert_eq!(
            snapshot.info.1,
            snapshot.consumers.len(),
            "xinfo_groups consumer count must match xinfo_consumers length",
        );
        assert_eq!(
            snapshot.info.2,
            snapshot.pending.len(),
            "xinfo_groups pending count must match xpending entries length",
        );

        let total_from_summary = snapshot
            .summary
            .3
            .iter()
            .map(|(_, count)| *count)
            .sum::<usize>();
        assert_eq!(
            snapshot.summary.0, total_from_summary,
            "xpending summary total must equal per-consumer totals",
        );
        assert_eq!(
            snapshot.summary.0,
            snapshot.pending.len(),
            "xpending summary total must equal materialized pending entries",
        );
        assert_eq!(
            snapshot.summary.1,
            snapshot.pending.first().map(|(id, _, _, _)| *id),
            "xpending summary lower bound must track the first pending id",
        );
        assert_eq!(
            snapshot.summary.2,
            snapshot.pending.last().map(|(id, _, _, _)| *id),
            "xpending summary upper bound must track the last pending id",
        );

        let mut pending_by_consumer: BTreeMap<Vec<u8>, Vec<StreamPendingRecord>> = BTreeMap::new();
        for record in &snapshot.pending {
            assert!(
                live_ids.contains(&record.0),
                "pending ids must reference live stream records",
            );
            pending_by_consumer
                .entry(record.1.clone())
                .or_default()
                .push(record.clone());
        }

        let summary_counts: BTreeMap<Vec<u8>, usize> = snapshot.summary.3.iter().cloned().collect();
        let consumer_names: BTreeSet<Vec<u8>> = snapshot
            .consumers
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        assert!(
            pending_by_consumer
                .keys()
                .all(|name| consumer_names.contains(name)),
            "pending entries must belong to known consumers",
        );

        for (consumer, pending_count, idle_ms) in &snapshot.consumers {
            let pending_entries = pending_by_consumer
                .get(consumer)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            assert_eq!(
                *pending_count,
                pending_entries.len(),
                "xinfo_consumers pending count must match pending entries",
            );
            assert_eq!(
                summary_counts.get(consumer).copied().unwrap_or(0),
                *pending_count,
                "xpending per-consumer counts must match xinfo_consumers",
            );
            assert_eq!(
                *idle_ms,
                pending_entries
                    .iter()
                    .map(|(_, _, pending_idle_ms, _)| *pending_idle_ms)
                    .max()
                    .unwrap_or(0),
                "xinfo_consumers idle time must match max pending idle time",
            );

            let replayed_result = store.xreadgroup(
                key,
                &snapshot.name,
                consumer,
                StreamGroupReadOptions {
                    cursor: StreamGroupReadCursor::Id((0, 0)),
                    noack: false,
                    count: None,
                },
                now_ms,
            );
            assert!(
                replayed_result.is_ok(),
                "pending replay on an existing stream key must not error",
            );
            let replayed = replayed_result.ok().flatten().unwrap_or_default();
            let replayed_ids: Vec<StreamId> = replayed.into_iter().map(|(id, _)| id).collect();
            let expected_ids: Vec<StreamId> =
                pending_entries.iter().map(|(id, _, _, _)| *id).collect();
            assert_eq!(
                replayed_ids, expected_ids,
                "pending replay must surface the same ids tracked in XPENDING for a consumer",
            );
        }
    }
}

fn assert_dump_restore_round_trip(store: &mut Store, now_ms: u64) {
    let Some(payload) = store.dump_key(STREAM_KEY, now_ms) else {
        return;
    };

    let expected_records = current_stream_records(store, STREAM_KEY, now_ms);
    let expected_groups = group_snapshots(store, STREAM_KEY, now_ms);
    let group_state = store
        .stream_consumer_groups(STREAM_KEY)
        .cloned()
        .unwrap_or_default();

    let mut restored = Store::new();
    assert!(
        restored
            .restore_key(RESTORED_STREAM_KEY, 0, &payload, false, now_ms)
            .is_ok(),
        "dumped stream payload must restore cleanly",
    );
    for (group_name, group) in group_state {
        restored.restore_stream_group(
            RESTORED_STREAM_KEY,
            group_name,
            group.last_delivered_id,
            group.consumers,
            group.pending,
        );
    }

    assert_eq!(
        current_stream_records(&mut restored, RESTORED_STREAM_KEY, now_ms),
        expected_records,
        "stream dump/restore must preserve stream records",
    );
    assert_stream_group_invariants(&mut restored, RESTORED_STREAM_KEY, now_ms);
    assert_eq!(
        group_snapshots(&mut restored, RESTORED_STREAM_KEY, now_ms),
        expected_groups,
        "manual stream group restore must preserve consumer-group state",
    );
}

fn group_snapshots(store: &mut Store, key: &[u8], now_ms: u64) -> Vec<GroupSnapshot> {
    let groups_result = store.xinfo_groups(key, now_ms);
    assert!(
        groups_result.is_ok(),
        "stream group snapshot must not fail for the fuzzed stream key",
    );
    let Ok(groups_opt) = groups_result else {
        return Vec::new();
    };
    let Some(groups) = groups_opt else {
        return Vec::new();
    };

    groups
        .into_iter()
        .map(|info| {
            let name = info.0.clone();
            let consumers_result = store.xinfo_consumers(key, &name, now_ms);
            assert!(
                consumers_result.is_ok(),
                "xinfo_consumers must not error for known group",
            );
            let consumers = consumers_result.ok().flatten().unwrap_or_default();
            let summary_result = store.xpending_summary(key, &name, now_ms);
            assert!(
                summary_result.is_ok(),
                "xpending_summary must not error for known group",
            );
            let summary = summary_result
                .ok()
                .flatten()
                .unwrap_or((0, None, None, Vec::new()));
            let pending_result = store.xpending_entries(
                key,
                &name,
                ((0, 0), (u64::MAX, u64::MAX)),
                usize::MAX,
                None,
                now_ms,
                0,
            );
            assert!(
                pending_result.is_ok(),
                "xpending_entries must not error for known group",
            );
            let pending = pending_result.ok().flatten().unwrap_or_default();
            GroupSnapshot {
                name,
                info,
                consumers,
                summary,
                pending,
            }
        })
        .collect()
}

fn current_stream_records(
    store: &mut Store,
    key: &[u8],
    now_ms: u64,
) -> Vec<(StreamId, Vec<StreamField>)> {
    store
        .xrange(key, (0, 0), (u64::MAX, u64::MAX), None, now_ms)
        .unwrap_or_default()
}

fn current_pending_entries(
    store: &mut Store,
    key: &[u8],
    group: &[u8],
    consumer: Option<&[u8]>,
    now_ms: u64,
) -> Vec<StreamPendingRecord> {
    match store.xpending_entries(
        key,
        group,
        ((0, 0), (u64::MAX, u64::MAX)),
        usize::MAX,
        consumer,
        now_ms,
        0,
    ) {
        Ok(Some(entries)) => entries,
        Ok(None) | Err(_) => Vec::new(),
    }
}

fn resolve_cursor(
    store: &mut Store,
    key: &[u8],
    group: &[u8],
    consumer: Option<&[u8]>,
    hint: CursorHint,
    now_ms: u64,
) -> StreamId {
    let records = current_stream_records(store, key, now_ms);
    let pending = current_pending_entries(store, key, group, consumer, now_ms);
    match hint {
        CursorHint::Zero => (0, 0),
        CursorHint::FirstStream => records.first().map(|(id, _)| *id).unwrap_or((0, 0)),
        CursorHint::MiddleStream => records
            .get(records.len().saturating_sub(1) / 2)
            .map(|(id, _)| *id)
            .unwrap_or((0, 0)),
        CursorHint::LastStream => records.last().map(|(id, _)| *id).unwrap_or((0, 0)),
        CursorHint::FirstPending => pending.first().map(|(id, _, _, _)| *id).unwrap_or((0, 0)),
        CursorHint::LastPending => pending.last().map(|(id, _, _, _)| *id).unwrap_or((0, 0)),
        CursorHint::Max => (u64::MAX, u64::MAX),
    }
}

fn normalize_count(count: Option<u8>) -> Option<usize> {
    count.map(|value| usize::from(value % 5))
}

fn normalize_fields(fields: Vec<(Blob, Blob)>) -> Vec<StreamField> {
    let mut fields: Vec<StreamField> = fields
        .into_iter()
        .take(MAX_FIELDS)
        .map(|(field, value)| {
            (
                field.bytes_or_default(b"field"),
                value.bytes_or_default(b"value"),
            )
        })
        .collect();
    if fields.is_empty() {
        fields.push((b"field".to_vec(), b"value".to_vec()));
    }
    fields
}

fn group_name(group: &Blob) -> Vec<u8> {
    group.bytes_or_default(b"group")
}

fn consumer_name(consumer: &Blob) -> Vec<u8> {
    consumer.bytes_or_default(b"consumer")
}

impl Blob {
    fn bytes_or_default(&self, fallback: &[u8]) -> Vec<u8> {
        let mut bytes = self.0.clone();
        bytes.truncate(MAX_BLOB_LEN);
        if bytes.is_empty() {
            fallback.to_vec()
        } else {
            bytes
        }
    }
}
