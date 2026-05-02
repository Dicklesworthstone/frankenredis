#![forbid(unsafe_code)]

use fr_expire::evaluate_expiry;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Bound::{Excluded, Included, Unbounded};

/// Redis-compatible version string. Single source of truth for all version reporting.
pub const REDIS_COMPAT_VERSION: &str = "7.2.0";

const RDB_DUMP_VERSION: u16 = 11;
const RDB_TYPE_STRING: u8 = 0;
const RDB_TYPE_LIST: u8 = 1;
const RDB_TYPE_SET: u8 = 2;
const RDB_TYPE_HASH: u8 = 4;
const RDB_TYPE_ZSET_2: u8 = 5;
const RDB_TYPE_SET_INTSET: u8 = 11;
const RDB_TYPE_HASH_LISTPACK: u8 = 16;
const RDB_TYPE_ZSET_LISTPACK: u8 = 17;
const RDB_TYPE_LIST_QUICKLIST_2: u8 = 18;
const RDB_TYPE_STREAM_LISTPACKS: u8 = fr_persist::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS;
const RDB_TYPE_STREAM_LISTPACKS_2: u8 = fr_persist::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_2;
const RDB_TYPE_SET_LISTPACK: u8 = 20;
const RDB_TYPE_STREAM_LISTPACKS_3: u8 = fr_persist::UPSTREAM_RDB_TYPE_STREAM_LISTPACKS_3;
const RDB_OPCODE_FUNCTION2: u8 = 245;
const DUMP_VERSION_LEN: usize = 2;
const DUMP_CRC64_LEN: usize = 8;
const DUMP_TRAILER_LEN: usize = DUMP_VERSION_LEN + DUMP_CRC64_LEN;
const RDB_ENCVAL: u8 = 0xC0;
const RDB_ENC_INT8: u8 = 0;
const RDB_ENC_INT16: u8 = 1;
const RDB_ENC_INT32: u8 = 2;
const RDB_ENC_LZF: u8 = 3;
const RDB_STRING_MAX_ALLOC: usize = 536_870_912;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitRangeUnit {
    Byte,
    Bit,
}

thread_local! {
    static TOUCH_DISABLED: Cell<bool> = const { Cell::new(false) };
}

struct TouchGuard {
    previous: bool,
}

impl TouchGuard {
    fn new(disabled: bool) -> Self {
        let previous = TOUCH_DISABLED.with(|flag| {
            let prev = flag.get();
            flag.set(disabled);
            prev
        });
        Self { previous }
    }
}

impl Drop for TouchGuard {
    fn drop(&mut self) {
        TOUCH_DISABLED.with(|flag| flag.set(self.previous));
    }
}

fn touch_disabled() -> bool {
    TOUCH_DISABLED.with(Cell::get)
}

/// Temporarily disable LRU/LFU touch updates for the current thread.
pub fn with_touch_disabled<T>(disabled: bool, f: impl FnOnce() -> T) -> T {
    let _guard = TouchGuard::new(disabled);
    f()
}

// ── Keyspace notification flags (matching Redis server.h) ───────────
pub const NOTIFY_KEYSPACE: u32 = 1 << 0; // K
pub const NOTIFY_KEYEVENT: u32 = 1 << 1; // E
pub const NOTIFY_GENERIC: u32 = 1 << 2; // g
pub const NOTIFY_STRING: u32 = 1 << 3; // $
pub const NOTIFY_LIST: u32 = 1 << 4; // l
pub const NOTIFY_SET: u32 = 1 << 5; // s
pub const NOTIFY_HASH: u32 = 1 << 6; // h
pub const NOTIFY_ZSET: u32 = 1 << 7; // z
pub const NOTIFY_EXPIRED: u32 = 1 << 8; // x
pub const NOTIFY_EVICTED: u32 = 1 << 9; // e
pub const NOTIFY_STREAM: u32 = 1 << 10; // t
pub const NOTIFY_KEY_MISS: u32 = 1 << 11; // m
pub const NOTIFY_NEW: u32 = 1 << 12; // n
pub const NOTIFY_ALL: u32 = NOTIFY_GENERIC
    | NOTIFY_STRING
    | NOTIFY_LIST
    | NOTIFY_SET
    | NOTIFY_HASH
    | NOTIFY_ZSET
    | NOTIFY_EXPIRED
    | NOTIFY_EVICTED
    | NOTIFY_STREAM;

/// Parse a notify-keyspace-events configuration string into flags.
/// Returns None if the string contains invalid characters.
#[must_use]
pub fn keyspace_events_parse(classes: &str) -> Option<u32> {
    let mut flags = 0u32;
    for c in classes.chars() {
        match c {
            'A' => flags |= NOTIFY_ALL,
            'g' => flags |= NOTIFY_GENERIC,
            '$' => flags |= NOTIFY_STRING,
            'l' => flags |= NOTIFY_LIST,
            's' => flags |= NOTIFY_SET,
            'h' => flags |= NOTIFY_HASH,
            'z' => flags |= NOTIFY_ZSET,
            'x' => flags |= NOTIFY_EXPIRED,
            'e' => flags |= NOTIFY_EVICTED,
            'K' => flags |= NOTIFY_KEYSPACE,
            'E' => flags |= NOTIFY_KEYEVENT,
            't' => flags |= NOTIFY_STREAM,
            'm' => flags |= NOTIFY_KEY_MISS,
            'n' => flags |= NOTIFY_NEW,
            _ => return None,
        }
    }
    // Redis requires at least K or E to be set for notifications to fire.
    // If event types are specified but neither K nor E is set, disable all.
    if flags != 0 && (flags & (NOTIFY_KEYSPACE | NOTIFY_KEYEVENT)) == 0 {
        flags = 0;
    }
    Some(flags)
}

/// Convert notification flags back to a configuration string.
///
/// Mirrors upstream `notify.c::keyspaceEventsFlagsToString`: canonical
/// order is A | g $ l s h z x e t n | K E m, where the `n` (NOTIFY_NEW)
/// bit only appears when `A` is NOT set (upstream lists `n` inside the
/// per-class else branch). This matters for CONFIG GET parity:
/// `CONFIG SET notify-keyspace-events KEA` must echo back `AKE`,
/// not `KEA`. (br-frankenredis-xmev)
#[must_use]
pub fn keyspace_events_to_string(flags: u32) -> String {
    let mut s = String::new();
    if (flags & NOTIFY_ALL) == NOTIFY_ALL {
        s.push('A');
    } else {
        if flags & NOTIFY_GENERIC != 0 {
            s.push('g');
        }
        if flags & NOTIFY_STRING != 0 {
            s.push('$');
        }
        if flags & NOTIFY_LIST != 0 {
            s.push('l');
        }
        if flags & NOTIFY_SET != 0 {
            s.push('s');
        }
        if flags & NOTIFY_HASH != 0 {
            s.push('h');
        }
        if flags & NOTIFY_ZSET != 0 {
            s.push('z');
        }
        if flags & NOTIFY_EXPIRED != 0 {
            s.push('x');
        }
        if flags & NOTIFY_EVICTED != 0 {
            s.push('e');
        }
        if flags & NOTIFY_STREAM != 0 {
            s.push('t');
        }
        if flags & NOTIFY_NEW != 0 {
            s.push('n');
        }
    }
    if flags & NOTIFY_KEYSPACE != 0 {
        s.push('K');
    }
    if flags & NOTIFY_KEYEVENT != 0 {
        s.push('E');
    }
    if flags & NOTIFY_KEY_MISS != 0 {
        s.push('m');
    }
    s
}

pub type StreamId = (u64, u64);
pub type StreamField = (Vec<u8>, Vec<u8>);
pub type StreamEntries = BTreeMap<StreamId, Vec<StreamField>>;
pub type StreamRecord = (StreamId, Vec<StreamField>);
pub type StreamInfoBounds = (usize, Option<StreamRecord>, Option<StreamRecord>);
/// (name, pending_count, idle_ms)
pub type StreamConsumerInfo = (Vec<u8>, usize, u64);
pub type StreamPendingEntries = BTreeMap<StreamId, StreamPendingEntry>;
pub type StreamPendingSummaryConsumer = (Vec<u8>, usize);
pub type StreamPendingSummary = (
    usize,
    Option<StreamId>,
    Option<StreamId>,
    Vec<StreamPendingSummaryConsumer>,
);
pub type StreamPendingRecord = (StreamId, Vec<u8>, u64, u64);
pub type StreamAutoClaimDeleted = Vec<StreamId>;

/// An asynchronous server message queued for delivery to a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubMessage {
    /// Direct channel subscription match: `["message", channel, data]`.
    Message { channel: Vec<u8>, data: Vec<u8> },
    /// Pattern subscription match: `["pmessage", pattern, channel, data]`.
    PMessage {
        pattern: Vec<u8>,
        channel: Vec<u8>,
        data: Vec<u8>,
    },
    /// Shard-channel subscription match: `["smessage", channel, data]`.
    SMessage { channel: Vec<u8>, data: Vec<u8> },
    /// Client-side caching invalidation: `["invalidate", [keys...]]`.
    Invalidate { keys: Vec<Vec<u8>> },
}

/// A single SLOWLOG entry recording a command that exceeded the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowlogEntry {
    pub id: u64,
    pub timestamp_sec: u64,
    pub duration_us: u64,
    pub argv: Vec<Vec<u8>>,
}

/// Score bound for sorted set range queries (ZRANGEBYSCORE, ZCOUNT, etc.).
/// Supports inclusive (default), exclusive (`(` prefix), and infinity (`-inf`/`+inf`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScoreBound {
    Inclusive(f64),
    Exclusive(f64),
}

/// Numerical f64 of a score bound, used to detect inverted ranges
/// (start > end) without panicking the BTreeMap range walker.
#[inline]
fn score_bound_value(bound: ScoreBound) -> f64 {
    match bound {
        ScoreBound::Inclusive(s) | ScoreBound::Exclusive(s) => s,
    }
}

impl ScoreBound {
    /// Check if `score` satisfies this bound as a minimum (lower bound).
    pub fn check_min(self, score: f64) -> bool {
        match self {
            ScoreBound::Inclusive(v) => score >= v,
            ScoreBound::Exclusive(v) => score > v,
        }
    }

    /// Check if `score` satisfies this bound as a maximum (upper bound).
    pub fn check_max(self, score: f64) -> bool {
        match self {
            ScoreBound::Inclusive(v) => score <= v,
            ScoreBound::Exclusive(v) => score < v,
        }
    }
}

/// Check if a score falls within the given min/max bounds.
pub fn score_in_range(score: f64, min: ScoreBound, max: ScoreBound) -> bool {
    min.check_min(score) && max.check_max(score)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPendingEntry {
    pub consumer: Vec<u8>,
    pub deliveries: u64,
    pub last_delivered_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamClaimOptions {
    pub min_idle_time_ms: u64,
    pub idle_ms: Option<u64>,
    pub time_ms: Option<u64>,
    pub retry_count: Option<u64>,
    pub force: bool,
    pub justid: bool,
    pub last_id: Option<StreamId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamClaimReply {
    Entries(Vec<StreamRecord>),
    Ids(Vec<StreamId>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamAutoClaimOptions {
    pub min_idle_time_ms: u64,
    pub count: usize,
    pub justid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAutoClaimReply {
    Entries {
        next_start: StreamId,
        entries: Vec<StreamRecord>,
        deleted_ids: StreamAutoClaimDeleted,
    },
    Ids {
        next_start: StreamId,
        ids: Vec<StreamId>,
        deleted_ids: StreamAutoClaimDeleted,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamGroupReadCursor {
    NewEntries,
    Id(StreamId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGroupReadOptions {
    pub cursor: StreamGroupReadCursor,
    pub noack: bool,
    pub count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamGroup {
    pub last_delivered_id: StreamId,
    pub consumers: BTreeSet<Vec<u8>>,
    pub pending: StreamPendingEntries,
}

pub type StreamGroupState = BTreeMap<Vec<u8>, StreamGroup>;
pub type StreamGroupInfo = (Vec<u8>, usize, usize, StreamId);

#[derive(Debug, Clone, Copy, Default)]
pub struct ZaddOptions {
    pub nx: bool,
    pub xx: bool,
    pub gt: bool,
    pub lt: bool,
    pub ch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    ValueNotInteger,
    HashValueNotInteger,
    ValueNotFloat,
    IncrFloatNaN,
    IntegerOverflow,
    KeyNotFound,
    WrongType,
    InvalidHllValue,
    IndexOutOfRange,
    InvalidDumpPayload,
    BusyKey,
    GenericError(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortedSet {
    /// member -> score
    dict: HashMap<Vec<u8>, f64>,
    /// (score, member) -> ()
    /// We use ScoreMember wrapper to handle f64 comparison and lexicographical member tie-breaking.
    ordered: BTreeMap<ScoreMember, ()>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum MemberPart {
    Min,
    Actual(Vec<u8>),
    Max,
}

impl MemberPart {
    fn as_actual(&self) -> Option<&Vec<u8>> {
        match self {
            MemberPart::Actual(v) => Some(v),
            MemberPart::Min | MemberPart::Max => None,
        }
    }

    fn into_actual(self) -> Option<Vec<u8>> {
        match self {
            MemberPart::Actual(v) => Some(v),
            MemberPart::Min | MemberPart::Max => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ScoreMember {
    score: f64,
    member: MemberPart,
}

impl Eq for ScoreMember {}

impl PartialOrd for ScoreMember {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoreMember {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        canonicalize_zero_score(self.score)
            .total_cmp(&canonicalize_zero_score(other.score))
            .then_with(|| self.member.cmp(&other.member))
    }
}

impl ScoreMember {
    fn min_for_score(score: f64) -> Self {
        Self {
            score,
            member: MemberPart::Min,
        }
    }

    fn max_for_score(score: f64) -> Self {
        Self {
            score,
            member: MemberPart::Max,
        }
    }

    fn actual(score: f64, member: Vec<u8>) -> Self {
        Self {
            score,
            member: MemberPart::Actual(member),
        }
    }
}

impl SortedSet {
    fn new() -> Self {
        Self {
            dict: HashMap::new(),
            ordered: BTreeMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.dict.len()
    }

    fn is_empty(&self) -> bool {
        self.dict.is_empty()
    }

    fn insert(&mut self, member: Vec<u8>, score: f64) -> bool {
        let score = canonicalize_zero_score(score);
        if let Some(old_score) = self.dict.insert(member.clone(), score) {
            if old_score.total_cmp(&score).is_eq() {
                return false;
            }
            self.ordered
                .remove(&ScoreMember::actual(old_score, member.clone()));
            self.ordered.insert(ScoreMember::actual(score, member), ());
            return false;
        }
        self.ordered.insert(ScoreMember::actual(score, member), ());
        true
    }

    fn remove(&mut self, member: &[u8]) -> bool {
        if let Some(score) = self.dict.remove(member) {
            self.ordered
                .remove(&ScoreMember::actual(score, member.to_vec()));
            true
        } else {
            false
        }
    }

    fn get_score(&self, member: &[u8]) -> Option<f64> {
        self.dict.get(member).copied()
    }

    pub fn iter_asc(&self) -> impl Iterator<Item = (&Vec<u8>, &f64)> {
        self.ordered
            .keys()
            .filter_map(|sm| sm.member.as_actual().map(|member| (member, &sm.score)))
    }

    fn iter_desc(&self) -> impl Iterator<Item = (&Vec<u8>, &f64)> {
        self.ordered
            .keys()
            .rev()
            .filter_map(|sm| sm.member.as_actual().map(|member| (member, &sm.score)))
    }

    fn pop_min(&mut self) -> Option<(Vec<u8>, f64)> {
        while let Some(sm) = self.ordered.first_key_value().map(|(sm, _)| sm.clone()) {
            self.ordered.remove(&sm);
            if let Some(member) = sm.member.into_actual() {
                let score = sm.score;
                self.dict.remove(&member);
                return Some((member, score));
            }
        }
        None
    }

    fn pop_max(&mut self) -> Option<(Vec<u8>, f64)> {
        while let Some(sm) = self.ordered.last_key_value().map(|(sm, _)| sm.clone()) {
            self.ordered.remove(&sm);
            if let Some(member) = sm.member.into_actual() {
                let score = sm.score;
                self.dict.remove(&member);
                return Some((member, score));
            }
        }
        None
    }

    /// Iterate over (member, score) pairs in hash-map order (unordered).
    fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &f64)> {
        self.dict.iter()
    }

    /// Return an iterator over the member keys.
    fn keys(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.dict.keys()
    }
}

impl From<std::collections::HashMap<Vec<u8>, f64>> for SortedSet {
    fn from(map: std::collections::HashMap<Vec<u8>, f64>) -> Self {
        let mut ss = SortedSet::new();
        for (member, score) in map {
            ss.insert(member, score);
        }
        ss
    }
}

/// The inner value held by a key in the store.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(Vec<u8>),
    Hash(BTreeMap<Vec<u8>, Vec<u8>>),
    List(VecDeque<Vec<u8>>),
    Set(BTreeSet<Vec<u8>>),
    /// Sorted set: dual-indexed for efficiency.
    SortedSet(SortedSet),
    /// Stream entries keyed by `(milliseconds, sequence)` stream IDs.
    Stream(StreamEntries),
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    value: Value,
    expires_at_ms: Option<u64>,
    /// Last access timestamp in milliseconds (for OBJECT IDLETIME / LRU).
    last_access_ms: u64,
    /// LFU access frequency counter exposed via OBJECT FREQ when LFU eviction is active.
    lfu_freq: u8,
    /// Last LFU access/decrement timestamp in whole minutes.
    lfu_last_touch_min: u64,
    /// Monotonic modification counter (bumped on every write, used by WATCH).
    modification_count: u64,
    /// Upstream Redis 7.2 promotes a String value's encoding from
    /// embstr to raw on the first in-place mutation (APPEND,
    /// SETRANGE, GETSET-with-overwrite). The encoding is sticky —
    /// even if a later operation shrinks the string back below the
    /// 44-byte embstr threshold, OBJECT ENCODING continues to
    /// report 'raw'. Mirrored here as a per-entry flag.
    /// (br-frankenredis-84bv)
    force_raw_encoding: bool,
}

impl Entry {
    fn new(value: Value, expires_at_ms: Option<u64>, now_ms: u64) -> Self {
        Self {
            value,
            expires_at_ms,
            last_access_ms: now_ms,
            lfu_freq: 0,
            lfu_last_touch_min: now_ms / 60_000,
            modification_count: 0,
            force_raw_encoding: false,
        }
    }

    fn touch(&mut self, now_ms: u64) {
        if touch_disabled() {
            return;
        }
        self.last_access_ms = now_ms;
    }

    fn current_lfu_freq(&self, now_ms: u64, decay_time: u64) -> u8 {
        if decay_time == 0 {
            return self.lfu_freq;
        }
        let now_min = now_ms / 60_000;
        let elapsed = now_min.saturating_sub(self.lfu_last_touch_min);
        let periods = elapsed / decay_time;
        self.lfu_freq
            .saturating_sub(u8::try_from(periods).unwrap_or(u8::MAX))
    }

    fn bump_lfu_freq(&mut self, now_ms: u64, decay_time: u64) {
        self.lfu_freq = self.current_lfu_freq(now_ms, decay_time).saturating_add(1);
        self.lfu_last_touch_min = now_ms / 60_000;
    }

    fn bump_mod_count(&mut self) {
        self.modification_count = self.modification_count.wrapping_add(1);
    }

    fn touch_write(&mut self, now_ms: u64) {
        self.touch(now_ms);
        self.bump_mod_count();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttlValue {
    KeyMissing,
    NoExpiry,
    Remaining(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpireTimeValue {
    KeyMissing,
    NoExpiry,
    ExpiresAt(u64),
}

/// Conditional flag for `HEXPIRE` / `HPEXPIRE` / `HEXPIREAT` /
/// `HPEXPIREAT` — matches the upstream NX/XX/GT/LT semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFieldTtlCondition {
    /// Apply unconditionally (no flag on the wire).
    None,
    /// Apply only if the field currently has no TTL.
    Nx,
    /// Apply only if the field currently has a TTL.
    Xx,
    /// Apply only if the new deadline is strictly greater than the
    /// existing one; no-op if there's no existing TTL.
    Gt,
    /// Apply only if the new deadline is strictly less than the
    /// existing one; always applies when there's no existing TTL.
    Lt,
}

/// Outcome of `Store::hash_field_set_abs_expiry`, using upstream reply
/// codes:
/// - `Applied` → 1 (TTL set or updated)
/// - `AppliedAlreadyExpired` → 2 (deadline was past; caller should reap)
/// - `ConditionNotMet` → 0 (NX/XX/GT/LT blocked the update)
/// - `FieldMissing` → -2 (field not in the hash)
/// - `KeyMissing` → -2 (hash itself not found)
/// - `WrongType` → surfaced as an error reply at the command layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFieldTtlSet {
    Applied,
    AppliedAlreadyExpired,
    ConditionNotMet,
    FieldMissing,
    KeyMissing,
    WrongType,
}

/// Result of `Store::hash_field_ttl` (HTTL/HPTTL/HEXPIRETIME/HPEXPIRETIME).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFieldTtl {
    /// Time remaining (or absolute expiry, per `absolute`) in the
    /// requested unit.
    Remaining(u64),
    /// Field exists but has no TTL (reply code -1).
    NoTtl,
    /// Field does not exist on the hash (reply code -2).
    FieldMissing,
    /// The hash key itself does not exist (reply code -2).
    KeyMissing,
    /// Key exists but isn't a hash — WRONGTYPE on the wire.
    WrongType,
    /// Field has a TTL that has already elapsed; the caller must reap it
    /// before reporting to the client. Not currently emitted by the
    /// Store API (fields are reaped eagerly in set_abs_expiry); reserved
    /// for part 3 lazy-expiry hooks.
    Expired,
}

/// Unit selector for `Store::hash_field_ttl` — seconds for
/// HTTL/HEXPIRETIME, milliseconds for HPTTL/HPEXPIRETIME.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFieldTtlUnit {
    Seconds,
    Milliseconds,
}

/// Outcome of `Store::hash_field_persist` (HPERSIST):
/// - `Persisted` → 1 (TTL was present and has been cleared).
/// - `NoTtl` → -1 (field existed but had no TTL).
/// - `FieldMissing` → -2 (field not in the hash).
/// - `KeyMissing` → -2 (hash itself not found).
/// - `WrongType` → WRONGTYPE at the command layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashFieldPersistResult {
    Persisted,
    NoTtl,
    FieldMissing,
    KeyMissing,
    WrongType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveExpireCycleResult {
    pub sampled_keys: usize,
    pub evicted_keys: usize,
    pub next_cursor: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxmemoryPressureLevel {
    None,
    Soft,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxmemoryPressureState {
    pub maxmemory_bytes: usize,
    pub logical_usage_bytes: usize,
    pub not_counted_bytes: usize,
    pub counted_usage_bytes: usize,
    pub bytes_to_free: usize,
    pub level: MaxmemoryPressureLevel,
}

/// Maxmemory eviction policy — controls which keys are chosen for eviction
/// when memory pressure exceeds the maxmemory limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxmemoryPolicy {
    /// Don't evict anything, return errors on writes that exceed the limit.
    #[default]
    Noeviction,
    /// Evict any key, preferring the least recently used.
    AllkeysLru,
    /// Evict only keys with an expiry, preferring the least recently used.
    VolatileLru,
    /// Evict any key at random.
    AllkeysRandom,
    /// Evict only keys with an expiry at random.
    VolatileRandom,
    /// Evict keys with an expiry, preferring those closest to expiring.
    VolatileTtl,
    /// Evict any key, preferring the least frequently used (approximated as LRU).
    AllkeysLfu,
    /// Evict only keys with an expiry, preferring the least frequently used.
    VolatileLfu,
}

impl MaxmemoryPolicy {
    /// Parse from a Redis config string (e.g. "allkeys-lru").
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "noeviction" => Some(Self::Noeviction),
            "allkeys-lru" => Some(Self::AllkeysLru),
            "volatile-lru" => Some(Self::VolatileLru),
            "allkeys-random" => Some(Self::AllkeysRandom),
            "volatile-random" => Some(Self::VolatileRandom),
            "volatile-ttl" => Some(Self::VolatileTtl),
            "allkeys-lfu" => Some(Self::AllkeysLfu),
            "volatile-lfu" => Some(Self::VolatileLfu),
            _ => None,
        }
    }

    /// Return the Redis config string representation.
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Noeviction => "noeviction",
            Self::AllkeysLru => "allkeys-lru",
            Self::VolatileLru => "volatile-lru",
            Self::AllkeysRandom => "allkeys-random",
            Self::VolatileRandom => "volatile-random",
            Self::VolatileTtl => "volatile-ttl",
            Self::AllkeysLfu => "allkeys-lfu",
            Self::VolatileLfu => "volatile-lfu",
        }
    }

    #[must_use]
    pub fn tracks_lfu(self) -> bool {
        matches!(self, Self::AllkeysLfu | Self::VolatileLfu)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictionSafetyGateState {
    pub loading: bool,
    pub command_yielding: bool,
    pub replica_ignore_maxmemory: bool,
    pub asm_importing: bool,
    pub paused_for_evict: bool,
}

impl EvictionSafetyGateState {
    #[must_use]
    pub fn blocks_eviction(self) -> bool {
        self.loading
            || self.command_yielding
            || self.replica_ignore_maxmemory
            || self.asm_importing
            || self.paused_for_evict
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionLoopStatus {
    Ok,
    Running,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionLoopFailure {
    SafetyGateSuppressed,
    NoCandidates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySample {
    pub timestamp_sec: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LatencyTracker {
    events: HashMap<String, VecDeque<LatencySample>>,
    pub threshold_ms: u64,
}

impl LatencyTracker {
    const MAX_SAMPLES_PER_EVENT: usize = 160;

    pub fn record_sample(&mut self, event: &str, duration_ms: u64, now_sec: u64) {
        let samples = self.events.entry(event.to_string()).or_default();
        samples.push_back(LatencySample {
            timestamp_sec: now_sec,
            duration_ms,
        });
        while samples.len() > Self::MAX_SAMPLES_PER_EVENT {
            samples.pop_front();
        }
    }

    #[must_use]
    pub fn latest(&self) -> Vec<(String, LatencySample)> {
        let mut latest: Vec<(String, LatencySample)> = self
            .events
            .iter()
            .filter_map(|(event, samples)| {
                samples
                    .back()
                    .copied()
                    .map(|sample| (event.clone(), sample))
            })
            .collect();
        latest.sort_by(|left, right| left.0.cmp(&right.0));
        latest
    }

    #[must_use]
    pub fn history(&self, event: &str) -> Vec<LatencySample> {
        self.events
            .get(event)
            .map(|samples| samples.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn reset(&mut self, events: &[&str]) -> usize {
        if events.is_empty() {
            let count = self.events.len();
            self.events.clear();
            return count;
        }

        events
            .iter()
            .filter(|event| self.events.remove(**event).is_some())
            .count()
    }
}

/// Per-command latency histogram using power-of-2 microsecond buckets.
/// Buckets: [0, 1), [1, 2), [2, 4), [4, 8), ..., [2^22, 2^23), [2^23, ∞)
/// This gives ~8 second max tracked latency with 24 buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandHistogram {
    /// Bucket counts: bucket[i] covers [2^(i-1), 2^i) microseconds, except bucket[0] is [0, 1).
    buckets: [u64; 24],
    /// Total number of calls recorded.
    pub calls: u64,
}

impl CommandHistogram {
    /// Record a latency sample in microseconds.
    pub fn record(&mut self, latency_us: u64) {
        self.calls += 1;
        let bucket_idx = if latency_us == 0 {
            0
        } else {
            // Find the bucket: bucket i covers [2^(i-1), 2^i) for i > 0
            // Use leading zeros to find log2
            let log2 = 63_u32.saturating_sub(latency_us.leading_zeros());
            // Bucket 0 is [0,1), bucket 1 is [1,2), bucket 2 is [2,4), etc.
            // So bucket index = log2 + 1, clamped to 23 max
            (log2 + 1).min(23) as usize
        };
        self.buckets[bucket_idx] += 1;
    }

    /// Get histogram data as (bucket_start_us, count) pairs for non-zero buckets.
    #[must_use]
    pub fn to_buckets(&self) -> Vec<(u64, u64)> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|&(_, count)| *count > 0)
            .map(|(i, count)| {
                let start_us = if i == 0 { 0 } else { 1_u64 << (i - 1) };
                (start_us, *count)
            })
            .collect()
    }

    /// Reset the histogram.
    pub fn reset(&mut self) {
        self.buckets = [0; 24];
        self.calls = 0;
    }
}

/// Tracks per-command latency histograms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandHistogramTracker {
    histograms: HashMap<String, CommandHistogram>,
}

impl CommandHistogramTracker {
    /// Record a command latency in microseconds.
    pub fn record(&mut self, command: &str, latency_us: u64) {
        self.histograms
            .entry(command.to_ascii_uppercase())
            .or_default()
            .record(latency_us);
    }

    /// Get histogram for a specific command.
    #[must_use]
    pub fn get(&self, command: &str) -> Option<&CommandHistogram> {
        self.histograms.get(&command.to_ascii_uppercase())
    }

    /// Get all histograms, sorted by command name.
    #[must_use]
    pub fn all(&self) -> Vec<(&str, &CommandHistogram)> {
        let mut result: Vec<_> = self
            .histograms
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        result.sort_by(|a, b| a.0.cmp(b.0));
        result
    }

    /// Reset histograms for specified commands, or all if empty.
    pub fn reset(&mut self, commands: &[&str]) -> usize {
        if commands.is_empty() {
            let count = self.histograms.len();
            self.histograms.clear();
            return count;
        }
        commands
            .iter()
            .filter_map(|cmd| {
                let key = cmd.to_ascii_uppercase();
                self.histograms.get_mut(&key).map(|h| {
                    h.reset();
                    1
                })
            })
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionLoopResult {
    pub status: EvictionLoopStatus,
    pub failure: Option<EvictionLoopFailure>,
    pub sampled_keys: usize,
    pub evicted_keys: usize,
    pub bytes_freed: usize,
    pub bytes_to_free_after: usize,
}

impl ValueType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::ZSet => "zset",
            Self::Stream => "stream",
        }
    }
}

/// Default number of databases (matches Redis default).
pub const DEFAULT_NUM_DATABASES: usize = 16;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientTrackingState {
    pub enabled: bool,
    pub redirect: Option<u64>,
    pub bcast: bool,
    pub optin: bool,
    pub optout: bool,
    pub caching: Option<bool>,
    pub noloop: bool,
    pub prefixes: BTreeSet<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientReplyState {
    pub off: bool,
    pub skip_next: bool,
    pub suppress_current_response: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchClientContext {
    pub client_id: u64,
    pub client_name: Option<Vec<u8>>,
    pub client_lib_name: Option<String>,
    pub client_lib_ver: Option<String>,
    pub age_seconds: u64,
    pub idle_seconds: u64,
    pub db_index: usize,
    pub flags: String,
    pub peer_addr: String,
    pub authenticated_user: Vec<u8>,
    pub resp_protocol_version: i64,
    pub channel_subscriptions: usize,
    pub pattern_subscriptions: usize,
    pub shard_subscriptions: usize,
    pub multi_count: i64,
    pub watch_count: usize,
    pub is_pubsub: bool,
    pub client_tracking: ClientTrackingState,
    pub client_reply: ClientReplyState,
    pub acl_permissions: Option<DispatchAclPermissions>,
}

impl Default for DispatchClientContext {
    fn default() -> Self {
        Self {
            client_id: 1,
            client_name: None,
            client_lib_name: None,
            client_lib_ver: None,
            age_seconds: 0,
            idle_seconds: 0,
            db_index: 0,
            flags: "N".to_string(),
            peer_addr: "127.0.0.1:0".to_string(),
            authenticated_user: b"default".to_vec(),
            resp_protocol_version: 2,
            channel_subscriptions: 0,
            pattern_subscriptions: 0,
            shard_subscriptions: 0,
            multi_count: -1,
            watch_count: 0,
            is_pubsub: false,
            client_tracking: ClientTrackingState::default(),
            client_reply: ClientReplyState::default(),
            acl_permissions: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAclPermissionReason {
    Command,
    Key,
    Channel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAclLogContext {
    Lua,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAclLogEvent {
    pub reason: DispatchAclPermissionReason,
    pub context: DispatchAclLogContext,
    pub object: String,
    pub username: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DispatchAclPermissions {
    pub all_commands: bool,
    pub allowed_commands: HashSet<String>,
    pub denied_commands: HashSet<String>,
    pub allowed_categories: HashSet<String>,
    pub denied_categories: HashSet<String>,
    pub key_patterns: Vec<Vec<u8>>,
    pub all_keys: bool,
    pub channel_patterns: Vec<Vec<u8>>,
    pub all_channels: bool,
}

// Match upstream server.h: PROPAGATE_AOF=1, PROPAGATE_REPL=2.
// Lua-visible redis.REPL_AOF / REPL_SLAVE / REPL_REPLICA / REPL_ALL
// constants depend on these values being correct.
// (br-frankenredis-replconst)
pub const SCRIPT_PROPAGATE_AOF: u8 = 0b0001;
pub const SCRIPT_PROPAGATE_REPLICA: u8 = 0b0010;
pub const SCRIPT_PROPAGATE_ALL: u8 = SCRIPT_PROPAGATE_REPLICA | SCRIPT_PROPAGATE_AOF;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptPropagationRecord {
    pub argv: Vec<Vec<u8>>,
    pub targets: u8,
}

#[derive(Debug)]
pub struct Store {
    entries: HashMap<Vec<u8>, Entry>,
    ordered_keys: BTreeSet<Vec<u8>>,
    running_digest: u64,
    digest_mutations: u64,
    digest_stale: bool,
    db_key_counts: Vec<usize>,
    db_expires_counts: Vec<usize>,
    /// Number of databases (configurable at startup, default 16).
    pub database_count: usize,
    pub cluster_enabled: bool,
    /// Hash slots assigned to this node in cluster mode. Empty on startup,
    /// matching Redis until CLUSTER ADDSLOTS/ADDSLOTSRANGE assigns ranges.
    pub cluster_assigned_slots: BTreeSet<u16>,
    stream_groups: HashMap<Vec<u8>, StreamGroupState>,
    /// Per-stream last-generated-id set by XSETID (may be higher than max entry).
    stream_last_ids: HashMap<Vec<u8>, StreamId>,
    /// Highest stream entry ID removed via XDEL/XTRIM for each stream key.
    pub stream_max_deleted_ids: HashMap<Vec<u8>, StreamId>,
    /// Script cache: SHA1 hex string → script body.
    script_cache: HashMap<String, Vec<u8>>,
    /// Pub/Sub: channels this client is subscribed to.
    pub subscribed_channels: HashSet<Vec<u8>>,
    /// Pub/Sub: patterns this client is subscribed to.
    pub subscribed_patterns: HashSet<Vec<u8>>,
    /// Pub/Sub: shard channels this client is subscribed to (Redis 7.0+).
    pub subscribed_shard_channels: HashSet<Vec<u8>>,
    /// Pub/Sub: pending messages for delivery.
    pub pubsub_pending: Vec<PubSubMessage>,
    /// Function libraries: library_name → FunctionLibrary.
    function_libraries: HashMap<String, FunctionLibrary>,

    /// Per-field hash TTLs (Redis 7.4 HEXPIRE family). Keyed on
    /// (hash-key-bytes, field-bytes) → absolute expiry in ms-since-epoch.
    /// Read/write paths treat this as additive: a missing entry means
    /// "no TTL" (the field persists indefinitely with its hash); a past
    /// entry means "expired" and the field should be hidden/reaped on next
    /// access. Part 1 (br-frankenredis-wwz3) only wires storage +
    /// primitives; hash-read-path lazy expiry is part 3 (b8ut).
    pub hash_field_expires: BTreeMap<(Vec<u8>, Vec<u8>), u64>,

    /// Cumulative count of per-field TTL reaps observed per hash key.
    /// Surfaced via DEBUG OBJECT's `hexpired_fields:<n>` suffix
    /// (br-frankenredis-25re). Cleared when the whole hash key is
    /// removed, so the counter is always scoped to the *current*
    /// incarnation of the key.
    pub hash_field_expired_counts: BTreeMap<Vec<u8>, u64>,

    // Eviction policy — configurable via CONFIG SET maxmemory-policy.
    pub maxmemory_policy: MaxmemoryPolicy,
    pub lfu_decay_time: u64,

    // Encoding thresholds — configurable via CONFIG SET, used by OBJECT ENCODING.
    pub hash_max_listpack_entries: usize,
    pub hash_max_listpack_value: usize,
    pub list_max_listpack_size: i64,
    pub list_max_listpack_entries: usize,
    pub list_max_listpack_value: usize,
    pub set_max_intset_entries: usize,
    pub set_max_listpack_entries: usize,
    pub zset_max_listpack_entries: usize,
    pub zset_max_listpack_value: usize,

    /// Seed for deterministic pseudo-random operations (HRANDFIELD, RANDOMKEY, etc.).
    pub rng_seed: u64,

    /// Total number of successful mutations since startup.
    pub dirty: u64,

    /// Unix timestamp of the last successful SAVE/BGSAVE observed by this store.
    pub last_save_time_sec: u64,
    /// Number of successful SAVE/BGSAVE operations observed by this store.
    pub stat_rdb_saves: u64,
    /// Unix timestamp of the last successful BGSAVE, if any.
    pub stat_rdb_last_bgsave_time_sec: Option<u64>,
    /// Unix timestamp of the last successful AOF rewrite, if any.
    pub stat_aof_last_rewrite_time_sec: Option<u64>,
    /// Status of the last BGSAVE attempt reported via INFO persistence.
    pub stat_rdb_last_bgsave_ok: bool,
    /// Status of the last BGREWRITEAOF attempt reported via INFO persistence.
    pub stat_aof_last_bgrewrite_ok: bool,
    /// Status of the last AOF snapshot write reported via INFO persistence.
    pub stat_aof_last_write_ok: bool,
    /// Whether appendonly is currently enabled for live INFO/config reporting.
    pub aof_enabled: bool,

    /// Current recursion depth of Lua script execution.
    pub script_nesting_level: usize,
    /// Whether the current script/function execution context forbids writes.
    pub script_read_only: bool,
    /// Current propagation mask for commands emitted from the active Lua script.
    pub script_propagation_mode: u8,
    /// Commands emitted by the active Lua script together with their propagation masks.
    pub script_propagation_records: Vec<ScriptPropagationRecord>,

    /// Number of keys currently tracked in the expires set.
    pub expires_count: usize,

    /// Cached memory usage to avoid O(N) calculation on every command.
    pub cached_memory_usage_bytes: std::cell::Cell<usize>,
    pub cached_memory_usage_dirty: std::cell::Cell<u64>,

    /// Keyspace notification flags (parsed from notify-keyspace-events config).
    pub notify_keyspace_events: u32,

    /// Pending keyspace notification messages (channel, message) to deliver
    /// via the pub/sub system after command execution.
    pub keyspace_notifications: Vec<(Vec<u8>, Vec<u8>)>,

    // ── Server-wide metadata and stats (updated by runtime, read by INFO) ──
    /// Unique 40-character hex run ID generated at startup.
    pub server_run_id: String,
    /// Stable 40-character shard ID used by CLUSTER MYSHARDID when cluster mode is enabled.
    pub cluster_shard_id: String,
    /// Server process ID.
    pub server_pid: u32,
    /// Server TCP port.
    pub server_port: u16,
    /// Wall-clock millisecond timestamp when this Store was created.
    /// Used by INFO server's `uptime_in_seconds` to compute time
    /// since process startup (not the absolute timestamp).
    /// (br-frankenredis-uptime)
    pub server_start_ms: u64,
    /// Total number of commands processed since server start.
    pub stat_total_commands_processed: u64,
    /// Total number of connections received since server start.
    pub stat_total_connections_received: u64,
    /// Number of currently connected clients.
    pub stat_connected_clients: u64,
    /// Number of clients currently blocked on a blocking operation.
    pub stat_blocked_clients: u64,
    /// Number of clients with client-side caching tracking enabled.
    pub stat_tracking_clients: u64,
    /// Number of successful key lookups performed through store read/query APIs.
    pub stat_keyspace_hits: u64,
    /// Number of missing-key lookups performed through store read/query APIs.
    pub stat_keyspace_misses: u64,
    /// Total number of unexpected error replies emitted during internal replay paths.
    pub stat_unexpected_error_replies: u64,
    /// Total number of RESP error replies emitted to clients.
    pub stat_total_error_replies: u64,
    /// Total number of client-visible readonly commands processed.
    pub stat_total_reads_processed: u64,
    /// Total number of client-visible write commands processed.
    pub stat_total_writes_processed: u64,
    /// Total keys removed due to expiration (lazy or active).
    pub stat_expired_keys: u64,
    /// Total keys removed due to maxmemory eviction.
    pub stat_evicted_keys: u64,
    /// Percentage of expired keys found during active-expire sampling.
    pub stat_expired_stale_perc: u64,
    /// Cumulative CPU time spent in active-expire cycles.
    pub stat_expire_cycle_cpu_milliseconds: u64,
    /// Slow log ring buffer shared by runtime and command dispatch paths.
    pub slowlog: VecDeque<SlowlogEntry>,
    /// Next slow log entry ID.
    pub slowlog_id_counter: u64,
    /// Slow log threshold in microseconds (slowlog-log-slower-than config).
    pub slowlog_log_slower_than_us: i64,
    /// Slow log maximum length.
    pub slowlog_max_len: usize,
    /// Store-owned latency monitor state shared between runtime recording and command reads.
    pub latency_tracker: LatencyTracker,
    /// Per-command latency histograms for LATENCY HISTOGRAM command.
    pub command_histograms: CommandHistogramTracker,
    /// Store-owned Sentinel state used by SENTINEL subcommands.
    pub sentinel_state: fr_sentinel::SentinelState,
    /// True when the server was started in sentinel mode (`redis-server
    /// --sentinel` upstream). Off by default — the SENTINEL command is
    /// only registered when this flag is set, matching upstream's
    /// `server.sentinel_mode` gate. Without this gate, a vanilla
    /// frankenredis instance would expose SENTINEL responses where
    /// upstream returns "ERR unknown command 'SENTINEL'", which trips
    /// the core_module_sentinel conformance fixture.
    /// (br-frankenredis-pq3z)
    pub sentinel_mode: bool,
    /// Server hz (event loop frequency), synced from runtime.
    pub server_hz: u64,
    /// Replication backlog size, synced from runtime.
    pub server_repl_backlog_size: u64,
    /// Maximum number of clients, synced from runtime.
    pub server_maxclients: u64,
    /// Live maxmemory setting, synced from runtime.
    pub maxmemory_bytes_live: usize,
    /// Current client/session metadata for delegated dispatch paths such as Lua.
    pub dispatch_client_ctx: DispatchClientContext,
    /// ACL log events raised inside delegated dispatch paths such as Lua scripts.
    pending_acl_log_events: Vec<PendingAclLogEvent>,
    /// Controls whether runtime active-expire cycles are allowed to run.
    pub active_expire_enabled: bool,
    /// Set by DEBUG RELOAD; runtime consumes it after command dispatch.
    pub debug_reload_requested: bool,
    /// Set by BGREWRITEAOF in delegated dispatch paths; runtime consumes it after dispatch.
    pub bgrewriteaof_requested: bool,
    /// Most recent sampled resident set size (RSS) in bytes.
    pub stat_used_memory_rss: usize,
    /// Peak sampled memory high-water mark (RSS when available, logical fallback).
    pub stat_used_memory_peak: usize,
    /// Connections rejected due to maxclients limit.
    pub stat_rejected_connections: u64,
    /// Full resyncs completed (PSYNC FULLRESYNC).
    pub stat_sync_full: u64,
    /// Partial resyncs accepted (PSYNC CONTINUE).
    pub stat_sync_partial_ok: u64,
    /// Partial resyncs rejected (fell back to full resync).
    pub stat_sync_partial_err: u64,
    /// Total bytes received from client connections (non-replication).
    pub stat_total_net_input_bytes: u64,
    /// Total bytes sent to client connections (non-replication).
    pub stat_total_net_output_bytes: u64,
    /// Ring buffer for instantaneous ops/sec sampling (16 samples, ~100ms apart).
    ops_sec_samples: [u64; 16],
    /// Index into `ops_sec_samples` ring buffer.
    ops_sec_idx: usize,
    /// `stat_total_commands_processed` at the time of the last sample.
    ops_sec_last_sample_count: u64,
    /// Ring buffer for instantaneous input kbps sampling (16 samples).
    input_kbps_samples: [f64; 16],
    /// Ring buffer for instantaneous output kbps sampling (16 samples).
    output_kbps_samples: [f64; 16],
    /// `stat_total_net_input_bytes` at the time of the last sample.
    net_input_last_sample_bytes: u64,
    /// `stat_total_net_output_bytes` at the time of the last sample.
    net_output_last_sample_bytes: u64,
}

const DB_NAMESPACE_PREFIX: &[u8] = b"\0frdb\0";

#[must_use]
pub fn encode_db_key(db: usize, key: &[u8]) -> Vec<u8> {
    if db == 0 {
        return key.to_vec();
    }
    let mut encoded =
        Vec::with_capacity(DB_NAMESPACE_PREFIX.len() + std::mem::size_of::<u64>() + key.len());
    encoded.extend_from_slice(DB_NAMESPACE_PREFIX);
    encoded.extend_from_slice(&(db as u64).to_be_bytes());
    encoded.extend_from_slice(key);
    encoded
}

#[must_use]
pub fn decode_db_key(key: &[u8]) -> Option<(usize, &[u8])> {
    let db_len = std::mem::size_of::<u64>();
    let prefix_len = DB_NAMESPACE_PREFIX.len() + db_len;
    if key.len() < prefix_len || !key.starts_with(DB_NAMESPACE_PREFIX) {
        return None;
    }
    let db_bytes: [u8; 8] = key[DB_NAMESPACE_PREFIX.len()..prefix_len].try_into().ok()?;
    let db = usize::try_from(u64::from_be_bytes(db_bytes)).ok()?;
    Some((db, &key[prefix_len..]))
}

#[must_use]
pub fn read_rss_bytes() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb_str = rest.trim().strip_suffix("kB")?.trim();
                let kb: usize = kb_str.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

impl Default for Store {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            ordered_keys: BTreeSet::new(),
            running_digest: 0,
            digest_mutations: 0,
            digest_stale: false,
            db_key_counts: vec![0; DEFAULT_NUM_DATABASES],
            db_expires_counts: vec![0; DEFAULT_NUM_DATABASES],
            database_count: DEFAULT_NUM_DATABASES,
            cluster_enabled: false,
            cluster_assigned_slots: BTreeSet::new(),
            stream_groups: HashMap::new(),
            stream_last_ids: HashMap::new(),
            stream_max_deleted_ids: HashMap::new(),
            script_cache: HashMap::new(),
            subscribed_channels: HashSet::new(),
            subscribed_patterns: HashSet::new(),
            subscribed_shard_channels: HashSet::new(),
            pubsub_pending: Vec::new(),
            function_libraries: HashMap::new(),
            hash_field_expires: BTreeMap::new(),
            hash_field_expired_counts: BTreeMap::new(),
            maxmemory_policy: MaxmemoryPolicy::default(),
            lfu_decay_time: 1,
            hash_max_listpack_entries: 512,
            hash_max_listpack_value: 64,
            list_max_listpack_size: -2,
            list_max_listpack_entries: 128,
            list_max_listpack_value: 64,
            set_max_intset_entries: 512,
            set_max_listpack_entries: 128,
            zset_max_listpack_entries: 128,
            zset_max_listpack_value: 64,
            rng_seed: 0xDEADBEEF_C0FFEE11,
            dirty: 0,
            last_save_time_sec: 0,
            stat_rdb_saves: 0,
            stat_rdb_last_bgsave_time_sec: None,
            stat_aof_last_rewrite_time_sec: None,
            stat_rdb_last_bgsave_ok: true,
            stat_aof_last_bgrewrite_ok: true,
            stat_aof_last_write_ok: true,
            aof_enabled: false,
            script_nesting_level: 0,
            script_read_only: false,
            script_propagation_mode: SCRIPT_PROPAGATE_ALL,
            script_propagation_records: Vec::new(),
            expires_count: 0,
            cached_memory_usage_bytes: std::cell::Cell::new(0),
            cached_memory_usage_dirty: std::cell::Cell::new(0),
            notify_keyspace_events: 0,
            keyspace_notifications: Vec::new(),
            server_run_id: generate_run_id(),
            cluster_shard_id: generate_run_id(),
            server_pid: std::process::id(),
            server_port: 6379,
            server_start_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            stat_total_commands_processed: 0,
            stat_total_connections_received: 0,
            stat_connected_clients: 0,
            stat_blocked_clients: 0,
            stat_tracking_clients: 0,
            stat_keyspace_hits: 0,
            stat_keyspace_misses: 0,
            stat_unexpected_error_replies: 0,
            stat_total_error_replies: 0,
            stat_total_reads_processed: 0,
            stat_total_writes_processed: 0,
            stat_expired_keys: 0,
            stat_evicted_keys: 0,
            stat_expired_stale_perc: 0,
            stat_expire_cycle_cpu_milliseconds: 0,
            slowlog: VecDeque::new(),
            slowlog_id_counter: 0,
            slowlog_log_slower_than_us: 10_000,
            slowlog_max_len: 128,
            latency_tracker: LatencyTracker::default(),
            command_histograms: CommandHistogramTracker::default(),
            sentinel_state: fr_sentinel::SentinelState::new(),
            sentinel_mode: false,
            server_hz: 10,
            server_repl_backlog_size: 1_048_576,
            server_maxclients: 10000,
            maxmemory_bytes_live: 0,
            dispatch_client_ctx: DispatchClientContext::default(),
            pending_acl_log_events: Vec::new(),
            active_expire_enabled: true,
            debug_reload_requested: false,
            bgrewriteaof_requested: false,
            stat_used_memory_rss: 0,
            stat_used_memory_peak: 0,
            stat_rejected_connections: 0,
            stat_sync_full: 0,
            stat_sync_partial_ok: 0,
            stat_sync_partial_err: 0,
            stat_total_net_input_bytes: 0,
            stat_total_net_output_bytes: 0,
            ops_sec_samples: [0; 16],
            ops_sec_idx: 0,
            ops_sec_last_sample_count: 0,
            input_kbps_samples: [0.0; 16],
            output_kbps_samples: [0.0; 16],
            net_input_last_sample_bytes: 0,
            net_output_last_sample_bytes: 0,
        }
    }
}

/// A registered function library (Redis 7.0+ FUNCTION framework).
#[derive(Clone, Debug)]
pub struct FunctionLibrary {
    /// Library name (unique identifier).
    pub name: String,
    /// Engine name (e.g. "LUA").
    pub engine: String,
    /// Optional description.
    pub description: Option<String>,
    /// Raw library code.
    pub code: Vec<u8>,
    /// Functions registered by this library: function_name → FunctionEntry.
    pub functions: Vec<FunctionEntry>,
}

/// A single function within a library.
#[derive(Clone, Debug)]
pub struct FunctionEntry {
    /// Function name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Flags (e.g. "no-writes", "allow-stale").
    pub flags: Vec<String>,
}

impl Store {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn ordered_physical_keys_in_db(&self, db: usize) -> Vec<Vec<u8>> {
        if db == 0 {
            return self
                .ordered_keys
                .iter()
                .filter(|key| decode_db_key(key).is_none())
                .cloned()
                .collect();
        }

        let prefix = encode_db_key(db, b"");
        self.ordered_keys
            .range(prefix.clone()..)
            .take_while(|key| key.starts_with(&prefix))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn next_rand(&mut self) -> u64 {
        self.rng_seed = self
            .rng_seed
            .wrapping_mul(0x5851_f42d_4c95_7f2d)
            .wrapping_add(1);
        self.rng_seed
    }

    pub fn mark_saved_at(&mut self, now_ms: u64) {
        self.last_save_time_sec = now_ms / 1000;
    }

    pub fn record_save(&mut self, now_ms: u64, background: bool) {
        self.mark_saved_at(now_ms);
        self.stat_rdb_saves = self.stat_rdb_saves.saturating_add(1);
        if background {
            self.stat_rdb_last_bgsave_time_sec = Some(self.last_save_time_sec);
        }
    }

    pub fn record_aof_rewrite(&mut self, now_ms: u64) {
        self.stat_aof_last_rewrite_time_sec = Some(now_ms / 1000);
    }

    pub fn record_bgsave_status(&mut self, ok: bool) {
        self.stat_rdb_last_bgsave_ok = ok;
    }

    pub fn record_aof_bgrewrite_status(&mut self, ok: bool) {
        self.stat_aof_last_bgrewrite_ok = ok;
    }

    pub fn record_aof_write_status(&mut self, ok: bool) {
        self.stat_aof_last_write_ok = ok;
    }

    pub fn set_aof_enabled(&mut self, enabled: bool) {
        self.aof_enabled = enabled;
    }

    pub fn clear_script_propagation_state(&mut self) {
        self.script_propagation_mode = SCRIPT_PROPAGATE_ALL;
        self.script_propagation_records.clear();
    }

    pub fn record_script_propagation(&mut self, argv: &[Vec<u8>]) {
        self.script_propagation_records
            .push(ScriptPropagationRecord {
                argv: argv.to_vec(),
                targets: self.script_propagation_mode,
            });
    }

    pub fn observe_memory_sample(&mut self, used_memory_rss: usize) {
        self.stat_used_memory_rss = used_memory_rss;
        self.stat_used_memory_peak = self.stat_used_memory_peak.max(used_memory_rss);
    }

    /// Record a periodic sample for ops/sec, throughput, and RSS high-water calculations.
    /// Call this once per server-hz tick (e.g. every 100ms at 10hz).
    /// `elapsed_ms` is the wall-clock time since the last sample (typically ~100ms).
    pub fn record_ops_sec_sample(&mut self, elapsed_ms: u64) {
        let current = self.stat_total_commands_processed;
        let delta = current.saturating_sub(self.ops_sec_last_sample_count);
        // Scale delta to a per-second rate: ops_in_window * (1000 / elapsed_ms)
        let ops_per_sec = delta
            .saturating_mul(1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0);
        self.ops_sec_samples[self.ops_sec_idx] = ops_per_sec;
        self.ops_sec_last_sample_count = current;

        // Network throughput: bytes/sec → kbps
        let elapsed_sec = elapsed_ms as f64 / 1000.0;
        let in_delta = self
            .stat_total_net_input_bytes
            .saturating_sub(self.net_input_last_sample_bytes);
        let out_delta = self
            .stat_total_net_output_bytes
            .saturating_sub(self.net_output_last_sample_bytes);
        self.input_kbps_samples[self.ops_sec_idx] = if elapsed_sec > 0.0 {
            (in_delta as f64 / 1024.0) / elapsed_sec
        } else {
            0.0
        };
        self.output_kbps_samples[self.ops_sec_idx] = if elapsed_sec > 0.0 {
            (out_delta as f64 / 1024.0) / elapsed_sec
        } else {
            0.0
        };
        self.net_input_last_sample_bytes = self.stat_total_net_input_bytes;
        self.net_output_last_sample_bytes = self.stat_total_net_output_bytes;

        let used_memory = self.estimate_memory_usage_bytes();
        let used_memory_rss = read_rss_bytes().unwrap_or(used_memory);
        self.observe_memory_sample(used_memory_rss);

        self.ops_sec_idx = (self.ops_sec_idx + 1) % 16;
    }

    /// Return the averaged instantaneous ops/sec across the sample ring buffer.
    #[must_use]
    pub fn instantaneous_ops_per_sec(&self) -> u64 {
        let sum: u64 = self.ops_sec_samples.iter().sum();
        sum / 16
    }

    /// Return the averaged instantaneous input throughput in KiB/sec.
    #[must_use]
    pub fn instantaneous_input_kbps(&self) -> f64 {
        let sum: f64 = self.input_kbps_samples.iter().sum();
        sum / 16.0
    }

    /// Return the averaged instantaneous output throughput in KiB/sec.
    #[must_use]
    pub fn instantaneous_output_kbps(&self) -> f64 {
        let sum: f64 = self.output_kbps_samples.iter().sum();
        sum / 16.0
    }

    pub fn reset_info_stats(&mut self) {
        self.reset_slowlog();
        self.command_histograms = CommandHistogramTracker::default();
        self.stat_total_commands_processed = 0;
        self.stat_total_connections_received = 0;
        self.stat_unexpected_error_replies = 0;
        self.stat_total_error_replies = 0;
        self.stat_total_reads_processed = 0;
        self.stat_total_writes_processed = 0;
        self.stat_expired_keys = 0;
        self.stat_evicted_keys = 0;
        self.stat_expired_stale_perc = 0;
        self.stat_expire_cycle_cpu_milliseconds = 0;
        self.stat_keyspace_hits = 0;
        self.stat_keyspace_misses = 0;
        self.stat_rejected_connections = 0;
        self.stat_sync_full = 0;
        self.stat_sync_partial_ok = 0;
        self.stat_sync_partial_err = 0;
        self.stat_used_memory_rss = 0;
        self.stat_used_memory_peak = 0;
        self.stat_total_net_input_bytes = 0;
        self.stat_total_net_output_bytes = 0;
        self.ops_sec_samples = [0; 16];
        self.ops_sec_idx = 0;
        self.ops_sec_last_sample_count = 0;
        self.input_kbps_samples = [0.0; 16];
        self.output_kbps_samples = [0.0; 16];
        self.net_input_last_sample_bytes = 0;
        self.net_output_last_sample_bytes = 0;
    }

    fn record_keyspace_lookup(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        let hit = self.entries.contains_key(key);
        if hit {
            self.stat_keyspace_hits = self.stat_keyspace_hits.saturating_add(1);
        } else {
            self.stat_keyspace_misses = self.stat_keyspace_misses.saturating_add(1);
        }
        hit
    }

    fn lfu_tracking_enabled(&self) -> bool {
        self.maxmemory_policy.tracks_lfu()
    }

    pub fn record_latency_sample(&mut self, event: &str, duration_ms: u64, now_sec: u64) {
        self.latency_tracker
            .record_sample(event, duration_ms, now_sec);
    }

    #[must_use]
    pub fn latency_latest(&self) -> Vec<(String, LatencySample)> {
        self.latency_tracker.latest()
    }

    #[must_use]
    pub fn latency_history(&self, event: &str) -> Vec<LatencySample> {
        self.latency_tracker.history(event)
    }

    pub fn latency_reset(&mut self, events: &[&str]) -> usize {
        self.latency_tracker.reset(events)
    }

    /// Record a command execution latency for LATENCY HISTOGRAM.
    pub fn record_command_histogram(&mut self, command: &str, latency_us: u64) {
        self.command_histograms.record(command, latency_us);
    }

    /// Get histogram data for a specific command.
    #[must_use]
    pub fn get_command_histogram(&self, command: &str) -> Option<&CommandHistogram> {
        self.command_histograms.get(command)
    }

    /// Get all command histograms, sorted by command name.
    #[must_use]
    pub fn all_command_histograms(&self) -> Vec<(&str, &CommandHistogram)> {
        self.command_histograms.all()
    }

    /// Reset command histograms for specified commands, or all if empty.
    pub fn reset_command_histograms(&mut self, commands: &[&str]) -> usize {
        self.command_histograms.reset(commands)
    }

    fn update_stream_max_deleted_id(&mut self, key: &[u8], deleted_id: StreamId) {
        let entry = self
            .stream_max_deleted_ids
            .entry(key.to_vec())
            .or_insert((0, 0));
        if deleted_id > *entry {
            *entry = deleted_id;
        }
    }

    pub fn reset_slowlog(&mut self) {
        self.slowlog.clear();
        self.slowlog_id_counter = 0;
    }

    pub fn record_slowlog(&mut self, argv: &[Vec<u8>], duration_us: u64, now_ms: u64) {
        if self.slowlog_log_slower_than_us < 0 {
            return;
        }
        if (duration_us as i64) < self.slowlog_log_slower_than_us {
            return;
        }
        let entry = SlowlogEntry {
            id: self.slowlog_id_counter,
            timestamp_sec: now_ms / 1000,
            duration_us,
            argv: argv.to_vec(),
        };
        self.slowlog_id_counter = self.slowlog_id_counter.saturating_add(1);
        self.slowlog.push_back(entry);
        while self.slowlog.len() > self.slowlog_max_len {
            self.slowlog.pop_front();
        }
    }

    #[must_use]
    pub fn get_slowlog(&self, count: usize) -> Vec<SlowlogEntry> {
        self.slowlog.iter().rev().take(count).cloned().collect()
    }

    #[must_use]
    pub fn slowlog_len(&self) -> usize {
        self.slowlog.len()
    }

    fn list_fits_legacy_listpack_size(&self, list: &VecDeque<Vec<u8>>) -> bool {
        if self.list_max_listpack_size >= 0 {
            return list.len() <= self.list_max_listpack_size as usize;
        }

        let max_bytes: usize = match self.list_max_listpack_size {
            -1 => 4096,
            -2 => 8192,
            -3 => 16384,
            -4 => 32768,
            _ => 65536, // -5 and below
        };
        let total: usize = list.iter().map(|v| v.len() + 11).sum(); // 11 bytes overhead per entry
        total <= max_bytes
    }

    /// Get a string value. Returns `None` if the key doesn't exist.
    /// Returns `Err(WrongType)` if the key holds a non-string value.
    pub fn get(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        let lfu_tracking_enabled = self.lfu_tracking_enabled();
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let v = v.clone();
                    if lfu_tracking_enabled {
                        entry.bump_lfu_freq(now_ms, self.lfu_decay_time);
                    }
                    entry.touch(now_ms);
                    Ok(Some(v))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn set(&mut self, key: Vec<u8>, value: Vec<u8>, px_ttl_ms: Option<u64>, now_ms: u64) {
        self.drop_if_expired(key.as_slice(), now_ms);
        let expires_at_ms = px_ttl_ms.map(|ttl| now_ms.saturating_add(ttl));
        let next_lfu_freq = if self.lfu_tracking_enabled() {
            self.entries.get(key.as_slice()).map_or(0, |entry| {
                entry
                    .current_lfu_freq(now_ms, self.lfu_decay_time)
                    .saturating_add(1)
            })
        } else {
            0
        };
        self.stream_groups.remove(key.as_slice());
        self.stream_last_ids.remove(key.as_slice());
        let mut entry = Entry::new(Value::String(value), expires_at_ms, now_ms);
        entry.lfu_freq = next_lfu_freq;
        entry.lfu_last_touch_min = now_ms / 60_000;
        self.internal_entries_insert(key, entry);
        self.dirty = self.dirty.saturating_add(1);
    }

    /// SET variant that takes an absolute expiry timestamp (for EXAT/PXAT/KEEPTTL).
    pub fn set_with_abs_expiry(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        expires_at_ms: Option<u64>,
        now_ms: u64,
    ) {
        self.drop_if_expired(key.as_slice(), now_ms);
        let next_lfu_freq = if self.lfu_tracking_enabled() {
            self.entries.get(key.as_slice()).map_or(0, |entry| {
                entry
                    .current_lfu_freq(now_ms, self.lfu_decay_time)
                    .saturating_add(1)
            })
        } else {
            0
        };
        self.stream_groups.remove(key.as_slice());
        self.stream_last_ids.remove(key.as_slice());
        let mut entry = Entry::new(Value::String(value), expires_at_ms, now_ms);
        entry.lfu_freq = next_lfu_freq;
        entry.lfu_last_touch_min = now_ms / 60_000;
        self.internal_entries_insert(key, entry);
        self.dirty = self.dirty.saturating_add(1);
    }

    /// Returns the current absolute expiry timestamp for a key, if any.
    pub fn get_expires_at_ms(&mut self, key: &[u8], now_ms: u64) -> Option<u64> {
        self.drop_if_expired(key, now_ms);
        self.entries.get(key).and_then(|entry| entry.expires_at_ms)
    }

    pub fn expiretime_value(&mut self, key: &[u8], now_ms: u64) -> ExpireTimeValue {
        if !self.record_keyspace_lookup(key, now_ms) {
            return ExpireTimeValue::KeyMissing;
        }
        if let Some(entry) = self.entries.get_mut(key) {
            entry.touch(now_ms);
        }
        match self.entries.get(key).and_then(|entry| entry.expires_at_ms) {
            Some(expires_at_ms) => ExpireTimeValue::ExpiresAt(expires_at_ms),
            None => ExpireTimeValue::NoExpiry,
        }
    }

    pub fn del(&mut self, keys: &[Vec<u8>], now_ms: u64) -> u64 {
        let mut removed = 0_u64;
        for key in keys {
            self.drop_if_expired(key, now_ms);
            if self.internal_entries_remove(key.as_slice()).is_some() {
                self.stream_groups.remove(key.as_slice());
                self.stream_last_ids.remove(key.as_slice());
                removed = removed.saturating_add(1);
            }
        }
        self.dirty = self.dirty.saturating_add(removed);
        removed
    }

    pub fn exists(&mut self, key: &[u8], now_ms: u64) -> bool {
        if !self.record_keyspace_lookup(key, now_ms) {
            return false;
        }
        let lfu_tracking_enabled = self.lfu_tracking_enabled();
        if let Some(entry) = self.entries.get_mut(key) {
            if lfu_tracking_enabled {
                entry.bump_lfu_freq(now_ms, self.lfu_decay_time);
            }
            entry.touch(now_ms);
            true
        } else {
            false
        }
    }

    pub fn exists_no_touch(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.record_keyspace_lookup(key, now_ms)
    }

    pub fn incr(&mut self, key: &[u8], now_ms: u64) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (current, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (parse_i64(v)?, entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (0_i64, None),
        };
        let next = current.checked_add(1).ok_or(StoreError::IntegerOverflow)?;
        self.internal_entries_insert(
            key.to_vec(),
            Entry::new(
                Value::String(next.to_string().into_bytes()),
                expires_at_ms,
                now_ms,
            ),
        );
        self.dirty = self.dirty.saturating_add(1);
        Ok(next)
    }

    pub fn expire_seconds(&mut self, key: &[u8], seconds: i64, now_ms: u64) -> bool {
        let ttl_ms = seconds.checked_mul(1000).unwrap_or_else(|| {
            if seconds.is_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        });
        self.expire_milliseconds(key, ttl_ms, now_ms)
    }

    pub fn expire_milliseconds(&mut self, key: &[u8], milliseconds: i64, now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        if !self.entries.contains_key(key) {
            return false;
        }
        let (db, logical_key) = match decode_db_key(key) {
            Some((db, lk)) => (db, lk.to_vec()),
            None => (0, key.to_vec()),
        };
        if milliseconds <= 0 {
            self.notify_keyspace_event(NOTIFY_GENERIC, "del", &logical_key, db);
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
            self.dirty = self.dirty.saturating_add(1);
            return true;
        }

        let ttl_ms = u64::try_from(milliseconds).unwrap_or(u64::MAX);
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        let mut added_expiry = false;
        if self
            .with_mutated_entry(key, |entry| {
                added_expiry = entry.expires_at_ms.is_none();
                entry.expires_at_ms = Some(expires_at_ms);
            })
            .is_some()
        {
            if added_expiry {
                self.expires_count = self.expires_count.saturating_add(1);
                let db = decode_db_key(key).map(|(db, _)| db).unwrap_or(0);
                if db < self.database_count {
                    self.db_expires_counts[db] = self.db_expires_counts[db].saturating_add(1);
                }
            }
            self.dirty = self.dirty.saturating_add(1);
            self.notify_keyspace_event(NOTIFY_GENERIC, "expire", &logical_key, db);
        }
        true
    }

    pub fn expire_at_milliseconds(&mut self, key: &[u8], when_ms: i64, now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        if !self.entries.contains_key(key) {
            return false;
        }

        let (db, logical_key) = match decode_db_key(key) {
            Some((db, lk)) => (db, lk.to_vec()),
            None => (0, key.to_vec()),
        };
        if i128::from(when_ms) <= i128::from(now_ms) {
            self.notify_keyspace_event(NOTIFY_EXPIRED, "expired", &logical_key, db);
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
            self.dirty = self.dirty.saturating_add(1);
            return true;
        }

        let expires_at_ms = u64::try_from(when_ms).unwrap_or(u64::MAX);
        let mut added_expiry = false;
        if self
            .with_mutated_entry(key, |entry| {
                added_expiry = entry.expires_at_ms.is_none();
                entry.expires_at_ms = Some(expires_at_ms);
            })
            .is_some()
        {
            if added_expiry {
                self.expires_count = self.expires_count.saturating_add(1);
                let db = decode_db_key(key).map(|(db, _)| db).unwrap_or(0);
                if db < self.database_count {
                    self.db_expires_counts[db] = self.db_expires_counts[db].saturating_add(1);
                }
            }
            self.dirty = self.dirty.saturating_add(1);
            self.notify_keyspace_event(NOTIFY_GENERIC, "expire", &logical_key, db);
        }
        true
    }

    #[must_use]
    pub fn pttl(&mut self, key: &[u8], now_ms: u64) -> PttlValue {
        if !self.record_keyspace_lookup(key, now_ms) {
            return PttlValue::KeyMissing;
        };
        let Some(entry) = self.entries.get_mut(key) else {
            return PttlValue::KeyMissing;
        };
        entry.touch(now_ms);
        let decision = evaluate_expiry(now_ms, entry.expires_at_ms);
        if decision.remaining_ms == -1 {
            PttlValue::NoExpiry
        } else {
            PttlValue::Remaining(decision.remaining_ms)
        }
    }

    pub fn append(&mut self, key: &[u8], value: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        if let Some(result) = self.with_mutated_entry(key, |entry| match &mut entry.value {
            Value::String(v) => {
                v.extend_from_slice(value);
                let len = v.len();
                entry.touch_write(now_ms);
                // (br-frankenredis-84bv)
                entry.force_raw_encoding = true;
                Ok(len)
            }
            _ => Err(StoreError::WrongType),
        }) {
            if result.is_ok() {
                self.dirty = self.dirty.saturating_add(1);
            }
            result
        } else {
            let len = value.len();
            self.internal_entries_insert(
                key.to_vec(),
                Entry::new(Value::String(value.to_vec()), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
            Ok(len)
        }
    }

    pub fn strlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let len = v.len();
                    entry.touch(now_ms);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    /// MGET returns values for each key; non-string keys return None (like Redis).
    #[must_use]
    pub fn mget(&mut self, keys: &[&[u8]], now_ms: u64) -> Vec<Option<Vec<u8>>> {
        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            if !self.record_keyspace_lookup(key, now_ms) {
                results.push(None);
                continue;
            }
            let val = match self.entries.get_mut(*key) {
                Some(entry) => match &entry.value {
                    Value::String(v) => {
                        let v = v.clone();
                        entry.touch(now_ms);
                        Some(v)
                    }
                    _ => None,
                },
                None => None,
            };
            results.push(val);
        }
        results
    }

    pub fn setnx(&mut self, key: Vec<u8>, value: Vec<u8>, now_ms: u64) -> bool {
        self.drop_if_expired(&key, now_ms);
        if self.entries.contains_key(&key) {
            return false;
        }
        self.internal_entries_insert(key, Entry::new(Value::String(value), None, now_ms));
        self.dirty = self.dirty.saturating_add(1);
        true
    }

    pub fn getset(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(&key, now_ms);
        let old = match self.entries.get(&key) {
            Some(entry) => match &entry.value {
                Value::String(v) => Some(v.clone()),
                _ => return Err(StoreError::WrongType),
            },
            None => None,
        };
        self.internal_entries_insert(key, Entry::new(Value::String(value), None, now_ms));
        self.dirty = self.dirty.saturating_add(1);
        Ok(old)
    }

    pub fn incrby(&mut self, key: &[u8], delta: i64, now_ms: u64) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (current, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (parse_i64(v)?, entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (0_i64, None),
        };
        let next = current
            .checked_add(delta)
            .ok_or(StoreError::IntegerOverflow)?;
        self.internal_entries_insert(
            key.to_vec(),
            Entry::new(
                Value::String(next.to_string().into_bytes()),
                expires_at_ms,
                now_ms,
            ),
        );
        self.dirty = self.dirty.saturating_add(1);
        Ok(next)
    }

    pub fn incrbyfloat(&mut self, key: &[u8], delta: f64, now_ms: u64) -> Result<f64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (current, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (parse_f64(v)?, entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (0.0_f64, None),
        };
        let next = current + delta;
        if next.is_nan() || next.is_infinite() {
            return Err(StoreError::IncrFloatNaN);
        }
        self.internal_entries_insert(
            key.to_vec(),
            Entry::new(
                Value::String(next.to_string().into_bytes()),
                expires_at_ms,
                now_ms,
            ),
        );
        self.dirty = self.dirty.saturating_add(1);
        Ok(next)
    }

    pub fn getdel(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(_) => {}
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        }
        let Some(entry) = self.internal_entries_remove(key) else {
            return Ok(None);
        };
        self.stream_groups.remove(key);
        self.stream_last_ids.remove(key);
        self.dirty = self.dirty.saturating_add(1);
        match entry.value {
            Value::String(v) => Ok(Some(v)),
            _ => Err(StoreError::WrongType),
        }
    }

    pub fn getrange(
        &mut self,
        key: &[u8],
        start: i64,
        end: i64,
        now_ms: u64,
    ) -> Result<Vec<u8>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let len = v.len() as i64;
                    // Upstream t_string.c::getrangeCommand normalizes
                    // negative offsets relative to length and clamps
                    // BOTH start and end at 0 — fr previously only
                    // clamped start, so a fully-negative range like
                    // (-100, -90) on a length-6 string left end=-84
                    // and the 's > e' guard wrongly returned empty
                    // instead of the clamped slice [0..=0].
                    // (br-frankenredis-grangneg)
                    let mut s = if start < 0 { len + start } else { start };
                    let mut e = if end < 0 { len + end } else { end };
                    if s < 0 {
                        s = 0;
                    }
                    if e < 0 {
                        e = 0;
                    }
                    if s > e || len == 0 || s >= len {
                        Ok(Vec::new())
                    } else {
                        let e_idx = e.min(len - 1) as usize;
                        Ok(v[s as usize..e_idx + 1].to_vec())
                    }
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn setrange(
        &mut self,
        key: &[u8],
        offset: usize,
        value: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        if value.is_empty() {
            return match self.entries.get(key) {
                Some(entry) => match &entry.value {
                    Value::String(v) => Ok(v.len()),
                    _ => Err(StoreError::WrongType),
                },
                None => Ok(0),
            };
        }
        let needed = offset + value.len();
        match self.with_mutated_entry(key, |entry| {
            let len = match &mut entry.value {
                Value::String(v) => {
                    if v.len() < needed {
                        v.resize(needed, 0);
                    }
                    v[offset..offset + value.len()].copy_from_slice(value);
                    v.len()
                }
                _ => return Err(StoreError::WrongType),
            };
            entry.touch_write(now_ms);
            // (br-frankenredis-84bv)
            entry.force_raw_encoding = true;
            Ok(len)
        }) {
            Some(result) => {
                if result.is_ok() {
                    self.dirty = self.dirty.saturating_add(1);
                }
                result
            }
            None => {
                let mut current = vec![0; needed];
                current[offset..offset + value.len()].copy_from_slice(value);
                let new_len = current.len();
                let mut entry = Entry::new(Value::String(current), None, now_ms);
                // (br-frankenredis-84bv)
                entry.force_raw_encoding = true;
                self.internal_entries_insert(key.to_vec(), entry);
                self.dirty = self.dirty.saturating_add(1);
                Ok(new_len)
            }
        }
    }

    // ── Bitmap (string extension) operations ─────────────────────

    pub fn setbit(
        &mut self,
        key: &[u8],
        offset: usize,
        value: bool,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        let byte_idx = offset / 8;
        let bit_idx = 7 - (offset % 8); // MSB-first within each byte
        match self.with_mutated_entry(key, |entry| match &mut entry.value {
            Value::String(v) => {
                let old_len = v.len();
                if v.len() <= byte_idx {
                    v.resize(byte_idx + 1, 0);
                }
                let old_bit = (v[byte_idx] >> bit_idx) & 1 == 1;
                if value {
                    v[byte_idx] |= 1 << bit_idx;
                } else {
                    v[byte_idx] &= !(1 << bit_idx);
                }
                let changed = old_len != v.len() || old_bit != value;
                entry.touch_write(now_ms);
                Ok((old_bit, changed))
            }
            _ => Err(StoreError::WrongType),
        }) {
            Some(result) => {
                let (old_bit, changed) = result?;
                if changed {
                    self.dirty = self.dirty.saturating_add(1);
                }
                Ok(old_bit)
            }
            None => {
                let mut v = vec![0; byte_idx + 1];
                let old_bit = false;
                if value {
                    v[byte_idx] |= 1 << bit_idx;
                }
                self.internal_entries_insert(
                    key.to_vec(),
                    Entry::new(Value::String(v), None, now_ms),
                );
                self.dirty = self.dirty.saturating_add(1);
                Ok(old_bit)
            }
        }
    }

    pub fn getbit(&mut self, key: &[u8], offset: usize, now_ms: u64) -> Result<bool, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(false);
        }
        match self.entries.get_mut(key) {
            Some(entry) => {
                let is_string = matches!(&entry.value, Value::String(_));
                if is_string {
                    entry.touch(now_ms);
                }
                match &entry.value {
                    Value::String(v) => {
                        let byte_idx = offset / 8;
                        let bit_idx = 7 - (offset % 8);
                        if byte_idx >= v.len() {
                            Ok(false)
                        } else {
                            Ok((v[byte_idx] >> bit_idx) & 1 == 1)
                        }
                    }
                    _ => Err(StoreError::WrongType),
                }
            }
            None => Ok(false),
        }
    }

    /// Read an arbitrary-width integer field from the string at `key`.
    /// `bit_offset` is the starting bit position (MSB-first, bit 0 = MSB of byte 0).
    /// `bits` is the field width (1-64).
    /// `signed` indicates whether to sign-extend the result.
    /// If the key does not exist, it behaves as if reading from an infinite stream of zero bytes.
    pub fn bitfield_get(
        &mut self,
        key: &[u8],
        bit_offset: u64,
        bits: u8,
        signed: bool,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(bitfield_read(&[], bit_offset, bits, signed));
        }
        let bytes = match self.entries.get_mut(key) {
            Some(entry) => {
                let is_string = matches!(&entry.value, Value::String(_));
                if is_string {
                    entry.touch(now_ms);
                }
                match &entry.value {
                    Value::String(v) => v.as_slice(),
                    _ => return Err(StoreError::WrongType),
                }
            }
            None => &[],
        };
        Ok(bitfield_read(bytes, bit_offset, bits, signed))
    }

    /// Write an arbitrary-width integer field to the string at `key`.
    /// Returns the old value at that field position.
    /// Auto-creates/extends the string as needed.
    pub fn bitfield_set(
        &mut self,
        key: &[u8],
        bit_offset: u64,
        bits: u8,
        value: i64,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (mut bytes, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (v.clone(), entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (Vec::new(), None),
        };

        // Read old value first
        let signed = false; // Old value is read as unsigned for SET
        let old_value = bitfield_read(&bytes, bit_offset, bits, signed);

        // Ensure the byte array is large enough
        let end_bit = bit_offset.saturating_add(u64::from(bits));
        let needed_bytes = end_bit.div_ceil(8) as usize;
        if needed_bytes > 512 * 1024 * 1024 {
            return Err(StoreError::GenericError(
                "ERR string exceeds maximum allowed size (512MB)".to_string(),
            ));
        }

        let old_len = bytes.len();
        if bytes.len() < needed_bytes {
            bytes.resize(needed_bytes, 0);
        }

        // Write the new value
        bitfield_write(&mut bytes, bit_offset, bits, value);

        let signed = false;
        if old_len != bytes.len() || old_value != bitfield_read(&bytes, bit_offset, bits, signed) {
            self.dirty = self.dirty.saturating_add(1);
        }

        self.internal_entries_insert(
            key.to_vec(),
            Entry::new(Value::String(bytes), expires_at_ms, now_ms),
        );
        Ok(old_value)
    }

    pub fn bitcount(
        &mut self,
        key: &[u8],
        start: Option<i64>,
        end: Option<i64>,
        unit: BitRangeUnit,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => {
                let is_string = matches!(&entry.value, Value::String(_));
                if is_string {
                    entry.touch(now_ms);
                }
                match &entry.value {
                    Value::String(v) => {
                        let len = i64::try_from(v.len()).unwrap_or(i64::MAX);
                        let total_len = match unit {
                            BitRangeUnit::Byte => len,
                            BitRangeUnit::Bit => len.saturating_mul(8),
                        };
                        if total_len == 0 {
                            return Ok(0);
                        }

                        let mut range_start = start.unwrap_or(0);
                        let mut range_end = end.unwrap_or(total_len - 1);

                        if start.is_some_and(|s| s < 0)
                            && end.is_some_and(|e| e < 0)
                            && range_start > range_end
                        {
                            return Ok(0);
                        }

                        if range_start < 0 {
                            range_start += total_len;
                        }
                        if range_end < 0 {
                            range_end += total_len;
                        }
                        if range_start < 0 {
                            range_start = 0;
                        }
                        if range_end < 0 {
                            range_end = 0;
                        }
                        if range_end >= total_len {
                            range_end = total_len - 1;
                        }
                        if range_start > range_end {
                            return Ok(0);
                        }

                        match unit {
                            BitRangeUnit::Byte => {
                                let start_idx = usize::try_from(range_start)
                                    .expect("non-negative byte range start");
                                let end_idx = usize::try_from(range_end)
                                    .expect("non-negative byte range end");
                                let end_idx_excl = end_idx + 1;
                                Ok(v[start_idx..end_idx_excl]
                                    .iter()
                                    .map(|b| b.count_ones() as usize)
                                    .sum())
                            }
                            BitRangeUnit::Bit => {
                                let start_byte = usize::try_from(range_start >> 3)
                                    .expect("non-negative bit range start byte");
                                let end_byte = usize::try_from(range_end >> 3)
                                    .expect("non-negative bit range end byte");
                                let mut count: usize = v[start_byte..=end_byte]
                                    .iter()
                                    .map(|b| b.count_ones() as usize)
                                    .sum();

                                let first_byte_neg_mask =
                                    (!((1_u16 << (8 - ((range_start & 7) as u32))) - 1) & 0xFF)
                                        as u8;
                                let last_byte_neg_mask =
                                    ((1_u16 << (7 - ((range_end & 7) as u32))) - 1) as u8;
                                if first_byte_neg_mask != 0 || last_byte_neg_mask != 0 {
                                    let masked_edges = [
                                        v[start_byte] & first_byte_neg_mask,
                                        v[end_byte] & last_byte_neg_mask,
                                    ];
                                    count -= masked_edges
                                        .iter()
                                        .map(|b| b.count_ones() as usize)
                                        .sum::<usize>();
                                }
                                Ok(count)
                            }
                        }
                    }
                    _ => Err(StoreError::WrongType),
                }
            }
            None => Ok(0),
        }
    }

    pub fn bitpos(
        &mut self,
        key: &[u8],
        bit: bool,
        start: Option<i64>,
        end: Option<i64>,
        unit: BitRangeUnit,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return if bit { Ok(-1) } else { Ok(0) };
        }
        let bytes = match self.entries.get_mut(key) {
            Some(entry) => {
                let is_string = matches!(&entry.value, Value::String(_));
                if is_string {
                    entry.touch(now_ms);
                }
                match &entry.value {
                    Value::String(v) => v.as_slice(),
                    _ => return Err(StoreError::WrongType),
                }
            }
            None => return if bit { Ok(-1) } else { Ok(0) },
        };
        if bytes.is_empty() {
            return if bit { Ok(-1) } else { Ok(0) };
        }

        // Normalize start/end into the chosen unit's index space
        // (bit-or-byte). After this block, both indices are
        // non-negative and clamped to [0, total_len-1].
        let total_len = match unit {
            BitRangeUnit::Byte => bytes.len() as i64,
            BitRangeUnit::Bit => (bytes.len() as i64).saturating_mul(8),
        };
        let has_end = end.is_some();
        let s_idx = match start {
            Some(s) if s < 0 => (total_len + s).max(0),
            Some(s) => s,
            None => 0,
        };
        let mut e_idx = match end {
            Some(e) if e < 0 => (total_len + e).max(0),
            Some(e) => e,
            None => total_len - 1,
        };
        if s_idx > e_idx || s_idx >= total_len {
            return Ok(-1);
        }
        if e_idx >= total_len {
            e_idx = total_len - 1;
        }

        // Convert (s_idx, e_idx) into a byte-range with first/last
        // byte masks. In BYTE mode the mask covers the whole byte
        // (s_bit_in_byte=0, e_bit_in_byte=7); in BIT mode the masks
        // narrow to the user-requested bit window.
        let (s_byte, s_bit_in_byte, e_byte, e_bit_in_byte) = match unit {
            BitRangeUnit::Byte => (s_idx as usize, 0u8, e_idx as usize, 7u8),
            BitRangeUnit::Bit => (
                (s_idx / 8) as usize,
                (s_idx % 8) as u8,
                (e_idx / 8) as usize,
                (e_idx % 8) as u8,
            ),
        };

        // First/last byte keep-masks (1 in the bits inside the
        // requested range, 0 outside). `0xFFu8 >> n` and `0xFFu8 <<
        // (7 - n)` are well-defined for n in 0..=7 — the Bit-mode
        // arithmetic above guarantees that domain.
        let first_keep_mask: u8 = 0xFFu8 >> s_bit_in_byte;
        let last_keep_mask: u8 = 0xFFu8 << (7 - e_bit_in_byte);

        for (byte_offset, &raw) in bytes.iter().enumerate().take(e_byte + 1).skip(s_byte) {
            let first_byte = byte_offset == s_byte;
            let last_byte = byte_offset == e_byte;

            // Apply masks per direction. Looking for 1: zero the
            // out-of-range bits so they're skipped (`& keep_mask`).
            // Looking for 0: set the out-of-range bits to 1 so they
            // can't trip the search (`| !keep_mask`).
            let masked = if bit {
                let mut m = raw;
                if first_byte {
                    m &= first_keep_mask;
                }
                if last_byte {
                    m &= last_keep_mask;
                }
                m
            } else {
                let mut m = raw;
                if first_byte {
                    m |= !first_keep_mask;
                }
                if last_byte {
                    m |= !last_keep_mask;
                }
                m
            };

            if bit && masked != 0 {
                let bit_in_byte = masked.leading_zeros() as usize;
                return Ok((byte_offset * 8 + bit_in_byte) as i64);
            }
            if !bit && masked != 0xFF {
                let bit_in_byte = (!masked).leading_zeros() as usize;
                return Ok((byte_offset * 8 + bit_in_byte) as i64);
            }
        }

        // Upstream bitposCommand: when looking for a clear bit and
        // the caller did NOT supply an explicit end, the right of
        // the string is treated as zero-padded — return the bit
        // position just past the last covered byte. Same behavior in
        // BIT mode since `end` defaulting to `total_len - 1`
        // (the last bit) carries the same "not user-supplied" signal.
        if !bit && !has_end {
            return Ok(((e_byte + 1) * 8) as i64);
        }
        Ok(-1)
    }

    pub fn persist(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        let Some(had_expiry) = self
            .entries
            .get(key)
            .map(|entry| entry.expires_at_ms.is_some())
        else {
            return false;
        };
        if !had_expiry {
            return false;
        }
        self.with_mutated_entry(key, |entry| {
            entry.expires_at_ms = None;
        });
        self.expires_count = self.expires_count.saturating_sub(1);
        let db = decode_db_key(key).map(|(db, _)| db).unwrap_or(0);
        if db < self.database_count {
            self.db_expires_counts[db] = self.db_expires_counts[db].saturating_sub(1);
        }
        self.dirty = self.dirty.saturating_add(1);
        true
    }

    #[must_use]
    pub fn value_type(&mut self, key: &[u8], now_ms: u64) -> Option<ValueType> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return None;
        }
        let entry = self.entries.get(key)?;
        Some(match &entry.value {
            Value::String(_) => ValueType::String,
            Value::Hash(_) => ValueType::Hash,
            Value::List(_) => ValueType::List,
            Value::Set(_) => ValueType::Set,
            Value::SortedSet(_) => ValueType::ZSet,
            Value::Stream(_) => ValueType::Stream,
        })
    }

    #[must_use]
    pub fn key_type(&mut self, key: &[u8], now_ms: u64) -> Option<&'static str> {
        self.value_type(key, now_ms).map(ValueType::as_str)
    }

    /// Return the Redis-compatible encoding name for the value at `key`.
    #[must_use]
    pub fn object_encoding(&mut self, key: &[u8], now_ms: u64) -> Option<&'static str> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return None;
        }
        // Redis 7.4: any hash carrying a per-field TTL flips its encoding
        // from listpack/hashtable to the listpack_ex / hashtable_ex variant.
        // (br-frankenredis-omff)
        let hash_has_field_ttl = self
            .hash_field_expires
            .range((key.to_vec(), Vec::new())..)
            .next()
            .is_some_and(|((k, _), _)| k.as_slice() == key);
        let entry = self.entries.get(key)?;
        Some(match &entry.value {
            Value::String(v) => {
                // Redis returns "int" for strings that are the canonical
                // representation of an i64 (round-trip: parse then format must match)
                if !entry.force_raw_encoding
                    && let Ok(s) = std::str::from_utf8(v)
                    && let Ok(n) = s.parse::<i64>()
                    && n.to_string() == s
                {
                    "int"
                } else if !entry.force_raw_encoding && v.len() <= 44 {
                    "embstr"
                } else {
                    "raw"
                }
            }
            Value::Hash(m) => {
                let fits_listpack = m.len() <= self.hash_max_listpack_entries
                    && m.iter().all(|(k, v)| {
                        k.len() <= self.hash_max_listpack_value
                            && v.len() <= self.hash_max_listpack_value
                    });
                match (fits_listpack, hash_has_field_ttl) {
                    (true, false) => "listpack",
                    (false, false) => "hashtable",
                    (true, true) => "listpack_ex",
                    (false, true) => "hashtable_ex",
                }
            }
            Value::List(l) => {
                if self.list_fits_legacy_listpack_size(l)
                    && l.len() <= self.list_max_listpack_entries
                    && l.iter().all(|v| v.len() <= self.list_max_listpack_value)
                {
                    "listpack"
                } else {
                    "quicklist"
                }
            }
            Value::Set(s) => {
                if s.len() <= self.set_max_intset_entries && s.iter().all(|m| parse_i64(m).is_ok())
                {
                    "intset"
                } else if s.len() <= self.set_max_listpack_entries
                    && s.iter().all(|m| m.len() <= 64)
                {
                    "listpack"
                } else {
                    "hashtable"
                }
            }
            Value::SortedSet(zs) => {
                if zs.len() <= self.zset_max_listpack_entries
                    && zs.keys().all(|k| k.len() <= self.zset_max_listpack_value)
                {
                    "listpack"
                } else {
                    "skiplist"
                }
            }
            Value::Stream(_) => "stream",
        })
    }

    /// Return idle time in seconds for a key (time since last access).
    pub fn object_idletime(&mut self, key: &[u8], now_ms: u64) -> Option<u64> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return None;
        }
        self.entries.get(key).map(|entry| {
            let idle_ms = now_ms.saturating_sub(entry.last_access_ms);
            idle_ms / 1000
        })
    }

    /// Return the LFU access frequency counter for a key without mutating it.
    pub fn object_freq(&mut self, key: &[u8], now_ms: u64) -> Option<u8> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return None;
        }
        self.entries
            .get(key)
            .map(|entry| entry.current_lfu_freq(now_ms, self.lfu_decay_time))
    }

    /// Touch a key (update its last access time) without modifying the value.
    pub fn touch_key(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        let lfu_tracking_enabled = self.lfu_tracking_enabled();
        if let Some(entry) = self.entries.get_mut(key) {
            if lfu_tracking_enabled {
                entry.bump_lfu_freq(now_ms, self.lfu_decay_time);
            }
            entry.touch(now_ms);
            true
        } else {
            false
        }
    }

    pub fn rename(&mut self, key: &[u8], newkey: &[u8], now_ms: u64) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        if !self.entries.contains_key(key) {
            return Err(StoreError::KeyNotFound);
        }
        if key == newkey {
            return Ok(());
        }
        let Some(entry) = self.internal_entries_remove(key) else {
            return Err(StoreError::KeyNotFound);
        };
        let moved_groups = self.stream_groups.remove(key);
        let moved_last_id = self.stream_last_ids.remove(key);
        self.internal_entries_remove(newkey);
        self.stream_groups.remove(newkey);
        self.stream_last_ids.remove(newkey);
        self.internal_entries_insert(newkey.to_vec(), entry);
        if let Some(groups) = moved_groups {
            self.stream_groups.insert(newkey.to_vec(), groups);
        }
        if let Some(last_id) = moved_last_id {
            self.stream_last_ids.insert(newkey.to_vec(), last_id);
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(())
    }

    pub fn renamenx(&mut self, key: &[u8], newkey: &[u8], now_ms: u64) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.drop_if_expired(newkey, now_ms);
        if !self.entries.contains_key(key) {
            return Err(StoreError::KeyNotFound);
        }
        if self.entries.contains_key(newkey) {
            return Ok(false);
        }
        let Some(entry) = self.internal_entries_remove(key) else {
            return Err(StoreError::KeyNotFound);
        };
        let moved_groups = self.stream_groups.remove(key);
        let moved_last_id = self.stream_last_ids.remove(key);
        self.internal_entries_insert(newkey.to_vec(), entry);
        if let Some(groups) = moved_groups {
            self.stream_groups.insert(newkey.to_vec(), groups);
        }
        if let Some(last_id) = moved_last_id {
            self.stream_last_ids.insert(newkey.to_vec(), last_id);
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(true)
    }

    #[must_use]
    pub fn keys_in_db(&mut self, db: usize, now_ms: u64) -> Vec<Vec<u8>> {
        let physical_keys = self.ordered_physical_keys_in_db(db);

        for key in &physical_keys {
            self.drop_if_expired(key, now_ms);
        }

        self.ordered_physical_keys_in_db(db)
            .into_iter()
            .map(|key| {
                decode_db_key(&key)
                    .map(|(_, logical)| logical.to_vec())
                    .unwrap_or(key)
            })
            .collect()
    }

    #[must_use]
    pub fn keys_matching(&mut self, pattern: &[u8], now_ms: u64) -> Vec<Vec<u8>> {
        // Fallback to O(N) scan across all databases if DB index is not known.
        // This is primarily for unit tests and direct fr-command usage.
        let physical_keys: Vec<Vec<u8>> = self.ordered_keys.iter().cloned().collect();
        for key in &physical_keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut result: Vec<Vec<u8>> = self
            .ordered_keys
            .iter()
            .filter(|key| glob_match(pattern, key))
            .cloned()
            .collect();
        result.sort();
        result
    }

    #[must_use]
    pub fn keys_matching_in_db(&mut self, db: usize, pattern: &[u8], now_ms: u64) -> Vec<Vec<u8>> {
        let physical_keys = self.ordered_physical_keys_in_db(db);

        for key in &physical_keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut result: Vec<Vec<u8>> = self
            .ordered_physical_keys_in_db(db)
            .into_iter()
            .filter_map(|key| {
                let logical = decode_db_key(&key)
                    .map(|(_, logical)| logical)
                    .unwrap_or(key.as_slice());
                if glob_match(pattern, logical) {
                    Some(logical.to_vec())
                } else {
                    None
                }
            })
            .collect();
        result.sort();
        result
    }

    #[must_use]
    pub fn dbsize(&self, _now_ms: u64) -> usize {
        // DBSIZE must be O(1). We do not actively reap expired keys here
        // as that would require an O(N) scan. This matches legacy Redis
        // behavior which returns the total count including pending expires.
        self.entries.len()
    }

    #[must_use]
    pub fn dbsize_in_db(&self, db: usize) -> usize {
        if db < self.database_count {
            self.db_key_counts[db]
        } else {
            0
        }
    }

    #[must_use]
    pub fn expires_in_db(&self, db: usize) -> usize {
        if db < self.database_count {
            self.db_expires_counts[db]
        } else {
            0
        }
    }

    #[must_use]
    pub fn count_expiring_keys(&self) -> usize {
        self.expires_count
    }

    fn internal_entry(&mut self, key: Vec<u8>, default_value: Value, now_ms: u64) -> &mut Entry {
        if !self.entries.contains_key(&key) {
            self.internal_entries_insert(key.clone(), Entry::new(default_value, None, now_ms));
        }
        self.entries
            .get_mut(&key)
            .expect("entry must exist after internal insertion")
    }

    fn bump_digest_mutations(&mut self) {
        self.digest_mutations = self.digest_mutations.wrapping_add(1);
    }

    fn mark_digest_stale_fields(digest_stale: &mut bool, digest_mutations: &mut u64) {
        *digest_stale = true;
        *digest_mutations = digest_mutations.wrapping_add(1);
    }

    fn update_digest_hashes(&mut self, old_hash: Option<u64>, new_hash: Option<u64>) {
        if self.digest_stale {
            if old_hash.is_some() || new_hash.is_some() {
                self.bump_digest_mutations();
            }
            return;
        }
        if let Some(old_hash) = old_hash {
            self.running_digest ^= old_hash;
        }
        if let Some(new_hash) = new_hash {
            self.running_digest ^= new_hash;
        }
        self.digest_stale = false;
        if old_hash.is_some() || new_hash.is_some() {
            self.bump_digest_mutations();
        }
    }

    fn refresh_entry_digest(&mut self, key: &[u8], old_hash: u64) {
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        self.update_digest_hashes(Some(old_hash), Some(Self::entry_state_digest(key, entry)));
    }

    fn current_entry_digest(&self, key: &[u8]) -> Option<u64> {
        self.entries
            .get(key)
            .map(|entry| Self::entry_state_digest(key, entry))
    }

    fn with_mutated_entry<R>(
        &mut self,
        key: &[u8],
        mutate: impl FnOnce(&mut Entry) -> R,
    ) -> Option<R> {
        let old_hash = self.current_entry_digest(key)?;
        let result = {
            let entry = self
                .entries
                .get_mut(key)
                .expect("entry hash captured above");
            mutate(entry)
        };
        self.refresh_entry_digest(key, old_hash);
        Some(result)
    }

    fn state_digest_full_scan(&self) -> u64 {
        self.entries.iter().fold(0_u64, |digest, (key, entry)| {
            digest ^ Self::entry_state_digest(key, entry)
        })
    }

    fn internal_entries_insert(&mut self, key: Vec<u8>, mut entry: Entry) -> Option<Entry> {
        let db = decode_db_key(&key).map(|(db, _)| db).unwrap_or(0);
        let is_new_key = !self.entries.contains_key(&key);
        let old_digest = self
            .entries
            .get(&key)
            .map(|existing| Self::entry_state_digest(&key, existing));

        if let Some(old_entry) = self.entries.get(&key) {
            entry.modification_count = old_entry.modification_count.wrapping_add(1);
        }

        if entry.expires_at_ms.is_some() {
            self.expires_count = self.expires_count.saturating_add(1);
            if db < self.database_count {
                self.db_expires_counts[db] = self.db_expires_counts[db].saturating_add(1);
            }
        }
        if is_new_key {
            self.ordered_keys.insert(key.clone());
        }
        if let Some(old) = self.entries.insert(key.clone(), entry) {
            if old.expires_at_ms.is_some() {
                self.expires_count = self.expires_count.saturating_sub(1);
                if db < self.database_count {
                    self.db_expires_counts[db] = self.db_expires_counts[db].saturating_sub(1);
                }
            }
            let new_digest = self
                .entries
                .get(&key)
                .map(|inserted| Self::entry_state_digest(&key, inserted));
            self.update_digest_hashes(old_digest, new_digest);
            Some(old)
        } else {
            if db < self.database_count {
                self.db_key_counts[db] = self.db_key_counts[db].saturating_add(1);
            }
            let new_digest = self
                .entries
                .get(&key)
                .map(|inserted| Self::entry_state_digest(&key, inserted));
            self.update_digest_hashes(None, new_digest);
            None
        }
    }

    fn internal_entries_remove(&mut self, key: &[u8]) -> Option<Entry> {
        if let Some(entry) = self.entries.remove(key) {
            self.ordered_keys.remove(key);
            let db = decode_db_key(key).map(|(db, _)| db).unwrap_or(0);
            if db < self.database_count {
                self.db_key_counts[db] = self.db_key_counts[db].saturating_sub(1);
            }
            if entry.expires_at_ms.is_some() {
                self.expires_count = self.expires_count.saturating_sub(1);
                if db < self.database_count {
                    self.db_expires_counts[db] = self.db_expires_counts[db].saturating_sub(1);
                }
            }
            self.update_digest_hashes(Some(Self::entry_state_digest(key, &entry)), None);
            // Whole-key removal drops any per-field hash TTL entries so the
            // field_expires map doesn't accumulate orphan rows.
            // (br-frankenredis-b8ut)
            self.hash_field_ttl_clear_for_key(key);
            Some(entry)
        } else {
            None
        }
    }

    /// Emit the upstream-matching `hexpired` keyspace notification for a
    /// single reaped field. The event lives on the key (not the field —
    /// upstream keyspace events are key-scoped), same convention as the
    /// whole-key `expired` notification. (br-frankenredis-omff)
    fn notify_hash_field_expired(&mut self, key: &[u8]) {
        if self.notify_keyspace_events == 0 {
            return;
        }
        let (db, logical_key) = match decode_db_key(key) {
            Some((db, lk)) => (db, lk.to_vec()),
            None => (0, key.to_vec()),
        };
        self.notify_keyspace_event(NOTIFY_EXPIRED, "hexpired", &logical_key, db);
    }

    /// Drop every expired per-field TTL entry on `key` from the hash and
    /// from the hash_field_expires map. Called by the hash-read surface
    /// so expired fields are invisible to clients (Redis 7.4 semantic).
    /// Returns the count of fields that were reaped.
    /// (br-frankenredis-b8ut)
    fn drop_expired_hash_fields(&mut self, key: &[u8], now_ms: u64) -> usize {
        // Collect expired (key, field) pairs via the BTreeMap prefix range.
        let expired_fields: Vec<Vec<u8>> = self
            .hash_field_expires
            .range((key.to_vec(), Vec::new())..)
            .take_while(|((k, _), _)| k.as_slice() == key)
            .filter(|&(_, &at)| at <= now_ms)
            .map(|((_, f), _)| f.clone())
            .collect();
        if expired_fields.is_empty() {
            return 0;
        }

        let mut reaped = 0usize;
        let mut became_empty = false;
        if let Some(entry) = self.entries.get_mut(key)
            && let Value::Hash(map) = &mut entry.value
        {
            for field in &expired_fields {
                if map.remove(field.as_slice()).is_some() {
                    reaped += 1;
                }
            }
            became_empty = map.is_empty();
        }
        for field in &expired_fields {
            self.hash_field_expires
                .remove(&(key.to_vec(), field.clone()));
        }
        if reaped > 0 {
            self.dirty = self.dirty.saturating_add(reaped as u64);
            self.stat_expired_keys = self.stat_expired_keys.saturating_add(reaped as u64);
            *self
                .hash_field_expired_counts
                .entry(key.to_vec())
                .or_insert(0) = self
                .hash_field_expired_counts
                .get(key)
                .copied()
                .unwrap_or(0)
                .saturating_add(reaped as u64);
            for _ in 0..reaped {
                self.notify_hash_field_expired(key);
            }
        }
        if became_empty {
            // Upstream behavior: a hash with no fields is removed entirely.
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
        }
        reaped
    }

    /// Drop a single per-field TTL entry on `key`/`field` if its deadline
    /// has passed. Returns true if the field was reaped.
    /// (br-frankenredis-b8ut)
    fn drop_hash_field_if_expired(&mut self, key: &[u8], field: &[u8], now_ms: u64) -> bool {
        if !self.hash_field_is_expired(key, field, now_ms) {
            return false;
        }
        let composite = (key.to_vec(), field.to_vec());
        self.hash_field_expires.remove(&composite);

        let mut became_empty = false;
        let mut removed = false;
        if let Some(entry) = self.entries.get_mut(key)
            && let Value::Hash(map) = &mut entry.value
        {
            if map.remove(field).is_some() {
                removed = true;
            }
            became_empty = map.is_empty();
        }
        if removed {
            self.dirty = self.dirty.saturating_add(1);
            self.stat_expired_keys = self.stat_expired_keys.saturating_add(1);
            let entry = self
                .hash_field_expired_counts
                .entry(key.to_vec())
                .or_insert(0);
            *entry = entry.saturating_add(1);
            self.notify_hash_field_expired(key);
        }
        if became_empty {
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
        }
        removed
    }

    #[must_use]
    pub fn classify_maxmemory_pressure(
        &self,
        maxmemory_bytes: usize,
        not_counted_bytes: usize,
    ) -> MaxmemoryPressureState {
        let logical_usage_bytes = self.estimate_memory_usage_bytes();
        let counted_usage_bytes = logical_usage_bytes.saturating_sub(not_counted_bytes);
        let bytes_to_free = if maxmemory_bytes == 0 {
            0
        } else {
            counted_usage_bytes.saturating_sub(maxmemory_bytes)
        };
        let level = if bytes_to_free == 0 {
            MaxmemoryPressureLevel::None
        } else if bytes_to_free.saturating_mul(20) <= maxmemory_bytes {
            MaxmemoryPressureLevel::Soft
        } else {
            MaxmemoryPressureLevel::Hard
        };

        MaxmemoryPressureState {
            maxmemory_bytes,
            logical_usage_bytes,
            not_counted_bytes,
            counted_usage_bytes,
            bytes_to_free,
            level,
        }
    }

    #[must_use]
    pub fn run_bounded_eviction_loop(
        &mut self,
        now_ms: u64,
        maxmemory_bytes: usize,
        not_counted_bytes: usize,
        sample_limit: usize,
        max_cycles: usize,
        safety_gate: EvictionSafetyGateState,
    ) -> EvictionLoopResult {
        let initial_state = self.classify_maxmemory_pressure(maxmemory_bytes, not_counted_bytes);
        if initial_state.bytes_to_free == 0 {
            return EvictionLoopResult {
                status: EvictionLoopStatus::Ok,
                failure: None,
                sampled_keys: 0,
                evicted_keys: 0,
                bytes_freed: 0,
                bytes_to_free_after: 0,
            };
        }

        if safety_gate.blocks_eviction() {
            return EvictionLoopResult {
                status: EvictionLoopStatus::Fail,
                failure: Some(EvictionLoopFailure::SafetyGateSuppressed),
                sampled_keys: 0,
                evicted_keys: 0,
                bytes_freed: 0,
                bytes_to_free_after: initial_state.bytes_to_free,
            };
        }

        let sample_limit = sample_limit.max(1);
        let mut cursor: Option<Vec<u8>> = None;
        let mut sampled_keys = 0usize;
        let mut evicted_keys = 0usize;
        let mut bytes_freed = 0usize;

        for _ in 0..max_cycles {
            let before_state = self.classify_maxmemory_pressure(maxmemory_bytes, not_counted_bytes);
            if before_state.bytes_to_free == 0 {
                return EvictionLoopResult {
                    status: EvictionLoopStatus::Ok,
                    failure: None,
                    sampled_keys,
                    evicted_keys,
                    bytes_freed,
                    bytes_to_free_after: 0,
                };
            }

            let cycle = self.run_active_expire_cycle(now_ms, cursor.clone(), sample_limit);
            sampled_keys = sampled_keys.saturating_add(cycle.sampled_keys);
            evicted_keys = evicted_keys.saturating_add(cycle.evicted_keys);
            cursor = cycle.next_cursor;

            if cycle.evicted_keys == 0 {
                let Some(candidate) = self.select_eviction_candidate(now_ms) else {
                    break;
                };
                if self.internal_entries_remove(candidate.as_slice()).is_some() {
                    self.stream_groups.remove(candidate.as_slice());
                    self.stream_last_ids.remove(candidate.as_slice());
                    evicted_keys = evicted_keys.saturating_add(1);
                    self.stat_evicted_keys = self.stat_evicted_keys.saturating_add(1);
                    self.cached_memory_usage_bytes.set(0); // Force cache invalidation to track exact bytes freed
                    // Emit evicted keyspace notification (use logical key)
                    let (db, logical_key) = match decode_db_key(&candidate) {
                        Some((db, lk)) => (db, lk.to_vec()),
                        None => (0, candidate.clone()),
                    };
                    self.notify_keyspace_event(NOTIFY_EVICTED, "evicted", &logical_key, db);
                }
            }

            let after_state = self.classify_maxmemory_pressure(maxmemory_bytes, not_counted_bytes);
            bytes_freed = bytes_freed.saturating_add(
                before_state
                    .counted_usage_bytes
                    .saturating_sub(after_state.counted_usage_bytes),
            );
        }

        let final_state = self.classify_maxmemory_pressure(maxmemory_bytes, not_counted_bytes);
        if final_state.bytes_to_free == 0 {
            EvictionLoopResult {
                status: EvictionLoopStatus::Ok,
                failure: None,
                sampled_keys,
                evicted_keys,
                bytes_freed,
                bytes_to_free_after: 0,
            }
        } else if evicted_keys > 0 {
            EvictionLoopResult {
                status: EvictionLoopStatus::Running,
                failure: None,
                sampled_keys,
                evicted_keys,
                bytes_freed,
                bytes_to_free_after: final_state.bytes_to_free,
            }
        } else {
            EvictionLoopResult {
                status: EvictionLoopStatus::Fail,
                failure: Some(EvictionLoopFailure::NoCandidates),
                sampled_keys,
                evicted_keys,
                bytes_freed,
                bytes_to_free_after: final_state.bytes_to_free,
            }
        }
    }

    #[must_use]
    pub fn run_active_expire_cycle(
        &mut self,
        now_ms: u64,
        start_cursor: Option<Vec<u8>>,
        sample_limit: usize,
    ) -> ActiveExpireCycleResult {
        if sample_limit == 0 || self.entries.is_empty() {
            return ActiveExpireCycleResult {
                sampled_keys: 0,
                evicted_keys: 0,
                next_cursor: None,
            };
        }

        let keys_to_check: Vec<Vec<u8>> = match start_cursor {
            Some(ref k) => {
                let mut it = self.ordered_keys.range(k.clone()..).cloned();
                let mut collected: Vec<Vec<u8>> = it.by_ref().take(sample_limit).collect();
                if collected.len() < sample_limit {
                    // Wrap around
                    let remaining = sample_limit - collected.len();
                    collected.extend(self.ordered_keys.iter().take(remaining).cloned());
                }
                collected
            }
            None => self
                .ordered_keys
                .iter()
                .take(sample_limit)
                .cloned()
                .collect(),
        };

        let mut evicted_keys = 0usize;
        for key in &keys_to_check {
            let should_evict = evaluate_expiry(
                now_ms,
                self.entries.get(key).and_then(|entry| entry.expires_at_ms),
            )
            .should_evict;
            if should_evict {
                // Emit expired notification before removal (use logical key)
                let (db, logical_key) = match decode_db_key(key) {
                    Some((db, lk)) => (db, lk.to_vec()),
                    None => (0, key.clone()),
                };
                self.notify_keyspace_event(NOTIFY_EXPIRED, "expired", &logical_key, db);
                self.internal_entries_remove(key);
                self.stream_groups.remove(key.as_slice());
                self.stream_last_ids.remove(key.as_slice());
                evicted_keys = evicted_keys.saturating_add(1);
                self.stat_expired_keys = self.stat_expired_keys.saturating_add(1);
            }
        }

        let next_cursor = keys_to_check.last().and_then(|last| {
            self.ordered_keys
                .range((Excluded(last.clone()), Unbounded))
                .next()
                .cloned()
        });

        ActiveExpireCycleResult {
            sampled_keys: keys_to_check.len(),
            evicted_keys,
            next_cursor,
        }
    }

    pub fn flushdb(&mut self) {
        self.entries.clear();
        self.stream_groups.clear();
        self.stream_last_ids.clear();
        self.running_digest = 0;
        self.digest_stale = false;
        self.expires_count = 0;
        self.db_key_counts.fill(0);
        self.db_expires_counts.fill(0);
        self.dirty = self.dirty.saturating_add(1);
    }

    pub fn flush_prefix(&mut self, prefix: &[u8]) -> u64 {
        let keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        let removed = keys.len() as u64;
        for key in keys {
            self.internal_entries_remove(&key);
            self.stream_groups.remove(key.as_slice());
            self.stream_last_ids.remove(key.as_slice());
        }
        self.dirty = self.dirty.saturating_add(removed.max(1));
        removed
    }

    pub fn flush_database(&mut self, db: usize) -> u64 {
        let keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| {
                decode_db_key(key)
                    .map(|(entry_db, _)| entry_db == db)
                    .unwrap_or(db == 0)
            })
            .cloned()
            .collect();
        let removed = keys.len() as u64;
        for key in keys {
            self.internal_entries_remove(&key);
            self.stream_groups.remove(key.as_slice());
            self.stream_last_ids.remove(key.as_slice());
        }
        self.dirty = self.dirty.saturating_add(removed.max(1));
        removed
    }

    pub fn swap_prefixes(&mut self, left_prefix: &[u8], right_prefix: &[u8]) -> u64 {
        if left_prefix == right_prefix {
            self.dirty = self.dirty.saturating_add(1);
            return 0;
        }

        let left_keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| key.starts_with(left_prefix))
            .cloned()
            .collect();
        let right_keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| key.starts_with(right_prefix))
            .cloned()
            .collect();

        let left_count = left_keys.len();
        let right_count = right_keys.len();

        let mut left_entries = Vec::with_capacity(left_count);
        for key in left_keys {
            let Some(entry) = self.internal_entries_remove(&key) else {
                continue;
            };
            let groups = self.stream_groups.remove(key.as_slice());
            let last_id = self.stream_last_ids.remove(key.as_slice());
            left_entries.push((key, entry, groups, last_id));
        }

        let mut right_entries = Vec::with_capacity(right_count);
        for key in right_keys {
            let Some(entry) = self.internal_entries_remove(&key) else {
                continue;
            };
            let groups = self.stream_groups.remove(key.as_slice());
            let last_id = self.stream_last_ids.remove(key.as_slice());
            right_entries.push((key, entry, groups, last_id));
        }

        for (key, entry, groups, last_id) in left_entries {
            let mut swapped = Vec::with_capacity(
                right_prefix.len() + key.len().saturating_sub(left_prefix.len()),
            );
            swapped.extend_from_slice(right_prefix);
            swapped.extend_from_slice(&key[left_prefix.len()..]);
            self.internal_entries_insert(swapped.clone(), entry);
            if let Some(groups) = groups {
                self.stream_groups.insert(swapped.clone(), groups);
            }
            if let Some(last_id) = last_id {
                self.stream_last_ids.insert(swapped, last_id);
            }
        }

        for (key, entry, groups, last_id) in right_entries {
            let mut swapped = Vec::with_capacity(
                left_prefix.len() + key.len().saturating_sub(right_prefix.len()),
            );
            swapped.extend_from_slice(left_prefix);
            swapped.extend_from_slice(&key[right_prefix.len()..]);
            self.internal_entries_insert(swapped.clone(), entry);
            if let Some(groups) = groups {
                self.stream_groups.insert(swapped.clone(), groups);
            }
            if let Some(last_id) = last_id {
                self.stream_last_ids.insert(swapped, last_id);
            }
        }

        let touched = (left_count + right_count) as u64;
        self.dirty = self.dirty.saturating_add(touched.max(1));
        touched
    }

    pub fn swap_databases(&mut self, left_db: usize, right_db: usize) -> u64 {
        if left_db == right_db {
            self.dirty = self.dirty.saturating_add(1);
            return 0;
        }

        let left_keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| {
                decode_db_key(key)
                    .map(|(db, _)| db == left_db)
                    .unwrap_or(left_db == 0)
            })
            .cloned()
            .collect();
        let right_keys: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| {
                decode_db_key(key)
                    .map(|(db, _)| db == right_db)
                    .unwrap_or(right_db == 0)
            })
            .cloned()
            .collect();

        let left_count = left_keys.len();
        let right_count = right_keys.len();

        let mut left_entries = Vec::with_capacity(left_count);
        for key in left_keys {
            let Some(entry) = self.internal_entries_remove(&key) else {
                continue;
            };
            let logical = decode_db_key(&key)
                .map(|(_, logical)| logical.to_vec())
                .unwrap_or(key.clone());
            let groups = self.stream_groups.remove(key.as_slice());
            let last_id = self.stream_last_ids.remove(key.as_slice());
            left_entries.push((logical, entry, groups, last_id));
        }

        let mut right_entries = Vec::with_capacity(right_count);
        for key in right_keys {
            let Some(entry) = self.internal_entries_remove(&key) else {
                continue;
            };
            let logical = decode_db_key(&key)
                .map(|(_, logical)| logical.to_vec())
                .unwrap_or(key.clone());
            let groups = self.stream_groups.remove(key.as_slice());
            let last_id = self.stream_last_ids.remove(key.as_slice());
            right_entries.push((logical, entry, groups, last_id));
        }

        for (logical, entry, groups, last_id) in left_entries {
            let swapped = encode_db_key(right_db, &logical);
            self.internal_entries_insert(swapped.clone(), entry);
            if let Some(groups) = groups {
                self.stream_groups.insert(swapped.clone(), groups);
            }
            if let Some(last_id) = last_id {
                self.stream_last_ids.insert(swapped, last_id);
            }
        }

        for (logical, entry, groups, last_id) in right_entries {
            let swapped = encode_db_key(left_db, &logical);
            self.internal_entries_insert(swapped.clone(), entry);
            if let Some(groups) = groups {
                self.stream_groups.insert(swapped.clone(), groups);
            }
            if let Some(last_id) = last_id {
                self.stream_last_ids.insert(swapped, last_id);
            }
        }

        let touched = (left_count + right_count) as u64;
        self.dirty = self.dirty.saturating_add(touched.max(1));
        touched
    }

    // ── Hash operations ─────────────────────────────────────────

    pub fn hset(
        &mut self,
        key: &[u8],
        field: Vec<u8>,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.internal_entry(key.to_vec(), Value::Hash(BTreeMap::new()), now_ms);
        let result = self
            .with_mutated_entry(key, |entry| {
                let Value::Hash(m) = &mut entry.value else {
                    return Err(StoreError::WrongType);
                };
                let is_new = !m.contains_key(&field);
                m.insert(field, value);
                entry.touch_write(now_ms);
                Ok(is_new)
            })
            .expect("hash entry was ensured");
        self.dirty = self.dirty.saturating_add(1);
        result
    }

    pub fn hget(
        &mut self,
        key: &[u8],
        field: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        // Reap the specific field if its per-field TTL lapsed so expired
        // fields are invisible at the hash-read layer (Redis 7.4
        // br-frankenredis-b8ut).
        self.drop_hash_field_if_expired(key, field, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let result = m.get(field).cloned();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(result) = self.with_mutated_entry(key, |entry| {
            let Value::Hash(m) = &mut entry.value else {
                return Err(StoreError::WrongType);
            };
            let mut removed = 0_u64;
            for field in fields {
                if m.remove(*field).is_some() {
                    removed += 1;
                }
            }
            let is_empty = m.is_empty();
            if removed > 0 {
                entry.touch_write(now_ms);
            }
            Ok((removed, is_empty))
        }) else {
            return Ok(0);
        };
        let (removed, is_empty) = result?;
        if removed > 0 {
            self.dirty = self.dirty.saturating_add(removed);
            // Clear any per-field TTL entries for the removed fields so
            // re-added fields of the same name don't inherit stale TTLs.
            // (br-frankenredis-b8ut)
            for field in fields {
                self.hash_field_ttl_clear_for_field(key, field);
            }
        }
        if is_empty {
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
        }
        Ok(removed)
    }

    pub fn hexists(&mut self, key: &[u8], field: &[u8], now_ms: u64) -> Result<bool, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(false);
        }
        self.drop_hash_field_if_expired(key, field, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let result = m.contains_key(field);
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    pub fn hlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        self.drop_expired_hash_fields(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let len = m.len();
                    entry.touch(now_ms);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn hgetall(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        self.drop_expired_hash_fields(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let pairs: Vec<(Vec<u8>, Vec<u8>)> =
                        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    entry.touch(now_ms);
                    Ok(pairs)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn hkeys(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        self.drop_expired_hash_fields(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let keys: Vec<Vec<u8>> = m.keys().cloned().collect();
                    entry.touch(now_ms);
                    Ok(keys)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn hvals(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        self.drop_expired_hash_fields(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let result: Vec<Vec<u8>> = m.values().cloned().collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn hmget(
        &mut self,
        key: &[u8],
        fields: &[&[u8]],
        now_ms: u64,
    ) -> Result<Vec<Option<Vec<u8>>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(fields.iter().map(|_| None).collect());
        }
        for field in fields {
            self.drop_hash_field_if_expired(key, field, now_ms);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let result: Vec<Option<Vec<u8>>> =
                        fields.iter().map(|f| m.get(*f).cloned()).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(fields.iter().map(|_| None).collect()),
        }
    }

    pub fn hincrby(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: i64,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.internal_entry(key.to_vec(), Value::Hash(BTreeMap::new()), now_ms);
        let (res, is_empty) = self
            .with_mutated_entry(key, |entry| {
                let Value::Hash(m) = &mut entry.value else {
                    return (Err(StoreError::WrongType), false);
                };
                let mut touched = false;
                let current_res = match m.get(field) {
                    Some(v) => parse_i64(v).map_err(|_| StoreError::HashValueNotInteger),
                    None => Ok(0),
                };
                let res = match current_res {
                    Ok(current) => match current.checked_add(delta) {
                        Some(next) => {
                            m.insert(field.to_vec(), next.to_string().into_bytes());
                            touched = true;
                            Ok(next)
                        }
                        None => Err(StoreError::IntegerOverflow),
                    },
                    Err(e) => Err(e),
                };
                let is_empty = m.is_empty();
                if touched {
                    entry.touch_write(now_ms);
                }
                (res, is_empty)
            })
            .expect("hash entry was ensured");
        if res.is_ok() {
            self.dirty = self.dirty.saturating_add(1);
        }
        if is_empty {
            self.internal_entries_remove(key);
        }
        res
    }

    pub fn hsetnx(
        &mut self,
        key: &[u8],
        field: Vec<u8>,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.internal_entry(key.to_vec(), Value::Hash(BTreeMap::new()), now_ms);
        let result = self
            .with_mutated_entry(key, |entry| {
                let Value::Hash(m) = &mut entry.value else {
                    return Err(StoreError::WrongType);
                };
                if let std::collections::btree_map::Entry::Vacant(slot) = m.entry(field) {
                    slot.insert(value);
                    entry.touch_write(now_ms);
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .expect("hash entry was ensured");
        if matches!(result, Ok(true)) {
            self.dirty = self.dirty.saturating_add(1);
        }
        result
    }

    pub fn hstrlen(&mut self, key: &[u8], field: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        self.drop_hash_field_if_expired(key, field, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let result = m.get(field).map_or(0, Vec::len);
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn hincrbyfloat(
        &mut self,
        key: &[u8],
        field: &[u8],
        delta: f64,
        now_ms: u64,
    ) -> Result<f64, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.internal_entry(key.to_vec(), Value::Hash(BTreeMap::new()), now_ms);
        let (res, is_empty) = self
            .with_mutated_entry(key, |entry| {
                let Value::Hash(m) = &mut entry.value else {
                    return (Err(StoreError::WrongType), false);
                };
                let mut touched = false;
                let current_res = match m.get(field) {
                    Some(v) => parse_f64(v),
                    None => Ok(0.0),
                };

                let res = match current_res {
                    Ok(current) => {
                        let next = current + delta;
                        if next.is_nan() || next.is_infinite() {
                            Err(StoreError::IncrFloatNaN)
                        } else {
                            m.insert(field.to_vec(), next.to_string().into_bytes());
                            touched = true;
                            Ok(next)
                        }
                    }
                    Err(e) => Err(e),
                };
                let is_empty = m.is_empty();
                if touched {
                    entry.touch_write(now_ms);
                }
                (res, is_empty)
            })
            .expect("hash entry was ensured");
        if res.is_ok() {
            self.dirty = self.dirty.saturating_add(1);
        }
        if is_empty {
            self.internal_entries_remove(key);
        }
        res
    }

    pub fn hrandfield(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.drop_expired_hash_fields(key, now_ms);
        let rand_val = self.next_rand();
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    if m.is_empty() {
                        return Ok(None);
                    }
                    let idx = (rand_val as usize) % m.len();
                    let field = m.keys().nth(idx).cloned();
                    entry.touch(now_ms);
                    Ok(field)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return `count` random fields from a hash.
    /// Positive count: up to `count` distinct fields.
    /// Negative count: `|count|` fields with possible repeats.
    #[allow(clippy::type_complexity)]
    pub fn hrandfield_count(
        &mut self,
        key: &[u8],
        count: i64,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        self.drop_expired_hash_fields(key, now_ms);
        // Pre-generate some random values if we need many (for negative count).
        // For positive count, we'll need many for the shuffle.
        // Actually, it's easier to just pick the random values we need inside the match if we can.
        // But we can't because of the borrow.
        // So let's handle the hash lookup first, get the fields, and then do the work.

        let mut result_type = None;
        if let Some(entry) = self.entries.get_mut(key) {
            match &entry.value {
                Value::Hash(m) => {
                    if m.is_empty() {
                        return Ok(Vec::new());
                    }
                    let fields: Vec<(Vec<u8>, Vec<u8>)> =
                        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    entry.touch(now_ms);
                    result_type = Some(fields);
                }
                _ => return Err(StoreError::WrongType),
            }
        }

        let fields = match result_type {
            Some(f) if !f.is_empty() => f,
            _ => return Ok(Vec::new()),
        };

        if count >= 0 {
            let n = (count as usize).min(fields.len());
            // Use a more memory-efficient approach for small n
            if n < fields.len() / 2 && n < 1024 {
                let mut results = Vec::with_capacity(n);
                let mut picked = HashSet::with_capacity(n);
                while results.len() < n {
                    let idx = (self.next_rand() as usize) % fields.len();
                    if picked.insert(idx) {
                        results.push(fields[idx].clone());
                    }
                }
                Ok(results)
            } else {
                let mut indices: Vec<usize> = (0..fields.len()).collect();
                for i in 0..n {
                    let j = i + (self.next_rand() as usize % (fields.len() - i));
                    indices.swap(i, j);
                }
                Ok(indices[..n]
                    .iter()
                    .map(|&idx| fields[idx].clone())
                    .collect())
            }
        } else {
            let abs_count = count.unsigned_abs() as usize;
            // Cap initial allocation to avoid DoS, but allow growth.
            let mut result = Vec::with_capacity(abs_count.min(1024));
            for _ in 0..abs_count {
                let idx = (self.next_rand() as usize) % fields.len();
                result.push(fields[idx].clone());
            }
            Ok(result)
        }
    }

    // ── List operations ─────────────────────────────────────────

    pub fn lpush(
        &mut self,
        key: &[u8],
        values: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    for v in values {
                        l.push_front(v.clone());
                    }
                    let len = l.len();
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                    self.dirty = self.dirty.saturating_add(values.len() as u64);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                for v in values {
                    l.push_front(v.clone());
                }
                let len = l.len();
                self.internal_entries_insert(
                    key.to_vec(),
                    Entry::new(Value::List(l), None, now_ms),
                );
                self.dirty = self.dirty.saturating_add(values.len() as u64);
                Ok(len)
            }
        }
    }

    pub fn rpush(
        &mut self,
        key: &[u8],
        values: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    for v in values {
                        l.push_back(v.clone());
                    }
                    let len = l.len();
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                    self.dirty = self.dirty.saturating_add(values.len() as u64);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                for v in values {
                    l.push_back(v.clone());
                }
                let len = l.len();
                self.internal_entries_insert(
                    key.to_vec(),
                    Entry::new(Value::List(l), None, now_ms),
                );
                self.dirty = self.dirty.saturating_add(values.len() as u64);
                Ok(len)
            }
        }
    }

    pub fn lpop(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let val = l.pop_front();
                    if val.is_some() {
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    if l.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if val.is_some() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    Ok(val)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn lpop_count(
        &mut self,
        key: &[u8],
        count: usize,
        now_ms: u64,
    ) -> Result<Option<Vec<Vec<u8>>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let mut result = Vec::new();
                    for _ in 0..count {
                        match l.pop_front() {
                            Some(v) => result.push(v),
                            None => break,
                        }
                    }
                    if !result.is_empty() {
                        self.dirty = self.dirty.saturating_add(result.len() as u64);
                    }
                    if l.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if !result.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    Ok(Some(result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn rpop(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let val = l.pop_back();
                    if val.is_some() {
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    if l.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if val.is_some() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    Ok(val)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn rpop_count(
        &mut self,
        key: &[u8],
        count: usize,
        now_ms: u64,
    ) -> Result<Option<Vec<Vec<u8>>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let mut result = Vec::new();
                    for _ in 0..count {
                        match l.pop_back() {
                            Some(v) => result.push(v),
                            None => break,
                        }
                    }
                    if !result.is_empty() {
                        self.dirty = self.dirty.saturating_add(result.len() as u64);
                    }
                    if l.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if !result.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    Ok(Some(result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn llen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len();
                    entry.touch(now_ms);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn lrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    let s = normalize_index(start, len).max(0);
                    let e = normalize_index(stop, len).min(len - 1);
                    if s > e || s >= len || e < 0 {
                        return Ok(Vec::new());
                    }
                    let s = s as usize;
                    let e = e as usize;
                    let result: Vec<Vec<u8>> = l.iter().skip(s).take(e - s + 1).cloned().collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn lindex(
        &mut self,
        key: &[u8],
        index: i64,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    if index < -len || index >= len {
                        return Ok(None);
                    }
                    let idx = normalize_index(index, len) as usize;
                    let result = l.get(idx).cloned();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn lset(
        &mut self,
        key: &[u8],
        index: i64,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    if index < -len || index >= len {
                        return Err(StoreError::IndexOutOfRange);
                    }
                    let idx = normalize_index(index, len) as usize;
                    l[idx] = value;
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                    self.dirty = self.dirty.saturating_add(1);
                    Ok(())
                }
                _ => Err(StoreError::WrongType),
            },
            None => Err(StoreError::KeyNotFound),
        }
    }

    pub fn lpos(
        &mut self,
        key: &[u8],
        element: &[u8],
        now_ms: u64,
    ) -> Result<Option<usize>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let result = l.iter().position(|v| v.as_slice() == element);
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// LPOS with RANK, COUNT, and MAXLEN support.
    /// rank: 1-based rank of match (positive=head-to-tail, negative=tail-to-head). 0 is invalid.
    /// count: if Some(0) return all matches; if Some(n) return up to n; if None return first match only.
    /// maxlen: limit scan to first/last maxlen entries.
    pub fn lpos_full(
        &mut self,
        key: &[u8],
        element: &[u8],
        rank: i64,
        count: Option<u64>,
        maxlen: usize,
        now_ms: u64,
    ) -> Result<Vec<usize>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len();
                    let limit = if maxlen == 0 { len } else { maxlen.min(len) };
                    let max_results = match count {
                        Some(0) => usize::MAX,
                        Some(n) => n as usize,
                        None => 1,
                    };
                    let mut results = Vec::new();
                    let abs_rank = rank.unsigned_abs() as usize;
                    let skip = if abs_rank > 0 { abs_rank - 1 } else { 0 };
                    let mut matched = 0_usize;

                    if rank >= 0 {
                        // Forward scan
                        for (i, item) in l.iter().enumerate().take(limit) {
                            if item.as_slice() == element {
                                matched += 1;
                                if matched > skip {
                                    results.push(i);
                                    if results.len() >= max_results {
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        // Reverse scan
                        for (i, item) in l
                            .iter()
                            .enumerate()
                            .take(len)
                            .skip(len.saturating_sub(limit))
                            .rev()
                        {
                            if item.as_slice() == element {
                                matched += 1;
                                if matched > skip {
                                    results.push(i);
                                    if results.len() >= max_results {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    entry.touch(now_ms);
                    Ok(results)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn linsert_before(
        &mut self,
        key: &[u8],
        pivot: &[u8],
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    if let Some(pos) = l.iter().position(|v| v.as_slice() == pivot) {
                        l.insert(pos, value);
                        let len = l.len();
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(1);
                        Ok(len as i64)
                    } else {
                        Ok(-1)
                    }
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn linsert_after(
        &mut self,
        key: &[u8],
        pivot: &[u8],
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    if let Some(pos) = l.iter().position(|v| v.as_slice() == pivot) {
                        l.insert(pos + 1, value);
                        let len = l.len();
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(1);
                        Ok(len as i64)
                    } else {
                        Ok(-1)
                    }
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn lrem(
        &mut self,
        key: &[u8],
        count: i64,
        value: &[u8],
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let mut removed = 0_u64;
                    if count > 0 {
                        let limit = count as u64;
                        l.retain(|v| {
                            if removed < limit && v.as_slice() == value {
                                removed += 1;
                                false
                            } else {
                                true
                            }
                        });
                    } else if count < 0 {
                        let limit = count.unsigned_abs();
                        let total = l.iter().filter(|v| v.as_slice() == value).count() as u64;
                        let skip = total.saturating_sub(limit);
                        let mut seen = 0_u64;
                        l.retain(|v| {
                            if v.as_slice() == value {
                                seen += 1;
                                if seen > skip {
                                    removed += 1;
                                    return false;
                                }
                            }
                            true
                        });
                    } else {
                        let old_len = l.len();
                        l.retain(|v| v.as_slice() != value);
                        removed = (old_len - l.len()) as u64;
                    }
                    if removed > 0 {
                        self.dirty = self.dirty.saturating_add(removed);
                        if l.is_empty() {
                            self.internal_entries_remove(key);
                            self.stream_groups.remove(key);
                            self.stream_last_ids.remove(key);
                        } else {
                            Self::mark_digest_stale_fields(
                                &mut self.digest_stale,
                                &mut self.digest_mutations,
                            );
                            entry.touch_write(now_ms);
                        }
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn rpoplpush(
        &mut self,
        source: &[u8],
        destination: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(source, now_ms);
        self.drop_if_expired(destination, now_ms);

        match self.entries.get(source) {
            Some(entry) => {
                if !matches!(&entry.value, Value::List(_)) {
                    return Err(StoreError::WrongType);
                }
            }
            None => return Ok(None),
        }
        if source != destination
            && let Some(entry) = self.entries.get(destination)
            && !matches!(&entry.value, Value::List(_))
        {
            return Err(StoreError::WrongType);
        }

        // Pop from source
        let popped = match self.entries.get_mut(source) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let val = l.pop_back();
                    if val.is_some() && !l.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    val
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };
        let Some(val) = popped else {
            return Ok(None);
        };

        let mut source_ttl = None;
        if source == destination
            && let Some(entry) = self.entries.get(source)
        {
            source_ttl = entry.expires_at_ms;
        }

        // Clean up empty source.
        if let Some(entry) = self.entries.get(source)
            && let Value::List(l) = &entry.value
            && l.is_empty()
        {
            self.internal_entries_remove(source);
            self.stream_groups.remove(source);
            self.stream_last_ids.remove(source);
        }

        // Push to destination
        match self.entries.get_mut(destination) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    l.push_front(val.clone());
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                }
                _ => return Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                l.push_front(val.clone());
                self.internal_entries_insert(
                    destination.to_vec(),
                    Entry::new(Value::List(l), source_ttl, now_ms),
                );
            }
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(Some(val))
    }

    pub fn ltrim(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    let s = normalize_index(start, len).max(0);
                    let e = normalize_index(stop, len).min(len - 1);
                    let old_len = l.len();
                    if s > e || s >= len || e < 0 {
                        l.clear();
                    } else {
                        let s = s as usize;
                        let e = e as usize;
                        for _ in 0..s {
                            l.pop_front();
                        }
                        while l.len() > (e - s + 1) {
                            l.pop_back();
                        }
                    }
                    let removed = old_len - l.len();
                    if l.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if removed > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    if removed > 0 {
                        self.dirty = self.dirty.saturating_add(removed as u64);
                    }
                    Ok(())
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(()),
        }
    }

    pub fn lpushx(
        &mut self,
        key: &[u8],
        values: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    for v in values {
                        l.push_front(v.clone());
                    }
                    if !values.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                    }
                    Ok(l.len())
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn rpushx(
        &mut self,
        key: &[u8],
        values: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    for v in values {
                        l.push_back(v.clone());
                    }
                    if !values.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                    }
                    Ok(l.len())
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn lmove(
        &mut self,
        source: &[u8],
        destination: &[u8],
        wherefrom: &[u8],
        whereto: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(source, now_ms);
        self.drop_if_expired(destination, now_ms);

        match self.entries.get(source) {
            Some(entry) => {
                if !matches!(&entry.value, Value::List(_)) {
                    return Err(StoreError::WrongType);
                }
            }
            None => return Ok(None),
        }
        if source != destination
            && let Some(entry) = self.entries.get(destination)
            && !matches!(&entry.value, Value::List(_))
        {
            return Err(StoreError::WrongType);
        }

        // Pop from source.
        let popped = match self.entries.get_mut(source) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    let val = if eq_ascii_ci(wherefrom, b"LEFT") {
                        l.pop_front()
                    } else {
                        l.pop_back()
                    };
                    if val.is_some() && !l.is_empty() {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    val
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };
        let Some(val) = popped else {
            return Ok(None);
        };

        let mut source_ttl = None;
        if source == destination
            && let Some(entry) = self.entries.get(source)
        {
            source_ttl = entry.expires_at_ms;
        }

        // Clean up empty source.
        if let Some(entry) = self.entries.get(source)
            && let Value::List(l) = &entry.value
            && l.is_empty()
        {
            self.internal_entries_remove(source);
            self.stream_groups.remove(source);
            self.stream_last_ids.remove(source);
        }
        // Push to destination.
        match self.entries.get_mut(destination) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => {
                    if eq_ascii_ci(whereto, b"LEFT") {
                        l.push_front(val.clone());
                    } else {
                        l.push_back(val.clone());
                    }
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                }
                _ => return Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                if eq_ascii_ci(whereto, b"LEFT") {
                    l.push_front(val.clone());
                } else {
                    l.push_back(val.clone());
                }
                self.internal_entries_insert(
                    destination.to_vec(),
                    Entry::new(Value::List(l), source_ttl, now_ms),
                );
            }
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(Some(val))
    }

    // ── Set operations ──────────────────────────────────────────

    pub fn sadd(
        &mut self,
        key: &[u8],
        members: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Set(s) => {
                    let mut added = 0_u64;
                    for m in members {
                        if s.insert(m.clone()) {
                            added += 1;
                        }
                    }
                    if added > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                    }
                    entry.touch_write(now_ms);
                    self.dirty = self.dirty.saturating_add(added);
                    Ok(added)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut s = BTreeSet::new();
                let mut added = 0_u64;
                for m in members {
                    if s.insert(m.clone()) {
                        added += 1;
                    }
                }
                self.internal_entries_insert(key.to_vec(), Entry::new(Value::Set(s), None, now_ms));
                self.dirty = self.dirty.saturating_add(added);
                Ok(added)
            }
        }
    }

    pub fn srem(&mut self, key: &[u8], members: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Set(s) => {
                    let mut removed = 0_u64;
                    for m in members {
                        if s.remove(*m) {
                            removed += 1;
                        }
                    }
                    if s.is_empty() {
                        self.internal_entries_remove(key);
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    } else if removed > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                    }
                    self.dirty = self.dirty.saturating_add(removed);
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn smembers(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let members: Vec<Vec<u8>> = s.iter().cloned().collect();
                    entry.touch(now_ms);
                    Ok(members)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn scard(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let len = s.len();
                    entry.touch(now_ms);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn sismember(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(false);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let result = s.contains(member);
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    pub fn sinter(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        for key in keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut min_card = usize::MAX;
        let mut min_idx = 0;
        let mut has_empty = false;

        // First pass: typecheck and find the smallest set.
        for (i, key) in keys.iter().enumerate() {
            match self.entries.get(*key) {
                Some(entry) => match &entry.value {
                    Value::Set(s) => {
                        if s.len() < min_card {
                            min_card = s.len();
                            min_idx = i;
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    has_empty = true;
                }
            }
        }

        if has_empty {
            // Touch existing sets to emulate Redis behavior
            for key in keys {
                if let Some(entry) = self.entries.get_mut(*key)
                    && let Value::Set(_) = &entry.value
                {
                    entry.touch(now_ms);
                }
            }
            return Ok(Vec::new());
        }

        let mut result = match self.entries.get_mut(keys[min_idx]) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let res = s.clone();
                    entry.touch(now_ms);
                    res
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(Vec::new()),
        };

        for (i, key) in keys.iter().enumerate() {
            if i == min_idx {
                continue;
            }
            if result.is_empty() {
                if let Some(entry) = self.entries.get_mut(*key)
                    && let Value::Set(_) = &entry.value
                {
                    entry.touch(now_ms);
                }
                continue;
            }
            match self.entries.get_mut(*key) {
                Some(entry) => {
                    if let Value::Set(s) = &entry.value {
                        result.retain(|m| s.contains(m));
                        entry.touch(now_ms);
                    }
                }
                None => {
                    result.clear();
                }
            }
        }
        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn sintercard(
        &mut self,
        keys: &[&[u8]],
        limit: u64,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        if keys.is_empty() {
            return Ok(0);
        }
        for key in keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut min_card = usize::MAX;
        let mut min_idx = 0;
        let mut has_empty = false;

        for (i, key) in keys.iter().enumerate() {
            match self.entries.get(*key) {
                Some(entry) => match &entry.value {
                    Value::Set(s) => {
                        if s.len() < min_card {
                            min_card = s.len();
                            min_idx = i;
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    has_empty = true;
                }
            }
        }

        if has_empty {
            for key in keys {
                if let Some(entry) = self.entries.get_mut(*key)
                    && let Value::Set(_) = &entry.value
                {
                    entry.touch(now_ms);
                }
            }
            return Ok(0);
        }

        let mut result = match self.entries.get_mut(keys[min_idx]) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let res = s.clone();
                    entry.touch(now_ms);
                    res
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(0),
        };

        for (i, key) in keys.iter().enumerate() {
            if i == min_idx {
                continue;
            }
            if result.is_empty() {
                if let Some(entry) = self.entries.get_mut(*key)
                    && let Value::Set(_) = &entry.value
                {
                    entry.touch(now_ms);
                }
                continue;
            }
            match self.entries.get_mut(*key) {
                Some(entry) => match &entry.value {
                    Value::Set(s) => {
                        result.retain(|m| s.contains(m));
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    result.clear();
                }
            }
        }
        let count = u64::try_from(result.len()).unwrap_or(u64::MAX);
        if limit > 0 && count > limit {
            Ok(limit)
        } else {
            Ok(count)
        }
    }

    pub fn sunion(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        for key in keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut max_card = 0;
        let mut base_idx = None;
        for (i, key) in keys.iter().enumerate() {
            if let Some(entry) = self.entries.get_mut(*key) {
                match &entry.value {
                    Value::Set(s) => {
                        let len = s.len();
                        if len > max_card {
                            max_card = len;
                            base_idx = Some(i);
                        }
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }

        let Some(base_idx) = base_idx else {
            return Ok(Vec::new());
        };

        let mut result = match self.entries.get(keys[base_idx]) {
            Some(entry) => match &entry.value {
                Value::Set(s) => s.clone(),
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(Vec::new()),
        };

        for (i, key) in keys.iter().enumerate() {
            if i == base_idx {
                continue;
            }
            if let Some(entry) = self.entries.get(*key)
                && let Value::Set(s) = &entry.value
            {
                result.extend(s.iter().cloned());
            }
        }

        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn sdiff(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        for key in keys {
            self.drop_if_expired(key, now_ms);
        }
        let mut result = match self.entries.get_mut(keys[0]) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let res = s.clone();
                    entry.touch(now_ms);
                    res
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(Vec::new()),
        };
        for key in &keys[1..] {
            if result.is_empty() {
                if let Some(entry) = self.entries.get_mut(*key) {
                    if let Value::Set(_) = &entry.value {
                        entry.touch(now_ms);
                    } else {
                        return Err(StoreError::WrongType);
                    }
                }
                continue;
            }
            if let Some(entry) = self.entries.get_mut(*key) {
                match &entry.value {
                    Value::Set(s) => {
                        result.retain(|m| !s.contains(m));
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }
        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn spop(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        let rand_val = self.next_rand();
        let mut should_remove_key = false;
        let member = match self.entries.get_mut(key) {
            Some(entry) => {
                let result = match &mut entry.value {
                    Value::Set(s) => {
                        if s.is_empty() {
                            return Ok(None);
                        }
                        let idx = (rand_val as usize) % s.len();
                        let member = s.iter().nth(idx).cloned();
                        if let Some(ref m) = member {
                            s.remove(m);
                        }
                        if s.is_empty() {
                            should_remove_key = true;
                        }
                        Ok(member)
                    }
                    _ => Err(StoreError::WrongType),
                };
                if matches!(result, Ok(Some(_))) && !should_remove_key {
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                }
                if result.is_ok() {
                    entry.touch_write(now_ms);
                }
                result
            }
            None => Ok(None),
        }?;
        if should_remove_key {
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
        }
        Ok(member)
    }

    /// SPOP key count — pop up to `count` members from a set.
    pub fn spop_count(
        &mut self,
        key: &[u8],
        count: usize,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut result = Vec::new();
        for _ in 0..count {
            match self.spop(key, now_ms)? {
                Some(m) => result.push(m),
                None => break,
            }
        }
        Ok(result)
    }

    pub fn srandmember(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        let rand_val = self.next_rand();
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    if s.is_empty() {
                        return Ok(None);
                    }
                    let idx = (rand_val as usize) % s.len();
                    let member = s.iter().nth(idx).cloned();
                    entry.touch(now_ms);
                    Ok(member)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// SRANDMEMBER key count — returns multiple random members.
    /// Positive count: up to `count` distinct members.
    /// Negative count: exactly `|count|` members, possibly with repeats.
    pub fn srandmember_count(
        &mut self,
        key: &[u8],
        count: i64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut result_data = None;
        if let Some(entry) = self.entries.get_mut(key) {
            match &entry.value {
                Value::Set(s) => {
                    if !s.is_empty() {
                        let members: Vec<Vec<u8>> = s.iter().cloned().collect();
                        entry.touch(now_ms);
                        result_data = Some(members);
                    }
                }
                _ => return Err(StoreError::WrongType),
            }
        }

        let members = match result_data {
            Some(m) if !m.is_empty() => m,
            _ => return Ok(Vec::new()),
        };

        if count >= 0 {
            let n = (count as usize).min(members.len());
            // Use a more memory-efficient approach for small n
            if n < members.len() / 2 && n < 1024 {
                let mut results = Vec::with_capacity(n);
                let mut picked = HashSet::with_capacity(n);
                while results.len() < n {
                    let idx = (self.next_rand() as usize) % members.len();
                    if picked.insert(idx) {
                        results.push(members[idx].clone());
                    }
                }
                Ok(results)
            } else {
                let mut indices: Vec<usize> = (0..members.len()).collect();
                for i in 0..n {
                    let j = i + (self.next_rand() as usize % (members.len() - i));
                    indices.swap(i, j);
                }
                Ok(indices[..n]
                    .iter()
                    .map(|&idx| members[idx].clone())
                    .collect())
            }
        } else {
            let abs_count = count.unsigned_abs() as usize;
            // Cap initial allocation to avoid DoS, but allow growth.
            let mut result = Vec::with_capacity(abs_count.min(1024));
            for _ in 0..abs_count {
                let idx = (self.next_rand() as usize) % members.len();
                result.push(members[idx].clone());
            }
            Ok(result)
        }
    }

    pub fn smove(
        &mut self,
        source: &[u8],
        destination: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(source, now_ms);
        self.drop_if_expired(destination, now_ms);

        match self.entries.get(source) {
            Some(entry) => {
                if !matches!(&entry.value, Value::Set(_)) {
                    return Err(StoreError::WrongType);
                }
            }
            None => return Ok(false),
        }
        if source != destination
            && let Some(entry) = self.entries.get(destination)
            && !matches!(&entry.value, Value::Set(_))
        {
            return Err(StoreError::WrongType);
        }
        if source == destination {
            let present = match self.entries.get(source) {
                Some(entry) => match &entry.value {
                    Value::Set(set) => set.contains(member),
                    _ => return Err(StoreError::WrongType),
                },
                None => false,
            };
            return Ok(present);
        }

        // Remove from source
        let mut source_empty = false;
        let was_removed = if let Some(entry) = self.entries.get_mut(source) {
            let r = match &mut entry.value {
                Value::Set(s) => {
                    let r = s.remove(member);
                    if r {
                        source_empty = s.is_empty();
                    }
                    r
                }
                _ => return Err(StoreError::WrongType),
            };
            if r {
                if !source_empty {
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                }
                entry.touch_write(now_ms);
                self.dirty = self.dirty.saturating_add(1);
            }
            r
        } else {
            return Ok(false);
        };

        if !was_removed {
            return Ok(false);
        }

        // Clean up empty source
        if source_empty {
            self.internal_entries_remove(source);
            self.stream_groups.remove(source);
            self.stream_last_ids.remove(source);
        }
        // Add to destination
        self.sadd(destination, &[member.to_vec()], now_ms)?;
        Ok(true)
    }

    pub fn sinterstore(
        &mut self,
        destination: &[u8],
        keys: &[&[u8]],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let result = self.sinter(keys, now_ms)?;
        let count = result.len();
        let deleted = self.internal_entries_remove(destination).is_some();
        self.stream_groups.remove(destination);
        self.stream_last_ids.remove(destination);
        if !result.is_empty() {
            let set: BTreeSet<Vec<u8>> = result.into_iter().collect();
            self.internal_entries_insert(
                destination.to_vec(),
                Entry::new(Value::Set(set), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
        } else if deleted {
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(count)
    }

    pub fn sunionstore(
        &mut self,
        destination: &[u8],
        keys: &[&[u8]],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let result = self.sunion(keys, now_ms)?;
        let count = result.len();
        let deleted = self.internal_entries_remove(destination).is_some();
        self.stream_groups.remove(destination);
        self.stream_last_ids.remove(destination);
        if !result.is_empty() {
            let set: BTreeSet<Vec<u8>> = result.into_iter().collect();
            self.internal_entries_insert(
                destination.to_vec(),
                Entry::new(Value::Set(set), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
        } else if deleted {
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(count)
    }

    pub fn sdiffstore(
        &mut self,
        destination: &[u8],
        keys: &[&[u8]],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let result = self.sdiff(keys, now_ms)?;
        let count = result.len();
        let deleted = self.internal_entries_remove(destination).is_some();
        self.stream_groups.remove(destination);
        self.stream_last_ids.remove(destination);
        if !result.is_empty() {
            let set: BTreeSet<Vec<u8>> = result.into_iter().collect();
            self.internal_entries_insert(
                destination.to_vec(),
                Entry::new(Value::Set(set), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
        } else if deleted {
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(count)
    }

    // ── Sorted Set (ZSet) operations ─────────────────────────────

    /// Add members with scores. Returns the number of *new* members added.
    pub fn zadd(
        &mut self,
        key: &[u8],
        members: &[(f64, Vec<u8>)],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.zadd_with_options(key, members, ZaddOptions::default(), now_ms)
            .map(|(added, _changed)| added)
    }

    /// ZADD with NX/XX/GT/LT/CH options.
    /// Returns (count, changed) where:
    /// - Without CH: count = number of new elements added
    /// - With CH: count = number of new elements added + updated elements
    /// - changed = number of existing elements whose score was updated
    pub fn zadd_with_options(
        &mut self,
        key: &[u8],
        members: &[(f64, Vec<u8>)],
        opts: ZaddOptions,
        now_ms: u64,
    ) -> Result<(usize, usize), StoreError> {
        self.drop_if_expired(key, now_ms);

        // ZADD XX on a missing key should not create an empty sorted set
        if opts.xx && !self.entries.contains_key(key) {
            return Ok((0, 0));
        }

        let (added, changed, is_empty, touched) = {
            let entry =
                self.internal_entry(key.to_vec(), Value::SortedSet(SortedSet::new()), now_ms);
            let Value::SortedSet(zs) = &mut entry.value else {
                return Err(StoreError::WrongType);
            };
            let mut added = 0_usize;
            let mut changed = 0_usize;

            let mut deduplicated = Vec::with_capacity(members.len());
            let mut seen = std::collections::HashSet::new();
            for (score, member) in members.iter().rev() {
                if seen.insert(member.as_slice()) {
                    deduplicated.push((score, member));
                }
            }
            deduplicated.reverse();

            for (score, member) in deduplicated {
                match zs.get_score(member) {
                    Some(old_score) => {
                        // Existing member
                        if opts.nx {
                            continue; // NX: don't update existing
                        }
                        let should_update = if opts.gt {
                            *score > old_score
                        } else if opts.lt {
                            *score < old_score
                        } else {
                            true
                        };
                        if should_update {
                            let old_canonical = canonicalize_zero_score(old_score);
                            let new_canonical = canonicalize_zero_score(*score);
                            let score_changed = !old_canonical.total_cmp(&new_canonical).is_eq();
                            zs.insert(member.clone(), *score);
                            if score_changed {
                                changed += 1;
                            }
                        }
                    }
                    None => {
                        // New member
                        if opts.xx {
                            continue; // XX: don't add new
                        }
                        zs.insert(member.clone(), *score);
                        added += 1;
                    }
                }
            }
            let is_empty = zs.is_empty();
            let touched = added > 0 || changed > 0;
            if touched {
                entry.touch_write(now_ms);
            }
            (added, changed, is_empty, touched)
        };
        if touched {
            Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
            self.dirty = self.dirty.saturating_add((added + changed) as u64);
        }
        if is_empty {
            self.internal_entries_remove(key);
        }
        if opts.ch {
            Ok((added + changed, changed))
        } else {
            Ok((added, changed))
        }
    }

    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(0);
        };
        let (removed, is_empty) = {
            let Value::SortedSet(zs) = &mut entry.value else {
                return Err(StoreError::WrongType);
            };
            let mut removed = 0_u64;
            for member in members {
                if zs.remove(member) {
                    removed += 1;
                }
            }
            (removed, zs.is_empty())
        };

        if removed > 0 {
            if !is_empty {
                Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
                entry.touch_write(now_ms);
            }
            self.dirty = self.dirty.saturating_add(removed);
        }
        if is_empty {
            self.internal_entries_remove(key);
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
        }
        Ok(removed)
    }

    pub fn zget_score_or_set_member(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<Option<f64>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result = zs.get_score(member);
                    entry.touch(now_ms);
                    Ok(result)
                }
                Value::Set(s) => {
                    let result = if s.contains(member) { Some(1.0) } else { None };
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn zget_members_with_scores(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result = zs.iter_asc().map(|(m, s)| (m.clone(), *s)).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                Value::Set(s) => {
                    let mut members: Vec<_> = s.iter().cloned().collect();
                    members.sort();
                    let result = members.into_iter().map(|m| (m, 1.0)).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Get the score of a member. Returns None if member or key doesn't exist.
    pub fn zscore(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<Option<f64>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result = zs.get_score(member);
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return cardinality of sorted set.
    pub fn zcard(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len();
                    entry.touch(now_ms);
                    Ok(len)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    /// Return rank (0-based index) of member when sorted ascending by score.
    pub fn zrank(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<Option<usize>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let Some(score) = zs.get_score(member) else {
                        return Ok(None);
                    };
                    let rank = zs
                        .iter_asc()
                        .take_while(|&(m, s)| score_member_lt(*s, m, score, member))
                        .count();
                    entry.touch(now_ms);
                    Ok(Some(rank))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return reverse rank (0-based index) of member when sorted descending.
    pub fn zrevrank(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<Option<usize>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let Some(score) = zs.get_score(member) else {
                        return Ok(None);
                    };
                    let rank = zs
                        .iter_desc()
                        .take_while(|&(m, s)| score_member_lt(score, member, *s, m))
                        .count();
                    entry.touch(now_ms);
                    Ok(Some(rank))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return elements sorted ascending by score, by index range.
    pub fn zrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len || e < 0 {
                        return Ok(Vec::new());
                    }
                    let s_idx = s.max(0) as usize;
                    let e_idx = e.min(len - 1) as usize;
                    let count = e_idx - s_idx + 1;
                    let result: Vec<Vec<u8>> = zs
                        .iter_asc()
                        .skip(s_idx)
                        .take(count)
                        .map(|(m, _)| m.clone())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Return elements sorted descending by score, by index range.
    pub fn zrevrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len || e < 0 {
                        return Ok(Vec::new());
                    }
                    let s_idx = s.max(0) as usize;
                    let e_idx = e.min(len - 1) as usize;
                    let count = e_idx - s_idx + 1;
                    let result: Vec<Vec<u8>> = zs
                        .iter_desc()
                        .skip(s_idx)
                        .take(count)
                        .map(|(m, _)| m.clone())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Return members with scores within [min, max] range, sorted ascending.
    pub fn zrangebyscore(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        // (br-frankenredis-ot1z)
        if score_bound_value(min) > score_bound_value(max) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let lower = match min {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::min_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::max_for_score(s)),
                    };
                    let upper = match max {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::max_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::min_for_score(s)),
                    };

                    let result: Vec<Vec<u8>> = zs
                        .ordered
                        .range((lower, upper))
                        .filter_map(|(sm, _)| sm.member.as_actual().cloned())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Return members with scores within the given bounds, as (member, score) pairs.
    pub fn zrangebyscore_withscores(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        // Upstream behavior for an inverted score range (min > max)
        // is to return an empty range; BTreeMap::range panics if
        // start > end, so guard early. (br-frankenredis-ot1z)
        if score_bound_value(min) > score_bound_value(max) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let lower = match min {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::min_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::max_for_score(s)),
                    };
                    let upper = match max {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::max_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::min_for_score(s)),
                    };

                    let result: Vec<(Vec<u8>, f64)> = zs
                        .ordered
                        .range((lower, upper))
                        .filter_map(|(sm, _)| {
                            sm.member
                                .as_actual()
                                .map(|member| (member.clone(), sm.score))
                        })
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Create or overwrite a sorted set from member-score pairs.
    pub fn zstore_from_pairs(&mut self, key: Vec<u8>, pairs: Vec<(Vec<u8>, f64)>, now_ms: u64) {
        let mut zs = SortedSet::new();
        for (member, score) in pairs {
            zs.insert(member, score);
        }
        self.stream_groups.remove(key.as_slice());
        self.stream_last_ids.remove(key.as_slice());
        self.internal_entries_insert(key, Entry::new(Value::SortedSet(zs), None, now_ms));
    }

    /// Count members with scores within the given bounds.
    pub fn zcount(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let lower = match min {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::min_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::max_for_score(s)),
                    };
                    let upper = match max {
                        ScoreBound::Inclusive(s) => Included(ScoreMember::max_for_score(s)),
                        ScoreBound::Exclusive(s) => Excluded(ScoreMember::min_for_score(s)),
                    };
                    let result = zs
                        .ordered
                        .range((lower, upper))
                        .filter(|(sm, _)| sm.member.as_actual().is_some())
                        .count();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    /// Increment score of member by delta. Creates member with delta as score if absent.
    pub fn zincrby(
        &mut self,
        key: &[u8],
        member: Vec<u8>,
        delta: f64,
        now_ms: u64,
    ) -> Result<f64, StoreError> {
        self.zincrby_with_options(key, member, delta, ZaddOptions::default(), now_ms)
            .map(|opt| opt.unwrap_or(0.0)) // ZINCRBY without options always succeeds
    }

    /// Increment score of member by delta, respecting ZADD options (NX, XX, GT, LT).
    /// Returns None if the operation was aborted due to options.
    pub fn zincrby_with_options(
        &mut self,
        key: &[u8],
        member: Vec<u8>,
        delta: f64,
        opts: ZaddOptions,
        now_ms: u64,
    ) -> Result<Option<f64>, StoreError> {
        self.drop_if_expired(key, now_ms);

        if opts.xx && !self.entries.contains_key(key) {
            return Ok(None);
        }

        let (res, is_empty, touched) = {
            let entry =
                self.internal_entry(key.to_vec(), Value::SortedSet(SortedSet::new()), now_ms);
            let Value::SortedSet(zs) = &mut entry.value else {
                return Err(StoreError::WrongType);
            };

            let old_score = zs.get_score(&member);

            let res = if (opts.nx && old_score.is_some()) || (opts.xx && old_score.is_none()) {
                Ok(None)
            } else {
                let new_score = old_score.unwrap_or(0.0) + delta;

                if new_score.is_nan() {
                    Err(StoreError::IncrFloatNaN)
                } else if let Some(old) = old_score {
                    if (opts.gt && new_score <= old) || (opts.lt && new_score >= old) {
                        Ok(None)
                    } else {
                        zs.insert(member, new_score);
                        Ok(Some(new_score))
                    }
                } else {
                    zs.insert(member, new_score);
                    Ok(Some(new_score))
                }
            };
            let is_empty = zs.is_empty();
            let touched = matches!(&res, Ok(Some(_)));
            if touched {
                entry.touch_write(now_ms);
            }
            (res, is_empty, touched)
        };

        if touched {
            Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
            self.dirty = self.dirty.saturating_add(1);
        }
        if is_empty {
            self.internal_entries_remove(key);
        }
        res
    }

    /// Remove and return the member with the lowest score.
    pub fn zpopmin(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(None);
        };
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let result = zs.pop_min();
        let is_empty = zs.is_empty();
        if result.is_some() {
            self.dirty = self.dirty.saturating_add(1);
            if is_empty {
                self.internal_entries_remove(key);
                self.stream_groups.remove(key);
                self.stream_last_ids.remove(key);
            } else {
                Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
                entry.touch_write(now_ms);
            }
        }
        Ok(result)
    }

    /// Remove and return the member with the highest score.
    pub fn zpopmax(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(None);
        };
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let result = zs.pop_max();
        let is_empty = zs.is_empty();
        if result.is_some() {
            self.dirty = self.dirty.saturating_add(1);
            if is_empty {
                self.internal_entries_remove(key);
                self.stream_groups.remove(key);
                self.stream_last_ids.remove(key);
            } else {
                Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
                entry.touch_write(now_ms);
            }
        }
        Ok(result)
    }

    /// Remove and return up to `count` members with the lowest scores.
    pub fn zpopmin_count(
        &mut self,
        key: &[u8],
        count: usize,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(Vec::new());
        };
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let mut result = Vec::new();
        for _ in 0..count {
            match zs.pop_min() {
                Some(pair) => result.push(pair),
                None => break,
            }
        }
        let is_empty = zs.is_empty();
        if !result.is_empty() {
            if is_empty {
                self.internal_entries_remove(key);
                self.stream_groups.remove(key);
                self.stream_last_ids.remove(key);
            } else {
                Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
                entry.touch_write(now_ms);
            }
        }
        Ok(result)
    }

    /// Remove and return up to `count` members with the highest scores.
    pub fn zpopmax_count(
        &mut self,
        key: &[u8],
        count: usize,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(Vec::new());
        };
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let mut result = Vec::new();
        for _ in 0..count {
            match zs.pop_max() {
                Some(pair) => result.push(pair),
                None => break,
            }
        }
        let is_empty = zs.is_empty();
        if !result.is_empty() {
            if is_empty {
                self.internal_entries_remove(key);
                self.stream_groups.remove(key);
                self.stream_last_ids.remove(key);
            } else {
                Self::mark_digest_stale_fields(&mut self.digest_stale, &mut self.digest_mutations);
                entry.touch_write(now_ms);
            }
        }
        Ok(result)
    }

    /// Return range with scores (ascending order by score).
    pub fn zrange_withscores(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len || e < 0 {
                        return Ok(Vec::new());
                    }
                    let s_idx = s.max(0) as usize;
                    let e_idx = e.min(len - 1) as usize;
                    let count = e_idx - s_idx + 1;
                    let result: Vec<(Vec<u8>, f64)> = zs
                        .iter_asc()
                        .skip(s_idx)
                        .take(count)
                        .map(|(m, &s)| (m.clone(), s))
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zrevrange_withscores(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len || e < 0 {
                        return Ok(Vec::new());
                    }
                    let s_idx = s.max(0) as usize;
                    let e_idx = e.min(len - 1) as usize;
                    let count = e_idx - s_idx + 1;
                    let result: Vec<(Vec<u8>, f64)> = zs
                        .iter_desc()
                        .skip(s_idx)
                        .take(count)
                        .map(|(m, &s)| (m.clone(), s))
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zrevrangebyscore(
        &mut self,
        key: &[u8],
        max: f64,
        min: f64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let lower = Included(ScoreMember::min_for_score(min));
                    let upper = Included(ScoreMember::max_for_score(max));

                    let result: Vec<Vec<u8>> = zs
                        .ordered
                        .range((lower, upper))
                        .rev()
                        .filter_map(|(sm, _)| sm.member.as_actual().cloned())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zrangebylex(
        &mut self,
        key: &[u8],
        min: &[u8],
        max: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result: Vec<Vec<u8>> = zs
                        .iter_asc()
                        .filter(|(m, _)| lex_in_range(m, min, max))
                        .map(|(m, _)| m.clone())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zrevrangebyscore_withscores(
        &mut self,
        key: &[u8],
        max: f64,
        min: f64,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let lower = Included(ScoreMember::min_for_score(min));
                    let upper = Included(ScoreMember::max_for_score(max));

                    let result: Vec<(Vec<u8>, f64)> = zs
                        .ordered
                        .range((lower, upper))
                        .rev()
                        .filter_map(|(sm, _)| {
                            sm.member
                                .as_actual()
                                .map(|member| (member.clone(), sm.score))
                        })
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zrevrangebylex(
        &mut self,
        key: &[u8],
        max: &[u8],
        min: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result: Vec<Vec<u8>> = zs
                        .iter_desc()
                        .filter(|(m, _)| lex_in_range(m, min, max))
                        .map(|(m, _)| m.clone())
                        .collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn zlexcount(
        &mut self,
        key: &[u8],
        min: &[u8],
        max: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result = zs
                        .iter_asc()
                        .filter(|(m, _)| lex_in_range(m, min, max))
                        .count();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zremrangebyrank(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => {
                    let len = zs.len();
                    let s = normalize_index(start, len as i64);
                    let e = normalize_index(stop, len as i64);
                    if s > e || s >= len as i64 || e < 0 {
                        return Ok(0);
                    }
                    let s_idx = s.max(0) as usize;
                    let e_idx = e.min(len as i64 - 1) as usize;
                    let count = e_idx - s_idx + 1;
                    let to_remove: Vec<Vec<u8>> = zs
                        .iter_asc()
                        .skip(s_idx)
                        .take(count)
                        .map(|(m, _)| m.clone())
                        .collect();
                    let removed_count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    let is_empty = zs.is_empty();
                    if removed_count > 0 {
                        self.dirty = self.dirty.saturating_add(removed_count as u64);
                        if is_empty {
                            self.internal_entries_remove(key);
                            self.stream_groups.remove(key);
                            self.stream_last_ids.remove(key);
                        } else {
                            Self::mark_digest_stale_fields(
                                &mut self.digest_stale,
                                &mut self.digest_mutations,
                            );
                            entry.touch_write(now_ms);
                        }
                    }
                    Ok(removed_count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zremrangebyscore(
        &mut self,
        key: &[u8],
        min: ScoreBound,
        max: ScoreBound,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => {
                    let lower = match min {
                        ScoreBound::Inclusive(s) => {
                            std::ops::Bound::Included(ScoreMember::min_for_score(s))
                        }
                        ScoreBound::Exclusive(s) => {
                            std::ops::Bound::Excluded(ScoreMember::max_for_score(s))
                        }
                    };
                    let upper = match max {
                        ScoreBound::Inclusive(s) => {
                            std::ops::Bound::Included(ScoreMember::max_for_score(s))
                        }
                        ScoreBound::Exclusive(s) => {
                            std::ops::Bound::Excluded(ScoreMember::min_for_score(s))
                        }
                    };
                    let to_remove: Vec<Vec<u8>> = zs
                        .ordered
                        .range((lower, upper))
                        .filter_map(|(sm, _)| sm.member.as_actual().cloned())
                        .collect();
                    let removed_count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    let is_empty = zs.is_empty();
                    if removed_count > 0 {
                        self.dirty = self.dirty.saturating_add(removed_count as u64);
                        if is_empty {
                            self.internal_entries_remove(key);
                            self.stream_groups.remove(key);
                            self.stream_last_ids.remove(key);
                        } else {
                            Self::mark_digest_stale_fields(
                                &mut self.digest_stale,
                                &mut self.digest_mutations,
                            );
                            entry.touch_write(now_ms);
                        }
                    }
                    Ok(removed_count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zremrangebylex(
        &mut self,
        key: &[u8],
        min: &[u8],
        max: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => {
                    let to_remove: Vec<Vec<u8>> = zs
                        .iter_asc()
                        .filter(|(m, _)| lex_in_range(m, min, max))
                        .map(|(m, _)| m.clone())
                        .collect();
                    let removed_count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    let is_empty = zs.is_empty();
                    if removed_count > 0 {
                        self.dirty = self.dirty.saturating_add(removed_count as u64);
                        if is_empty {
                            self.internal_entries_remove(key);
                            self.stream_groups.remove(key);
                            self.stream_last_ids.remove(key);
                        } else {
                            Self::mark_digest_stale_fields(
                                &mut self.digest_stale,
                                &mut self.digest_mutations,
                            );
                            entry.touch_write(now_ms);
                        }
                    }
                    Ok(removed_count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zrandmember(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let rand_val = self.next_rand();
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    if zs.is_empty() {
                        return Ok(None);
                    }
                    let idx = (rand_val as usize) % zs.len();
                    let member = zs.iter_asc().nth(idx).map(|(m, _)| m.clone());
                    entry.touch(now_ms);
                    Ok(member)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return `count` random members from a sorted set.
    /// Positive count: up to `count` distinct members.
    /// Negative count: `|count|` fields with possible repeats.
    pub fn zrandmember_count(
        &mut self,
        key: &[u8],
        count: i64,
        now_ms: u64,
    ) -> Result<Vec<(Vec<u8>, f64)>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut result_data = None;
        if let Some(entry) = self.entries.get_mut(key) {
            match &entry.value {
                Value::SortedSet(zs) => {
                    if !zs.is_empty() {
                        let members: Vec<(Vec<u8>, f64)> =
                            zs.iter_asc().map(|(m, s)| (m.clone(), *s)).collect();
                        entry.touch(now_ms);
                        result_data = Some(members);
                    }
                }
                _ => return Err(StoreError::WrongType),
            }
        }

        let members = match result_data {
            Some(m) if !m.is_empty() => m,
            _ => return Ok(Vec::new()),
        };

        if count >= 0 {
            let n = (count as usize).min(members.len());
            // Use a more memory-efficient approach for small n
            if n < members.len() / 2 && n < 1024 {
                let mut results = Vec::with_capacity(n);
                let mut picked = HashSet::with_capacity(n);
                while results.len() < n {
                    let idx = (self.next_rand() as usize) % members.len();
                    if picked.insert(idx) {
                        results.push(members[idx].clone());
                    }
                }
                Ok(results)
            } else {
                let mut indices: Vec<usize> = (0..members.len()).collect();
                for i in 0..n {
                    let j = i + (self.next_rand() as usize % (members.len() - i));
                    indices.swap(i, j);
                }
                Ok(indices[..n]
                    .iter()
                    .map(|&idx| members[idx].clone())
                    .collect())
            }
        } else {
            let abs_count = count.unsigned_abs() as usize;
            // Cap initial allocation to avoid DoS, but allow growth.
            let mut result = Vec::with_capacity(abs_count.min(1024));
            for _ in 0..abs_count {
                let idx = (self.next_rand() as usize) % members.len();
                result.push(members[idx].clone());
            }
            Ok(result)
        }
    }

    pub fn zmscore(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
        now_ms: u64,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let result: Vec<Option<f64>> =
                        members.iter().map(|m| zs.get_score(m)).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(members.iter().map(|_| None).collect()),
        }
    }

    pub fn xlast_id_with_existence(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<(bool, Option<StreamId>), StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok((false, None));
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let btree_last = entries.last_key_value().map(|(id, _)| *id);
                    let xsetid_last = self.stream_last_ids.get(key).copied();
                    let last_id = match (btree_last, xsetid_last) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                    entry.touch(now_ms);
                    Ok((true, last_id))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok((false, None)),
        }
    }

    pub fn xlast_id(&mut self, key: &[u8], now_ms: u64) -> Result<Option<StreamId>, StoreError> {
        let (_, last_id) = self.xlast_id_with_existence(key, now_ms)?;
        Ok(last_id)
    }

    pub fn stream_watermark(&self, key: &[u8]) -> Result<Option<StreamId>, StoreError> {
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let btree_last = entries.last_key_value().map(|(id, _)| *id);
                    let xsetid_last = self.stream_last_ids.get(key).copied();
                    Ok(match (btree_last, xsetid_last) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    })
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return consumer group state for a stream key (for RDB persistence).
    #[must_use]
    pub fn stream_consumer_groups(&self, key: &[u8]) -> Option<&StreamGroupState> {
        self.stream_groups.get(key)
    }

    /// Restore a consumer group from RDB snapshot data (bypasses normal validation).
    pub fn restore_stream_group(
        &mut self,
        key: &[u8],
        group_name: Vec<u8>,
        last_delivered_id: StreamId,
        consumers: BTreeSet<Vec<u8>>,
        pending: StreamPendingEntries,
    ) {
        let groups = self.stream_groups.entry(key.to_vec()).or_default();
        groups.insert(
            group_name,
            StreamGroup {
                last_delivered_id,
                consumers,
                pending,
            },
        );
    }

    pub fn xadd(
        &mut self,
        key: &[u8],
        id: StreamId,
        fields: &[StreamField],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    entries.insert(id, fields.to_vec());
                    Self::mark_digest_stale_fields(
                        &mut self.digest_stale,
                        &mut self.digest_mutations,
                    );
                    entry.touch_write(now_ms);
                    // Track high watermark so IDs stay monotonic after XDEL
                    let wm = self.stream_last_ids.entry(key.to_vec()).or_insert((0, 0));
                    if id > *wm {
                        *wm = id;
                    }
                    self.dirty = self.dirty.saturating_add(1);
                    Ok(())
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut entries = BTreeMap::new();
                entries.insert(id, fields.to_vec());
                self.stream_groups.remove(key);
                self.stream_max_deleted_ids.remove(key);
                // Set high watermark to the first entry's ID
                self.stream_last_ids.insert(key.to_vec(), id);
                self.internal_entries_insert(
                    key.to_vec(),
                    Entry::new(Value::Stream(entries), None, now_ms),
                );
                self.dirty = self.dirty.saturating_add(1);
                Ok(())
            }
        }
    }

    pub fn xlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(0);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let result = entries.len();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn xrange(
        &mut self,
        key: &[u8],
        start: StreamId,
        end: StreamId,
        count: Option<usize>,
        now_ms: u64,
    ) -> Result<Vec<StreamRecord>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    if start > end {
                        return Ok(Vec::new());
                    }
                    let mut out = Vec::new();
                    for (id, fields) in entries.range(start..=end) {
                        out.push((*id, fields.clone()));
                        if let Some(limit) = count
                            && out.len() >= limit
                        {
                            break;
                        }
                    }
                    entry.touch(now_ms);
                    Ok(out)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn xrevrange(
        &mut self,
        key: &[u8],
        end: StreamId,
        start: StreamId,
        count: Option<usize>,
        now_ms: u64,
    ) -> Result<Vec<StreamRecord>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    if start > end {
                        return Ok(Vec::new());
                    }
                    let mut out = Vec::new();
                    for (id, fields) in entries.range(start..=end).rev() {
                        out.push((*id, fields.clone()));
                        if let Some(limit) = count
                            && out.len() >= limit
                        {
                            break;
                        }
                    }
                    entry.touch(now_ms);
                    Ok(out)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn xdel(&mut self, key: &[u8], ids: &[StreamId], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut max_deleted = None;
        let result = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    let mut removed = 0usize;
                    for id in ids {
                        if entries.remove(id).is_some() {
                            removed = removed.saturating_add(1);
                            max_deleted =
                                Some(max_deleted.map_or(*id, |current: StreamId| current.max(*id)));
                        }
                    }
                    // Upstream Redis XDEL deliberately LEAVES the
                    // entry in any consumer-group PEL — orphan IDs
                    // are surfaced (and cleaned up) by XAUTOCLAIM /
                    // XPENDING / XCLAIM when those commands iterate.
                    // Removing them here makes XAUTOCLAIM's cursor
                    // skip past the deleted slot and breaks the
                    // upstream-visible "deleted_ids" reply slot.
                    // (br-frankenredis-r82v)
                    if removed > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(removed as u64);
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        };
        if let Some(max_deleted) = max_deleted {
            self.update_stream_max_deleted_id(key, max_deleted);
        }
        result
    }

    pub fn xtrim(
        &mut self,
        key: &[u8],
        max_len: usize,
        limit: Option<usize>,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut max_deleted = None;
        let result = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    if entries.len() <= max_len {
                        return Ok(0);
                    }
                    let mut to_remove = entries.len() - max_len;
                    if let Some(cap) = limit {
                        to_remove = to_remove.min(cap);
                    }
                    let remove_ids: Vec<StreamId> =
                        entries.keys().copied().take(to_remove).collect();
                    max_deleted = remove_ids.last().copied();
                    for id in &remove_ids {
                        entries.remove(id);
                    }
                    // Match Redis stream semantics: trimming removes
                    // stream entries, but pending-entry-list rows remain
                    // until XAUTOCLAIM surfaces them as deleted IDs.
                    if to_remove > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(to_remove as u64);
                    }
                    Ok(to_remove)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        };
        if let Some(max_deleted) = max_deleted {
            self.update_stream_max_deleted_id(key, max_deleted);
        }
        result
    }

    /// XTRIM key MINID threshold [LIMIT count] — remove entries with
    /// IDs less than `min_id`. When `limit` is `Some(n)`, cap removal
    /// at n entries (used by the Redis 6.2 `~ ... LIMIT n` dialect).
    pub fn xtrim_minid(
        &mut self,
        key: &[u8],
        min_id: StreamId,
        limit: Option<usize>,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut max_deleted = None;
        let result = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    let mut remove_ids: Vec<StreamId> = entries
                        .keys()
                        .copied()
                        .take_while(|id| *id < min_id)
                        .collect();
                    if let Some(cap) = limit {
                        remove_ids.truncate(cap);
                    }
                    let removed = remove_ids.len();
                    max_deleted = remove_ids.last().copied();
                    for id in &remove_ids {
                        entries.remove(id);
                    }
                    // Match Redis stream semantics: trimming removes
                    // stream entries, but pending-entry-list rows remain
                    // until XAUTOCLAIM surfaces them as deleted IDs.
                    if removed > 0 {
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(removed as u64);
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        };
        if let Some(max_deleted) = max_deleted {
            self.update_stream_max_deleted_id(key, max_deleted);
        }
        result
    }

    pub fn xread(
        &mut self,
        key: &[u8],
        start_exclusive: StreamId,
        count: Option<usize>,
        now_ms: u64,
    ) -> Result<Vec<StreamRecord>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    if matches!(count, Some(0)) {
                        return Ok(Vec::new());
                    }
                    let mut out = Vec::new();
                    for (id, fields) in entries.range((Excluded(start_exclusive), Unbounded)) {
                        out.push((*id, fields.clone()));
                        if let Some(limit) = count
                            && out.len() >= limit
                        {
                            break;
                        }
                    }
                    Ok(out)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn xreadgroup(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        options: StreamGroupReadOptions,
        now_ms: u64,
    ) -> Result<Option<Vec<StreamRecord>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        let StreamGroupReadOptions {
            cursor,
            noack,
            count,
        } = options;

        let records = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };
                    let limit = count.unwrap_or(usize::MAX);
                    let mut out = Vec::new();
                    if limit > 0 {
                        match cursor {
                            StreamGroupReadCursor::NewEntries => {
                                for (id, fields) in entries
                                    .range((Excluded(group_state.last_delivered_id), Unbounded))
                                {
                                    out.push((*id, fields.clone()));
                                    if out.len() >= limit {
                                        break;
                                    }
                                }
                            }
                            StreamGroupReadCursor::Id(start_id) => {
                                for (id, pending_entry) in
                                    group_state.pending.range((Included(start_id), Unbounded))
                                {
                                    if pending_entry.consumer.as_slice() != consumer {
                                        continue;
                                    }
                                    if let Some(fields) = entries.get(id) {
                                        out.push((*id, fields.clone()));
                                    }
                                    if out.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    out
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };
        let last_seen_id = records.last().map(|(id, _)| *id);

        let Some(groups) = self.stream_groups.get_mut(key) else {
            return Ok(None);
        };
        let Some(group_state) = groups.get_mut(group) else {
            return Ok(None);
        };
        let consumer = consumer.to_vec();
        group_state.consumers.insert(consumer.clone());
        if let StreamGroupReadCursor::NewEntries = cursor
            && let Some(last_seen_id) = last_seen_id
        {
            group_state.last_delivered_id = last_seen_id;
            if !noack {
                for (id, _) in &records {
                    let pending_entry =
                        group_state
                            .pending
                            .entry(*id)
                            .or_insert_with(|| StreamPendingEntry {
                                consumer: consumer.clone(),
                                deliveries: 0,
                                last_delivered_ms: now_ms,
                            });
                    pending_entry.consumer = consumer.clone();
                    pending_entry.deliveries = pending_entry.deliveries.saturating_add(1);
                    pending_entry.last_delivered_ms = now_ms;
                }
            }
            // Consumer group state was mutated — mark dirty for AOF persistence
            self.dirty = self.dirty.saturating_add(1);
        }
        // Note: when reading pending entries (cursor is Id), Redis does NOT
        // increment delivery count - it's a non-destructive replay.

        Ok(Some(records))
    }

    pub fn xpending_summary(
        &mut self,
        key: &[u8],
        group: &[u8],
        now_ms: u64,
    ) -> Result<Option<StreamPendingSummary>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };

                    let mut per_consumer: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
                    for pending_entry in group_state.pending.values() {
                        *per_consumer
                            .entry(pending_entry.consumer.clone())
                            .or_default() += 1;
                    }

                    Ok(Some((
                        group_state.pending.len(),
                        group_state.pending.first_key_value().map(|(id, _)| *id),
                        group_state.pending.last_key_value().map(|(id, _)| *id),
                        per_consumer.into_iter().collect(),
                    )))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn xpending_entries(
        &mut self,
        key: &[u8],
        group: &[u8],
        bounds: (StreamId, StreamId),
        count: usize,
        consumer: Option<&[u8]>,
        now_ms: u64,
        min_idle_ms: u64,
    ) -> Result<Option<Vec<StreamPendingRecord>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };
                    let (start, end) = bounds;
                    if count == 0 || start > end {
                        return Ok(Some(Vec::new()));
                    }

                    let mut out = Vec::new();
                    for (id, pending_entry) in group_state.pending.range(start..=end) {
                        if let Some(filter_consumer) = consumer
                            && pending_entry.consumer.as_slice() != filter_consumer
                        {
                            continue;
                        }
                        if min_idle_ms > 0 {
                            let idle = now_ms.saturating_sub(pending_entry.last_delivered_ms);
                            if idle < min_idle_ms {
                                continue;
                            }
                        }
                        out.push((
                            *id,
                            pending_entry.consumer.clone(),
                            now_ms.saturating_sub(pending_entry.last_delivered_ms),
                            pending_entry.deliveries,
                        ));
                        if out.len() >= count {
                            break;
                        }
                    }
                    Ok(Some(out))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn xclaim(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        ids: &[StreamId],
        options: StreamClaimOptions,
        now_ms: u64,
    ) -> Result<Option<StreamClaimReply>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }

        let stream_records = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let mut out = BTreeMap::new();
                    for id in ids {
                        if let Some(fields) = entries.get(id) {
                            out.insert(*id, fields.clone());
                        }
                    }
                    out
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };

        let Some(groups) = self.stream_groups.get_mut(key) else {
            return Ok(None);
        };
        let Some(group_state) = groups.get_mut(group) else {
            return Ok(None);
        };

        if let Some(last_id) = options.last_id {
            group_state.last_delivered_id = last_id;
        }

        let mut claimed_ids = Vec::new();
        let mut claimed_entries = Vec::new();
        let consumer_vec = consumer.to_vec();

        for id in ids {
            let Some(fields) = stream_records.get(id) else {
                group_state.pending.remove(id);
                continue;
            };

            let mut created_by_force = false;
            if !group_state.pending.contains_key(id) {
                if !options.force {
                    continue;
                }
                group_state.pending.insert(
                    *id,
                    StreamPendingEntry {
                        consumer: consumer_vec.clone(),
                        deliveries: 0,
                        last_delivered_ms: now_ms,
                    },
                );
                created_by_force = true;
            }

            let Some(pending_entry) = group_state.pending.get_mut(id) else {
                continue;
            };
            if !created_by_force {
                let idle_ms = now_ms.saturating_sub(pending_entry.last_delivered_ms);
                if idle_ms < options.min_idle_time_ms {
                    continue;
                }
            }

            pending_entry.consumer = consumer_vec.clone();
            pending_entry.last_delivered_ms = if let Some(time_ms) = options.time_ms {
                time_ms
            } else if let Some(idle_ms) = options.idle_ms {
                now_ms.saturating_sub(idle_ms)
            } else {
                now_ms
            };

            if let Some(retry_count) = options.retry_count {
                pending_entry.deliveries = retry_count;
            } else if !options.justid {
                pending_entry.deliveries = pending_entry.deliveries.saturating_add(1);
            }

            claimed_ids.push(*id);
            if !options.justid {
                claimed_entries.push((*id, fields.clone()));
            }
        }

        if !claimed_ids.is_empty() {
            group_state.consumers.insert(consumer_vec);
            self.dirty = self.dirty.saturating_add(claimed_ids.len() as u64);
        }

        if options.justid {
            Ok(Some(StreamClaimReply::Ids(claimed_ids)))
        } else {
            Ok(Some(StreamClaimReply::Entries(claimed_entries)))
        }
    }

    pub fn xautoclaim(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        start: StreamId,
        options: StreamAutoClaimOptions,
        now_ms: u64,
    ) -> Result<Option<StreamAutoClaimReply>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }

        let (stream_records, pending_snapshot) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };

                    let scan_limit = options.count.saturating_mul(10).max(1);
                    let snapshot: Vec<(StreamId, StreamPendingEntry)> = group_state
                        .pending
                        .range(start..)
                        .take(scan_limit)
                        .map(|(id, pending_entry)| (*id, pending_entry.clone()))
                        .collect();

                    let mut fields_by_id = BTreeMap::new();
                    for (id, _) in &snapshot {
                        if let Some(fields) = entries.get(id) {
                            fields_by_id.insert(*id, fields.clone());
                        }
                    }

                    (fields_by_id, snapshot)
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };

        let mut claimed_ids = Vec::new();
        let mut deleted_ids = Vec::new();
        let mut scanned_last: Option<StreamId> = None;
        // Upstream t_stream.c::xautoclaimCommand decrements the same
        // `count` budget for both claimed and deleted entries, so the
        // top-level reply is bounded at `count` regardless of how
        // many orphans we surface. (br-frankenredis-r82v)
        let mut budget = options.count;
        for (id, pending_entry) in &pending_snapshot {
            if budget == 0 {
                break;
            }
            scanned_last = Some(*id);
            if !stream_records.contains_key(id) {
                deleted_ids.push(*id);
                budget -= 1;
                continue;
            }

            let idle_ms = now_ms.saturating_sub(pending_entry.last_delivered_ms);
            if idle_ms < options.min_idle_time_ms {
                continue;
            }

            claimed_ids.push(*id);
            budget -= 1;
        }

        let Some(groups) = self.stream_groups.get_mut(key) else {
            return Ok(None);
        };
        let Some(group_state) = groups.get_mut(group) else {
            return Ok(None);
        };

        for id in &deleted_ids {
            group_state.pending.remove(id);
        }

        let consumer_vec = consumer.to_vec();
        for id in &claimed_ids {
            if let Some(pending_entry) = group_state.pending.get_mut(id) {
                pending_entry.consumer = consumer_vec.clone();
                pending_entry.last_delivered_ms = now_ms;
                if !options.justid {
                    pending_entry.deliveries = pending_entry.deliveries.saturating_add(1);
                }
            }
        }
        if !claimed_ids.is_empty() {
            group_state.consumers.insert(consumer_vec);
            self.dirty = self.dirty.saturating_add(claimed_ids.len() as u64);
        }

        let next_start = scanned_last
            .and_then(|id| {
                group_state
                    .pending
                    .range((Excluded(id), Unbounded))
                    .next()
                    .map(|(next_id, _)| *next_id)
            })
            .unwrap_or((0, 0));

        if options.justid {
            Ok(Some(StreamAutoClaimReply::Ids {
                next_start,
                ids: claimed_ids,
                deleted_ids,
            }))
        } else {
            let entries = claimed_ids
                .iter()
                .filter_map(|id| stream_records.get(id).cloned().map(|fields| (*id, fields)))
                .collect();
            Ok(Some(StreamAutoClaimReply::Entries {
                next_start,
                entries,
                deleted_ids,
            }))
        }
    }

    pub fn xinfo_stream(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<StreamInfoBounds>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => {
                    let len = entries.len();
                    let first = entries
                        .first_key_value()
                        .map(|(id, fields)| (*id, fields.clone()));
                    let last = entries
                        .last_key_value()
                        .map(|(id, fields)| (*id, fields.clone()));
                    Ok(Some((len, first, last)))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    #[must_use]
    pub fn stream_max_deleted_id(&self, key: &[u8]) -> Option<StreamId> {
        self.stream_max_deleted_ids.get(key).copied()
    }

    pub fn xgroup_create(
        &mut self,
        key: &[u8],
        group: &[u8],
        start_id: StreamId,
        mkstream: bool,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        let key_exists_as_stream = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => true,
                _ => return Err(StoreError::WrongType),
            },
            None => false,
        };

        if !key_exists_as_stream {
            if !mkstream {
                return Err(StoreError::KeyNotFound);
            }
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
            self.internal_entries_insert(
                key.to_vec(),
                Entry::new(Value::Stream(BTreeMap::new()), None, now_ms),
            );
        }

        let groups = self.stream_groups.entry(key.to_vec()).or_default();
        if groups.contains_key(group) {
            return Ok(false);
        }
        groups.insert(
            group.to_vec(),
            StreamGroup {
                last_delivered_id: start_id,
                consumers: BTreeSet::new(),
                pending: BTreeMap::new(),
            },
        );
        self.dirty = self.dirty.saturating_add(1);
        Ok(true)
    }

    pub fn xgroup_destroy(
        &mut self,
        key: &[u8],
        group: &[u8],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let mut removed = false;
                    let mut remove_groups_key = false;
                    if let Some(groups) = self.stream_groups.get_mut(key) {
                        removed = groups.remove(group).is_some();
                        remove_groups_key = groups.is_empty();
                    }
                    if remove_groups_key {
                        self.stream_groups.remove(key);
                        self.stream_last_ids.remove(key);
                    }
                    if removed {
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    pub fn xgroup_setid(
        &mut self,
        key: &[u8],
        group: &[u8],
        last_delivered_id: StreamId,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    if let Some(groups) = self.stream_groups.get_mut(key)
                        && let Some(current_group) = groups.get_mut(group)
                    {
                        current_group.last_delivered_id = last_delivered_id;
                        self.dirty = self.dirty.saturating_add(1);
                        return Ok(true);
                    }
                    Ok(false)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Err(StoreError::KeyNotFound),
        }
    }

    pub fn xinfo_groups(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<StreamGroupInfo>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(None);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let groups = self
                        .stream_groups
                        .get(key)
                        .map(|groups| {
                            groups
                                .iter()
                                .map(|(name, group)| {
                                    (
                                        name.clone(),
                                        group.consumers.len(),
                                        group.pending.len(),
                                        group.last_delivered_id,
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(Some(groups))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn xgroup_createconsumer(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        now_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get_mut(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get_mut(group) else {
                        return Ok(None);
                    };
                    let created = group_state.consumers.insert(consumer.to_vec());
                    if created {
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    Ok(Some(created))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn xinfo_consumers(
        &mut self,
        key: &[u8],
        group: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<StreamConsumerInfo>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Err(StoreError::KeyNotFound);
        }
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };
                    let mut result: Vec<StreamConsumerInfo> = Vec::new();
                    for consumer_name in &group_state.consumers {
                        // Count pending entries for this consumer
                        let pending_count = group_state
                            .pending
                            .values()
                            .filter(|pe| pe.consumer == *consumer_name)
                            .count();
                        // Compute idle time: time since last delivery to this consumer
                        let last_delivery = group_state
                            .pending
                            .values()
                            .filter(|pe| pe.consumer == *consumer_name)
                            .map(|pe| pe.last_delivered_ms)
                            .max()
                            .unwrap_or(0);
                        let idle_ms = if last_delivery > 0 {
                            now_ms.saturating_sub(last_delivery)
                        } else {
                            0
                        };
                        result.push((consumer_name.clone(), pending_count, idle_ms));
                    }
                    result.sort_by(|a, b| a.0.cmp(&b.0));
                    Ok(Some(result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Err(StoreError::KeyNotFound),
        }
    }

    pub fn xgroup_delconsumer(
        &mut self,
        key: &[u8],
        group: &[u8],
        consumer: &[u8],
        now_ms: u64,
    ) -> Result<Option<u64>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get_mut(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get_mut(group) else {
                        return Ok(None);
                    };
                    group_state.consumers.remove(consumer);
                    let mut removed_pending = 0_u64;
                    group_state.pending.retain(|_, pending_entry| {
                        let keep = pending_entry.consumer.as_slice() != consumer;
                        if !keep {
                            removed_pending = removed_pending.saturating_add(1);
                        }
                        keep
                    });
                    if removed_pending > 0 {
                        self.dirty = self.dirty.saturating_add(1); // Redis treats this as 1 mutation
                    }
                    Ok(Some(removed_pending))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// XACK: acknowledge one or more messages in a consumer group.
    /// Returns the count of IDs that were successfully acknowledged
    /// (removed from the pending entries list).
    pub fn xack(
        &mut self,
        key: &[u8],
        group: &[u8],
        ids: &[StreamId],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get_mut(key) else {
                        return Ok(0);
                    };
                    let Some(group_state) = groups.get_mut(group) else {
                        return Ok(0);
                    };
                    let mut acked = 0usize;
                    for id in ids {
                        if group_state.pending.remove(id).is_some() {
                            acked += 1;
                        }
                    }
                    if acked > 0 {
                        self.dirty = self.dirty.saturating_add(acked as u64);
                    }
                    Ok(acked)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    /// XSETID: set the last-delivered-ID of a stream.
    /// The `entries_added` and `max_deleted_entry_id` options are accepted
    /// but ignored (they affect INFO reporting, which we derive dynamically).
    /// Returns Ok(true) if the stream exists, Ok(false) if not.
    pub fn xsetid(
        &mut self,
        key: &[u8],
        last_id: StreamId,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    self.stream_last_ids.insert(key.to_vec(), last_id);
                    entry.touch_write(now_ms);
                    self.dirty = self.dirty.saturating_add(1);
                    Ok(true)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    // ── HyperLogLog commands ───────────────────────────────────────────

    /// PFADD: add elements to a HyperLogLog. Returns `true` if any internal
    /// register was altered or the key was newly created.
    pub fn pfadd(
        &mut self,
        key: &[u8],
        elements: &[Vec<u8>],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (mut registers, encoding, existed) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(data) => {
                    let (encoding, registers) = hll_parse(data)?;
                    (registers, encoding, true)
                }
                _ => return Err(StoreError::WrongType),
            },
            None => (vec![0u8; HLL_REGISTERS], HllEncoding::Sparse, false),
        };

        let mut modified = false;
        for element in elements {
            let hash = hll_hash(element);
            let index = (hash as usize) & (HLL_REGISTERS - 1);
            let w = hash >> HLL_P;
            let count = hll_rho(w);
            if count > registers[index] {
                registers[index] = count;
                modified = true;
            }
        }

        let created = !existed;
        if created || modified {
            let expires_at = self.entries.get(key).and_then(|e| e.expires_at_ms);
            let encoding = match encoding {
                HllEncoding::Dense => HllEncoding::Dense,
                HllEncoding::Sparse if hll_sparse_should_promote(&registers) => HllEncoding::Dense,
                HllEncoding::Sparse => HllEncoding::Sparse,
            };
            let data = hll_encode(&registers, encoding);
            let mut entry = Entry::new(Value::String(data), expires_at, now_ms);
            entry.touch_write(now_ms);
            self.internal_entries_insert(key.to_vec(), entry);
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(created || modified)
    }

    /// PFCOUNT: return the approximate cardinality for one or more HLL keys.
    /// Multiple keys are merged into a temporary union before estimating.
    pub fn pfcount(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        let mut merged = vec![0u8; HLL_REGISTERS];
        for &key in keys {
            self.drop_if_expired(key, now_ms);
            if let Some(entry) = self.entries.get_mut(key) {
                match &entry.value {
                    Value::String(data) => {
                        let registers = hll_parse_registers(data)?;
                        for i in 0..HLL_REGISTERS {
                            merged[i] = merged[i].max(registers[i]);
                        }
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }
        Ok(hll_estimate(&merged))
    }

    /// PFMERGE: merge source HLLs into dest. If dest already exists as an HLL
    /// its registers are included in the union (per Redis semantics).
    pub fn pfmerge(
        &mut self,
        dest: &[u8],
        sources: &[&[u8]],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut merged = vec![0u8; HLL_REGISTERS];
        let mut dest_encoding = HllEncoding::Sparse;

        // Include dest if it already holds an HLL, and preserve its TTL
        self.drop_if_expired(dest, now_ms);
        let existing_ttl = self.entries.get(dest).and_then(|e| e.expires_at_ms);
        if let Some(entry) = self.entries.get(dest) {
            match &entry.value {
                Value::String(data) => {
                    let (encoding, registers) = hll_parse(data)?;
                    dest_encoding = encoding;
                    for i in 0..HLL_REGISTERS {
                        merged[i] = merged[i].max(registers[i]);
                    }
                }
                _ => return Err(StoreError::WrongType),
            }
        }

        // Merge all sources
        for &src in sources {
            self.drop_if_expired(src, now_ms);
            if let Some(entry) = self.entries.get(src) {
                match &entry.value {
                    Value::String(data) => {
                        let registers = hll_parse_registers(data)?;
                        for i in 0..HLL_REGISTERS {
                            merged[i] = merged[i].max(registers[i]);
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }

        let dest_encoding = match dest_encoding {
            HllEncoding::Dense => HllEncoding::Dense,
            HllEncoding::Sparse if hll_sparse_should_promote(&merged) => HllEncoding::Dense,
            HllEncoding::Sparse => HllEncoding::Sparse,
        };
        let data = hll_encode(&merged, dest_encoding);
        let mut entry = Entry::new(Value::String(data), existing_ttl, now_ms);
        entry.touch_write(now_ms);
        self.internal_entries_insert(dest.to_vec(), entry);
        self.dirty = self.dirty.saturating_add(1);
        Ok(())
    }

    pub fn hll_debug_getreg(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => {
                let (encoding, registers) = match &entry.value {
                    Value::String(data) => hll_parse(data)?,
                    _ => return Err(StoreError::WrongType),
                };
                match encoding {
                    HllEncoding::Sparse => {
                        entry.value = Value::String(hll_encode(&registers, HllEncoding::Dense));
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    HllEncoding::Dense => entry.touch(now_ms),
                }
                Ok(Some(registers))
            }
            None => Ok(None),
        }
    }

    pub fn hll_debug_validate(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<()>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(data) => {
                    hll_parse(data)?;
                    entry.touch(now_ms);
                    Ok(Some(()))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn hll_debug_decode(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<String>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(data) => {
                    let (encoding, registers) = hll_parse(data)?;
                    entry.touch(now_ms);
                    match encoding {
                        HllEncoding::Sparse => Ok(Some(hll_sparse_decode(&registers)?)),
                        HllEncoding::Dense => Err(StoreError::GenericError(
                            "ERR HLL encoding is not sparse".to_string(),
                        )),
                    }
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn hll_debug_encoding(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<&'static str>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(data) => {
                    let (encoding, _) = hll_parse(data)?;
                    entry.touch(now_ms);
                    Ok(Some(encoding.as_str()))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn hll_debug_todense(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<bool>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => {
                let (encoding, registers) = match &entry.value {
                    Value::String(data) => hll_parse(data)?,
                    _ => return Err(StoreError::WrongType),
                };
                match encoding {
                    HllEncoding::Sparse => {
                        entry.value = Value::String(hll_encode(&registers, HllEncoding::Dense));
                        entry.touch_write(now_ms);
                        self.dirty = self.dirty.saturating_add(1);
                        Ok(Some(true))
                    }
                    HllEncoding::Dense => {
                        entry.touch(now_ms);
                        Ok(Some(false))
                    }
                }
            }
            None => Ok(None),
        }
    }

    pub fn hll_selftest(&self) -> Result<(), StoreError> {
        hll_run_selftest().map_err(StoreError::GenericError)
    }

    fn drop_if_expired(&mut self, key: &[u8], now_ms: u64) {
        let should_evict = evaluate_expiry(
            now_ms,
            self.entries.get(key).and_then(|entry| entry.expires_at_ms),
        )
        .should_evict;
        if should_evict && self.internal_entries_remove(key).is_some() {
            self.stream_groups.remove(key);
            self.stream_last_ids.remove(key);
            self.dirty = self.dirty.saturating_add(1);
            self.stat_expired_keys = self.stat_expired_keys.saturating_add(1);
            // Emit expired keyspace notification (use logical key, not physical)
            let (db, logical_key) = match decode_db_key(key) {
                Some((db, lk)) => (db, lk),
                None => (0, key),
            };
            self.notify_keyspace_event(NOTIFY_EXPIRED, "expired", logical_key, db);
        }
    }

    // ── Hash field TTL primitives (Redis 7.4 HEXPIRE family) ────────
    //
    // Part 1 (br-frankenredis-wwz3): storage + flag-aware setters, TTL
    // readers, and the lazy expiry helper. No hook into hash reads yet —
    // that's part 3 (br-frankenredis-b8ut). No command-level dispatch —
    // part 2 (br-frankenredis-c0z9).

    /// Set or update the absolute expiry (ms-since-epoch) for a single
    /// hash field. Returns the applied-or-rejected outcome using the
    /// upstream reply codes: 0 = condition (NX/XX/GT/LT) blocked the
    /// update, 1 = applied, 2 = applied and the expiry was already in the
    /// past so the field must be reaped, -2 = hash key or field missing.
    ///
    /// Does not emit a keyspace event. Callers that want the
    /// upstream-matching `hexpire` / `hpexpire` / `hexpireat` / `hpexpireat`
    /// event should use [`hash_field_set_abs_expiry_with_event`] which
    /// threads the event name + NOTIFY_HASH emission.
    pub fn hash_field_set_abs_expiry(
        &mut self,
        key: &[u8],
        field: &[u8],
        expires_at_ms: u64,
        cond: HashFieldTtlCondition,
        now_ms: u64,
    ) -> HashFieldTtlSet {
        // Key + field existence check. We only look at the hash value —
        // expired FIELDS (per field_expires map) don't affect the "field
        // present on the hash" semantic until part 3 wires lazy expiry.
        match self.entries.get(key).map(|e| &e.value) {
            Some(Value::Hash(map)) => {
                if !map.contains_key(field) {
                    return HashFieldTtlSet::FieldMissing;
                }
            }
            Some(_) => return HashFieldTtlSet::WrongType,
            None => return HashFieldTtlSet::KeyMissing,
        }

        let composite = (key.to_vec(), field.to_vec());
        let current = self.hash_field_expires.get(&composite).copied();

        let allow = match (cond, current) {
            (HashFieldTtlCondition::None, _) => true,
            (HashFieldTtlCondition::Nx, None) => true,
            (HashFieldTtlCondition::Nx, Some(_)) => false,
            (HashFieldTtlCondition::Xx, Some(_)) => true,
            (HashFieldTtlCondition::Xx, None) => false,
            (HashFieldTtlCondition::Gt, Some(existing)) => expires_at_ms > existing,
            // Upstream: GT with no existing TTL is a no-op (not satisfied).
            (HashFieldTtlCondition::Gt, None) => false,
            (HashFieldTtlCondition::Lt, Some(existing)) => expires_at_ms < existing,
            // Upstream: LT with no existing TTL applies (anything < infinity).
            (HashFieldTtlCondition::Lt, None) => true,
        };
        if !allow {
            return HashFieldTtlSet::ConditionNotMet;
        }

        self.hash_field_expires.insert(composite, expires_at_ms);
        self.dirty = self.dirty.saturating_add(1);

        if expires_at_ms <= now_ms {
            HashFieldTtlSet::AppliedAlreadyExpired
        } else {
            HashFieldTtlSet::Applied
        }
    }

    /// Wrapper around [`hash_field_set_abs_expiry`] that emits the
    /// upstream-matching NOTIFY_HASH keyspace event ("hexpire" /
    /// "hpexpire" / "hexpireat" / "hpexpireat") on success. The Applied
    /// and AppliedAlreadyExpired outcomes both fire the event; blocked
    /// (NX/XX/GT/LT) and missing/wrong-type do not.
    /// (br-frankenredis-7jhg)
    pub fn hash_field_set_abs_expiry_with_event(
        &mut self,
        key: &[u8],
        field: &[u8],
        expires_at_ms: u64,
        cond: HashFieldTtlCondition,
        now_ms: u64,
        event: &str,
    ) -> HashFieldTtlSet {
        let outcome = self.hash_field_set_abs_expiry(key, field, expires_at_ms, cond, now_ms);
        if matches!(
            outcome,
            HashFieldTtlSet::Applied | HashFieldTtlSet::AppliedAlreadyExpired
        ) && self.notify_keyspace_events != 0
        {
            let (db, logical_key) = match decode_db_key(key) {
                Some((db, lk)) => (db, lk.to_vec()),
                None => (0, key.to_vec()),
            };
            self.notify_keyspace_event(NOTIFY_HASH, event, &logical_key, db);
        }
        outcome
    }

    /// Look up the TTL state for a single hash field.
    ///
    /// `unit` selects whether `HashFieldTtl::Remaining` is expressed in
    /// milliseconds (for HPTTL/HPEXPIRETIME callers) or whole seconds
    /// rounded up (for HTTL/HEXPIRETIME). `absolute` toggles between
    /// remaining-time and absolute-expiry semantics.
    #[must_use]
    pub fn hash_field_ttl(
        &self,
        key: &[u8],
        field: &[u8],
        now_ms: u64,
        unit: HashFieldTtlUnit,
        absolute: bool,
    ) -> HashFieldTtl {
        match self.entries.get(key).map(|e| &e.value) {
            Some(Value::Hash(map)) => {
                if !map.contains_key(field) {
                    return HashFieldTtl::FieldMissing;
                }
            }
            Some(_) => return HashFieldTtl::WrongType,
            None => return HashFieldTtl::KeyMissing,
        }

        let Some(&expires_at_ms) = self.hash_field_expires.get(&(key.to_vec(), field.to_vec()))
        else {
            return HashFieldTtl::NoTtl;
        };
        if expires_at_ms <= now_ms {
            return HashFieldTtl::Expired;
        }
        let ms = if absolute {
            expires_at_ms
        } else {
            expires_at_ms.saturating_sub(now_ms)
        };
        let value = match unit {
            HashFieldTtlUnit::Milliseconds => ms,
            HashFieldTtlUnit::Seconds => {
                if absolute {
                    // For HEXPIRETIME: truncate (upstream reports the
                    // integer-second floor of the epoch).
                    ms / 1000
                } else {
                    // HTTL rounds up so "600 ms remaining" reports 1 not 0.
                    ms.div_ceil(1000)
                }
            }
        };
        HashFieldTtl::Remaining(value)
    }

    /// Drop the TTL for a single hash field; the field persists with its
    /// hash indefinitely after this call.
    pub fn hash_field_persist(&mut self, key: &[u8], field: &[u8]) -> HashFieldPersistResult {
        match self.entries.get(key).map(|e| &e.value) {
            Some(Value::Hash(map)) => {
                if !map.contains_key(field) {
                    return HashFieldPersistResult::FieldMissing;
                }
            }
            Some(_) => return HashFieldPersistResult::WrongType,
            None => return HashFieldPersistResult::KeyMissing,
        }
        let composite = (key.to_vec(), field.to_vec());
        match self.hash_field_expires.remove(&composite) {
            Some(_) => {
                self.dirty = self.dirty.saturating_add(1);
                HashFieldPersistResult::Persisted
            }
            None => HashFieldPersistResult::NoTtl,
        }
    }

    /// Wrapper around [`hash_field_persist`] that emits the
    /// upstream-matching NOTIFY_HASH "hpersist" keyspace event on the
    /// Persisted outcome. (br-frankenredis-7jhg)
    pub fn hash_field_persist_with_event(
        &mut self,
        key: &[u8],
        field: &[u8],
    ) -> HashFieldPersistResult {
        let outcome = self.hash_field_persist(key, field);
        if matches!(outcome, HashFieldPersistResult::Persisted) && self.notify_keyspace_events != 0
        {
            let (db, logical_key) = match decode_db_key(key) {
                Some((db, lk)) => (db, lk.to_vec()),
                None => (0, key.to_vec()),
            };
            self.notify_keyspace_event(NOTIFY_HASH, "hpersist", &logical_key, db);
        }
        outcome
    }

    /// True if `field` on `key` is expired (past deadline) per the
    /// per-field TTL map. False for fields with no TTL, for missing
    /// hashes, or for non-hash keys.
    #[must_use]
    pub fn hash_field_is_expired(&self, key: &[u8], field: &[u8], now_ms: u64) -> bool {
        self.hash_field_expires
            .get(&(key.to_vec(), field.to_vec()))
            .is_some_and(|&at| at <= now_ms)
    }

    /// Number of hash keys carrying at least one per-field TTL. Used by
    /// OBJECT ENCODING (to flip to listpack_ex/hashtable_ex) once part 4
    /// lands; also exposed for tests.
    #[must_use]
    pub fn hash_field_ttl_carrier_count(&self) -> usize {
        let mut seen: HashSet<&[u8]> = HashSet::new();
        for (key, _) in self.hash_field_expires.keys() {
            seen.insert(key.as_slice());
        }
        seen.len()
    }

    /// Remove every per-field TTL entry for `key`. Called when the whole
    /// hash key is deleted (DEL / expired / FLUSH*) so the field-TTL map
    /// doesn't accumulate orphan entries. Also clears the DEBUG OBJECT
    /// expired-fields counter — a new incarnation of the same key
    /// name starts counting from zero (br-frankenredis-25re).
    pub fn hash_field_ttl_clear_for_key(&mut self, key: &[u8]) {
        // BTreeMap range over (key, _) prefix to enumerate + retain.
        let victims: Vec<(Vec<u8>, Vec<u8>)> = self
            .hash_field_expires
            .range((key.to_vec(), Vec::new())..)
            .take_while(|((k, _), _)| k.as_slice() == key)
            .map(|((k, f), _)| (k.clone(), f.clone()))
            .collect();
        for composite in victims {
            self.hash_field_expires.remove(&composite);
        }
        self.hash_field_expired_counts.remove(key);
    }

    /// Cumulative count of per-field TTL reaps for `key` since the
    /// current incarnation of the key was created. Used by DEBUG OBJECT
    /// to report `hexpired_fields:<n>`. (br-frankenredis-25re)
    #[must_use]
    pub fn hash_field_expired_count(&self, key: &[u8]) -> u64 {
        self.hash_field_expired_counts
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Remove the per-field TTL for (`key`, `field`). Called from HDEL
    /// once part 3 lands so stale TTLs don't shadow re-added fields.
    pub fn hash_field_ttl_clear_for_field(&mut self, key: &[u8], field: &[u8]) {
        self.hash_field_expires
            .remove(&(key.to_vec(), field.to_vec()));
    }

    /// GETEX: get string value and optionally set/remove expiration.
    pub fn getex(
        &mut self,
        key: &[u8],
        new_expires_at_ms: Option<Option<u64>>,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let result = v.clone();
                    if let Some(exp) = new_expires_at_ms {
                        let was_exp = entry.expires_at_ms.is_some();
                        let is_exp = exp.is_some();
                        if was_exp != is_exp {
                            let db = decode_db_key(key).map(|(db, _)| db).unwrap_or(0);
                            if is_exp {
                                self.expires_count = self.expires_count.saturating_add(1);
                                if db < self.database_count {
                                    self.db_expires_counts[db] =
                                        self.db_expires_counts[db].saturating_add(1);
                                }
                            } else {
                                self.expires_count = self.expires_count.saturating_sub(1);
                                if db < self.database_count {
                                    self.db_expires_counts[db] =
                                        self.db_expires_counts[db].saturating_sub(1);
                                }
                            }
                        }
                        entry.expires_at_ms = exp;
                        Self::mark_digest_stale_fields(
                            &mut self.digest_stale,
                            &mut self.digest_mutations,
                        );
                        self.dirty = self.dirty.saturating_add(1);
                    }
                    Ok(Some(result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// BITOP: perform bitwise operation between strings.
    pub fn bitop(
        &mut self,
        op: &[u8],
        dest: &[u8],
        keys: &[&[u8]],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        // Collect values, treating missing keys as empty strings.
        // Enforce a total memory limit for the operation to prevent DoS.
        const MAX_BITOP_TOTAL_BYTES: usize = 512 * 1024 * 1024; // 512 MiB
        let mut total_bytes = 0usize;
        let mut values: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
        for &key in keys {
            self.drop_if_expired(key, now_ms);
            match self.entries.get(key) {
                Some(entry) => match &entry.value {
                    Value::String(v) => {
                        total_bytes = total_bytes.saturating_add(v.len());
                        if total_bytes > MAX_BITOP_TOTAL_BYTES {
                            return Err(StoreError::GenericError(
                                "BITOP total input size exceeds limit".to_string(),
                            ));
                        }
                        values.push(v.clone());
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => values.push(Vec::new()),
            }
        }

        let max_len = values.iter().map(|v| v.len()).max().unwrap_or(0);
        let mut result = vec![0u8; max_len];

        if eq_ascii_ci(op, b"NOT") {
            if values.len() != 1 {
                return Err(StoreError::WrongType);
            }
            for (i, byte) in result.iter_mut().enumerate() {
                *byte = !values[0].get(i).copied().unwrap_or(0);
            }
        } else {
            // Initialize with first value
            if let Some(first) = values.first() {
                for (i, byte) in result.iter_mut().enumerate() {
                    *byte = first.get(i).copied().unwrap_or(0);
                }
            }

            let is_and = eq_ascii_ci(op, b"AND");
            let is_or = eq_ascii_ci(op, b"OR");
            let is_xor = eq_ascii_ci(op, b"XOR");

            for val in values.iter().skip(1) {
                if is_and {
                    for (i, byte) in result.iter_mut().enumerate() {
                        *byte &= val.get(i).copied().unwrap_or(0);
                    }
                } else if is_or {
                    for (i, byte) in result.iter_mut().enumerate() {
                        *byte |= val.get(i).copied().unwrap_or(0);
                    }
                } else if is_xor {
                    for (i, byte) in result.iter_mut().enumerate() {
                        *byte ^= val.get(i).copied().unwrap_or(0);
                    }
                }
            }
        }

        let len = result.len();
        self.stream_groups.remove(dest);
        self.stream_last_ids.remove(dest);
        // Upstream bitops.c::bitopCommand deletes the destination
        // key when the result length is 0, rather than storing an
        // empty string. (br-frankenredis-bitopempty)
        if len == 0 {
            self.entries.remove(dest);
        } else {
            self.internal_entries_insert(
                dest.to_vec(),
                Entry::new(Value::String(result), None, now_ms),
            );
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(len)
    }

    // ── Sorted Set algebra operations ──────────────────────────────

    /// ZUNIONSTORE: store union of sorted sets.
    pub fn zunionstore(
        &mut self,
        dest: &[u8],
        keys: &[&[u8]],
        weights: &[f64],
        aggregate: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let mut combined: HashMap<Vec<u8>, f64> = HashMap::new();

        for (i, &key) in keys.iter().enumerate() {
            self.drop_if_expired(key, now_ms);
            let weight = weights.get(i).copied().unwrap_or(1.0);
            if let Some(entry) = self.entries.get_mut(key) {
                let mut add_member = |member: &Vec<u8>, score: f64| {
                    let weighted = score * weight;
                    use std::collections::hash_map::Entry as HEntry;
                    match combined.entry(member.clone()) {
                        HEntry::Vacant(e) => {
                            e.insert(weighted);
                        }
                        HEntry::Occupied(mut e) => {
                            let current = e.get_mut();
                            *current = aggregate_scores(*current, weighted, aggregate);
                        }
                    }
                };

                match &entry.value {
                    Value::SortedSet(zs) => {
                        for (member, &score) in zs.iter() {
                            add_member(member, score);
                        }
                        entry.touch(now_ms);
                    }
                    Value::Set(s) => {
                        for member in s.iter() {
                            add_member(member, 1.0);
                        }
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }

        let count = combined.len();
        let deleted = self.internal_entries_remove(dest).is_some();
        self.stream_groups.remove(dest);
        self.stream_last_ids.remove(dest);

        if count > 0 {
            let mut zs = SortedSet::new();
            for (m, s) in combined {
                zs.insert(m, s);
            }
            self.internal_entries_insert(
                dest.to_vec(),
                Entry::new(Value::SortedSet(zs), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
        } else if deleted {
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(count)
    }

    /// ZINTERSTORE: store intersection of sorted sets.
    pub fn zinterstore(
        &mut self,
        dest: &[u8],
        keys: &[&[u8]],
        weights: &[f64],
        aggregate: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        if keys.is_empty() {
            let deleted = self.internal_entries_remove(dest).is_some();
            self.stream_groups.remove(dest);
            self.stream_last_ids.remove(dest);
            if deleted {
                self.dirty = self.dirty.saturating_add(1);
            }
            return Ok(0);
        }

        let mut min_card = usize::MAX;
        let mut min_idx = 0;
        let mut has_empty = false;

        for (i, &key) in keys.iter().enumerate() {
            self.drop_if_expired(key, now_ms);
            match self.entries.get(key) {
                Some(entry) => match &entry.value {
                    Value::SortedSet(zs) => {
                        if zs.len() < min_card {
                            min_card = zs.len();
                            min_idx = i;
                        }
                    }
                    Value::Set(s) => {
                        if s.len() < min_card {
                            min_card = s.len();
                            min_idx = i;
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    has_empty = true;
                }
            }
        }

        if has_empty {
            for key in keys {
                if let Some(entry) = self.entries.get_mut(*key)
                    && matches!(entry.value, Value::SortedSet(_) | Value::Set(_))
                {
                    entry.touch(now_ms);
                }
            }
            let deleted = self.internal_entries_remove(dest).is_some();
            self.stream_groups.remove(dest);
            self.stream_last_ids.remove(dest);
            if deleted {
                self.dirty = self.dirty.saturating_add(1);
            }
            return Ok(0);
        }

        let mut result: HashMap<Vec<u8>, f64> = match self.entries.get_mut(keys[min_idx]) {
            Some(entry) => {
                let w = weights.get(min_idx).copied().unwrap_or(1.0);
                let res = match &entry.value {
                    Value::SortedSet(zs) => {
                        zs.dict.iter().map(|(m, &s)| (m.clone(), s * w)).collect()
                    }
                    Value::Set(s) => s.iter().map(|m| (m.clone(), 1.0 * w)).collect(),
                    _ => return Err(StoreError::WrongType),
                };
                entry.touch(now_ms);
                res
            }
            None => HashMap::new(),
        };

        for (i, &key) in keys.iter().enumerate() {
            if i == min_idx {
                continue;
            }
            let weight = weights.get(i).copied().unwrap_or(1.0);
            if result.is_empty() {
                if let Some(entry) = self.entries.get_mut(key)
                    && matches!(entry.value, Value::SortedSet(_) | Value::Set(_))
                {
                    entry.touch(now_ms);
                }
                continue;
            }
            match self.entries.get_mut(key) {
                Some(entry) => match &entry.value {
                    Value::SortedSet(zs) => {
                        result.retain(|member, score| {
                            if let Some(&other_score) = zs.dict.get(member) {
                                *score = aggregate_scores(*score, other_score * weight, aggregate);
                                true
                            } else {
                                false
                            }
                        });
                        entry.touch(now_ms);
                    }
                    Value::Set(s) => {
                        result.retain(|member, score| {
                            if s.contains(member) {
                                *score = aggregate_scores(*score, 1.0 * weight, aggregate);
                                true
                            } else {
                                false
                            }
                        });
                        entry.touch(now_ms);
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    result.clear();
                }
            }
        }

        let count = result.len();
        let deleted = self.internal_entries_remove(dest).is_some();
        self.stream_groups.remove(dest);
        self.stream_last_ids.remove(dest);

        if count > 0 {
            let mut zs = SortedSet::new();
            for (m, s) in result {
                zs.insert(m, s);
            }
            self.internal_entries_insert(
                dest.to_vec(),
                Entry::new(Value::SortedSet(zs), None, now_ms),
            );
            self.dirty = self.dirty.saturating_add(1);
        } else if deleted {
            self.dirty = self.dirty.saturating_add(1);
        }
        Ok(count)
    }

    /// SMISMEMBER: check membership for multiple members.
    pub fn smismember(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
        now_ms: u64,
    ) -> Result<Vec<bool>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(vec![false; members.len()]);
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let result: Vec<bool> = members.iter().map(|m| s.contains(*m)).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(vec![false; members.len()]),
        }
    }

    // ── Server / utility operations ────────────────────────────────

    /// Return a random live key, or None if the keyspace is empty.
    #[must_use]
    pub fn randomkey(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        if self.entries.is_empty() {
            return None;
        }

        // Try up to 100 times to find a non-expired key randomly.
        // This is much faster than expiring all keys in an O(N) scan.
        for _ in 0..100 {
            let idx = (self.next_rand() as usize) % self.entries.len();
            let key = self.entries.keys().nth(idx).cloned()?;

            // Check if it's expired. If so, drop it (with stats/notifications) and try again.
            self.drop_if_expired(&key, now_ms);
            if self.entries.contains_key(&key) {
                return Some(key);
            }
            if self.entries.is_empty() {
                return None;
            }
        }

        // Fallback: if we failed many times, just pick the first key that isn't expired.
        // This handles cases where many keys are expired but not yet reaped.
        let mut expired_keys = Vec::new();
        let mut result = None;
        for (key, entry) in &self.entries {
            if evaluate_expiry(now_ms, entry.expires_at_ms).should_evict {
                expired_keys.push(key.clone());
            } else {
                result = Some(key.clone());
                break;
            }
        }
        for key in expired_keys {
            self.drop_if_expired(&key, now_ms);
        }
        result
    }

    #[must_use]
    pub fn randomkey_with_prefix(&mut self, prefix: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }

        let matching: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect();
        if matching.is_empty() {
            return None;
        }
        let idx = (self.next_rand() as usize) % matching.len();
        matching.get(idx).cloned()
    }

    #[must_use]
    pub fn randomkey_in_db(&mut self, db: usize, now_ms: u64) -> Option<Vec<u8>> {
        let matching = self.keys_in_db(db, now_ms);
        if matching.is_empty() {
            return None;
        }
        let idx = (self.next_rand() as usize) % matching.len();
        matching.get(idx).cloned()
    }

    /// Return up to `count` keys that hash to the given cluster slot.
    #[must_use]
    pub fn keys_in_slot(&mut self, slot: u16, count: usize, now_ms: u64) -> Vec<Vec<u8>> {
        self.entries
            .iter()
            .filter(|(k, e)| {
                crc16_slot(k) == slot && !evaluate_expiry(now_ms, e.expires_at_ms).should_evict
            })
            .take(count)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Count live keys that hash to the given cluster slot.
    #[must_use]
    pub fn count_keys_in_slot(&mut self, slot: u16, now_ms: u64) -> usize {
        self.entries
            .iter()
            .filter(|(k, e)| {
                crc16_slot(k) == slot && !evaluate_expiry(now_ms, e.expires_at_ms).should_evict
            })
            .count()
    }

    /// SCAN cursor-based iteration.
    /// Returns (next_cursor, keys). Cursor 0 means start / complete.
    /// This uses a simple sorted-keys approach for determinism.
    #[must_use]
    pub fn scan(
        &mut self,
        cursor: u64,
        pattern: Option<&[u8]>,
        count: usize,
        now_ms: u64,
    ) -> (u64, Vec<Vec<u8>>) {
        let start = cursor as usize;
        let batch_size = count.max(1);
        let mut result = Vec::new();
        let mut pos = start;

        let total_keys = self.entries.len();
        if start >= total_keys {
            return (0, Vec::new());
        }

        let mut processed = 0;
        for key in self.ordered_keys.iter().skip(start) {
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            pos += 1;
            processed += 1;
            if evaluate_expiry(now_ms, entry.expires_at_ms).should_evict {
                if processed >= batch_size {
                    break;
                }
                continue;
            }
            if let Some(pat) = pattern
                && !glob_match(pat, key)
            {
                if processed >= batch_size {
                    break;
                }
                continue;
            }
            result.push(key.clone());
            if processed >= batch_size {
                break;
            }
        }

        let next_cursor = if pos >= total_keys { 0 } else { pos as u64 };
        (next_cursor, result)
    }

    /// HSCAN: cursor-based iteration over hash fields.
    #[allow(clippy::type_complexity)]
    pub fn hscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        pattern: Option<&[u8]>,
        count: usize,
        now_ms: u64,
    ) -> Result<(u64, Vec<(Vec<u8>, Vec<u8>)>), StoreError> {
        self.drop_if_expired(key, now_ms);
        self.drop_expired_hash_fields(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Hash(h) => {
                    let start = cursor as usize;
                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;

                    let total_fields = h.len();
                    if start >= total_fields {
                        return Ok((0, Vec::new()));
                    }

                    let mut processed = 0;
                    for (field, value) in h.iter().skip(start) {
                        pos += 1;
                        processed += 1;
                        if let Some(pat) = pattern
                            && !glob_match(pat, field)
                        {
                            if processed >= batch_size {
                                break;
                            }
                            continue;
                        }
                        result.push((field.clone(), value.clone()));
                        if processed >= batch_size {
                            break;
                        }
                    }

                    let next = if pos >= total_fields { 0 } else { pos as u64 };
                    // SCAN-family commands are read-only: do NOT touch LRU
                    Ok((next, result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok((0, Vec::new())),
        }
    }

    /// SSCAN: cursor-based iteration over set members.
    pub fn sscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        pattern: Option<&[u8]>,
        count: usize,
        now_ms: u64,
    ) -> Result<(u64, Vec<Vec<u8>>), StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let start = cursor as usize;
                    if start >= s.len() {
                        return Ok((0, Vec::new()));
                    }

                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;
                    let mut processed = 0;
                    for member in s.iter().skip(start) {
                        pos += 1;
                        processed += 1;
                        if let Some(pat) = pattern
                            && !glob_match(pat, member)
                        {
                            if processed >= batch_size {
                                break;
                            }
                            continue;
                        }
                        result.push(member.clone());
                        if processed >= batch_size {
                            break;
                        }
                    }

                    let next = if pos >= s.len() { 0 } else { pos as u64 };
                    // SCAN-family commands are read-only: do NOT touch LRU
                    Ok((next, result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok((0, Vec::new())),
        }
    }

    /// ZSCAN: cursor-based iteration over sorted set members.
    #[allow(clippy::type_complexity)]
    pub fn zscan(
        &mut self,
        key: &[u8],
        cursor: u64,
        pattern: Option<&[u8]>,
        count: usize,
        now_ms: u64,
    ) -> Result<(u64, Vec<(Vec<u8>, f64)>), StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let start = cursor as usize;
                    if start >= zs.len() {
                        return Ok((0, Vec::new()));
                    }

                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;
                    let mut processed = 0;

                    for (member, score) in zs.iter_asc().skip(start) {
                        pos += 1;
                        processed += 1;
                        if let Some(pat) = pattern
                            && !glob_match(pat, member)
                        {
                            if processed >= batch_size {
                                break;
                            }
                            continue;
                        }
                        result.push((member.clone(), *score));
                        if processed >= batch_size {
                            break;
                        }
                    }

                    let next = if pos >= zs.len() { 0 } else { pos as u64 };
                    // SCAN-family commands are read-only: do NOT touch LRU
                    Ok((next, result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok((0, Vec::new())),
        }
    }

    /// TOUCH: returns count of keys that exist and updates last access time.
    pub fn touch(&mut self, keys: &[&[u8]], now_ms: u64) -> i64 {
        let mut count = 0i64;
        let lfu_tracking_enabled = self.lfu_tracking_enabled();
        for &key in keys {
            if !self.record_keyspace_lookup(key, now_ms) {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(key) {
                if lfu_tracking_enabled {
                    entry.bump_lfu_freq(now_ms, self.lfu_decay_time);
                }
                entry.touch(now_ms);
                count += 1;
            }
        }
        count
    }

    /// COPY: copy value from source to destination.
    pub fn copy(
        &mut self,
        source: &[u8],
        destination: &[u8],
        replace: bool,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(source, now_ms);
        self.drop_if_expired(destination, now_ms);

        let entry = match self.entries.get(source) {
            Some(e) => e.clone(),
            None => return Ok(false),
        };

        if !replace && self.entries.contains_key(destination) {
            return Ok(false);
        }

        self.stream_groups.remove(destination);
        self.stream_last_ids.remove(destination);
        // Copy stream consumer groups if source has them
        if let Some(groups) = self.stream_groups.get(source) {
            self.stream_groups
                .insert(destination.to_vec(), groups.clone());
        }
        // Copy stream last-generated-id if source has one
        if let Some(&last_id) = self.stream_last_ids.get(source) {
            self.stream_last_ids.insert(destination.to_vec(), last_id);
        }
        self.internal_entries_insert(destination.to_vec(), entry);
        self.dirty = self.dirty.saturating_add(1);
        Ok(true)
    }

    /// Get elements from a list, set, or sorted set for SORT command.
    /// Returns the elements as a vector of byte vectors.
    /// For sorted sets, returns the members (not scores).
    /// Returns empty vec if key does not exist.
    /// Returns Err(WrongType) for non-sortable types (string, hash, stream).
    pub fn sort_elements(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if !self.record_keyspace_lookup(key, now_ms) {
            return Ok(Vec::new());
        }
        match self.entries.get_mut(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let result = l.iter().cloned().collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                Value::Set(s) => {
                    let result = s.iter().cloned().collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                Value::SortedSet(zs) => {
                    let result = zs.iter_asc().map(|(m, _)| m.clone()).collect();
                    entry.touch(now_ms);
                    Ok(result)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Replace the value at `key` with a list built from `elements`.
    /// Used by SORT ... STORE to write the sorted result.
    pub fn store_as_list(&mut self, key: Vec<u8>, elements: Vec<Vec<u8>>) {
        self.stream_groups.remove(key.as_slice());
        self.stream_last_ids.remove(key.as_slice());
        self.internal_entries_insert(
            key,
            Entry::new(Value::List(elements.into_iter().collect()), None, 0),
        );
        self.dirty = self.dirty.saturating_add(1);
    }

    /// Compute a fingerprint for a single key's current state.
    /// Returns 0 if the key does not exist or is expired.
    /// Used by WATCH/EXEC to detect key modifications.
    #[must_use]
    pub fn key_fingerprint(&self, key: &[u8], now_ms: u64) -> u64 {
        let entry = match self.entries.get(key) {
            Some(e) => e,
            None => return 0,
        };
        if let Some(exp) = entry.expires_at_ms
            && now_ms >= exp
        {
            return 0;
        }
        Self::entry_state_digest(key, entry)
    }

    fn entry_state_digest(key: &[u8], entry: &Entry) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        hash = fnv1a_update(hash, key);
        match &entry.value {
            Value::String(v) => {
                hash = fnv1a_update(hash, b"S");
                hash = fnv1a_update(hash, v);
            }
            Value::Hash(m) => {
                hash = fnv1a_update(hash, b"H");
                for (k, v) in m {
                    hash = fnv1a_update(hash, k);
                    hash = fnv1a_update(hash, v);
                }
            }
            Value::List(l) => {
                hash = fnv1a_update(hash, b"L");
                for item in l {
                    hash = fnv1a_update(hash, item);
                }
            }
            Value::Set(s) => {
                hash = fnv1a_update(hash, b"E");
                let mut members: Vec<_> = s.iter().collect();
                members.sort();
                for m in members {
                    hash = fnv1a_update(hash, m);
                }
            }
            Value::SortedSet(zs) => {
                hash = fnv1a_update(hash, b"Z");
                for (member, score) in zs.iter_asc() {
                    hash = fnv1a_update(hash, member);
                    hash = fnv1a_update(hash, &score.to_bits().to_le_bytes());
                }
            }
            Value::Stream(entries) => {
                hash = fnv1a_update(hash, b"X");
                for ((ms, seq), fields) in entries {
                    hash = fnv1a_update(hash, &ms.to_le_bytes());
                    hash = fnv1a_update(hash, &seq.to_le_bytes());
                    for (field, value) in fields {
                        hash = fnv1a_update(hash, field);
                        hash = fnv1a_update(hash, value);
                    }
                }
            }
        }
        let expiry_bytes = entry.expires_at_ms.unwrap_or(0).to_le_bytes();
        hash = fnv1a_update(hash, &expiry_bytes);
        hash
    }

    /// Return the modification counter for a key (0 if key doesn't exist or is expired).
    pub fn key_modification_count(&self, key: &[u8], now_ms: u64) -> u64 {
        match self.entries.get(key) {
            Some(entry) => {
                if let Some(exp) = entry.expires_at_ms
                    && now_ms >= exp
                {
                    return 0;
                }
                entry.modification_count
            }
            None => 0,
        }
    }

    #[must_use]
    pub fn state_digest(&mut self) -> String {
        if self.digest_stale {
            self.running_digest = self.state_digest_full_scan();
            self.digest_stale = false;
        }
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            self.running_digest,
            self.state_digest_full_scan(),
            "running digest drifted from full scan"
        );
        format!("{:016x}", self.running_digest)
    }

    pub fn store_sorted_set(&mut self, dest: &[u8], members: HashMap<Vec<u8>, f64>, now_ms: u64) {
        self.internal_entries_remove(dest);
        self.stream_groups.remove(dest);
        self.stream_last_ids.remove(dest);

        if members.is_empty() {
            self.dirty = self.dirty.saturating_add(1);
            return;
        }

        let mut zs = SortedSet::new();
        for (m, s) in members {
            zs.insert(m, s);
        }
        self.internal_entries_insert(
            dest.to_vec(),
            Entry::new(Value::SortedSet(zs), None, now_ms),
        );
        self.dirty = self.dirty.saturating_add(1);
    }

    pub fn memory_usage_for_key(&mut self, key: &[u8], now_ms: u64) -> Option<usize> {
        self.drop_if_expired(key, now_ms);
        self.entries
            .get(key)
            .map(|entry| estimate_entry_memory_usage_bytes(key, entry))
    }

    pub fn request_debug_reload(&mut self) {
        self.debug_reload_requested = true;
    }

    #[must_use]
    pub fn take_debug_reload_requested(&mut self) -> bool {
        std::mem::take(&mut self.debug_reload_requested)
    }

    pub fn request_bgrewriteaof(&mut self) {
        self.bgrewriteaof_requested = true;
    }

    #[must_use]
    pub fn take_bgrewriteaof_requested(&mut self) -> bool {
        std::mem::take(&mut self.bgrewriteaof_requested)
    }

    pub fn request_acl_log_event(&mut self, event: PendingAclLogEvent) {
        self.pending_acl_log_events.push(event);
    }

    #[must_use]
    pub fn drain_pending_acl_log_events(&mut self) -> Vec<PendingAclLogEvent> {
        std::mem::take(&mut self.pending_acl_log_events)
    }

    /// Total live keys across every database. Used by the MEMORY
    /// STATS reply (`keys.count` field). (br-frankenredis-s14v)
    pub fn total_keys_across_dbs(&self) -> usize {
        self.entries.len()
    }

    pub fn estimate_memory_usage_bytes(&self) -> usize {
        let cached_bytes = self.cached_memory_usage_bytes.get();
        let cached_dirty = self.cached_memory_usage_dirty.get();

        let mutations = self
            .dirty
            .saturating_add(self.stat_evicted_keys)
            .saturating_add(self.stat_expired_keys);

        // Return cached value if we haven't mutated much since last calculation.
        // We recompute roughly every 64 mutations to amortize O(N) cost while staying accurate.
        if cached_bytes > 0 && mutations.saturating_sub(cached_dirty) < 64 {
            return cached_bytes;
        }

        let usage = self
            .entries
            .iter()
            .map(|(key, entry)| estimate_entry_memory_usage_bytes(key, entry))
            .sum();

        self.cached_memory_usage_bytes.set(usage);
        self.cached_memory_usage_dirty.set(mutations);
        usage
    }

    fn select_eviction_candidate(&mut self, _now_ms: u64) -> Option<Vec<u8>> {
        match self.maxmemory_policy {
            MaxmemoryPolicy::Noeviction => None,

            MaxmemoryPolicy::AllkeysLru | MaxmemoryPolicy::AllkeysLfu => {
                // Pick the key with the smallest last_access_ms (least recently used).
                // LFU is approximated as LRU since we don't track access frequency.
                let mut best_key: Option<Vec<u8>> = None;
                let mut best_access: u64 = u64::MAX;
                for (key, entry) in &self.entries {
                    if entry.last_access_ms < best_access {
                        best_access = entry.last_access_ms;
                        best_key = Some(key.clone());
                    }
                }
                best_key
            }

            MaxmemoryPolicy::VolatileLru | MaxmemoryPolicy::VolatileLfu => {
                // Pick the volatile key (has expiry) with the smallest last_access_ms.
                let mut best_key: Option<Vec<u8>> = None;
                let mut best_access: u64 = u64::MAX;
                for (key, entry) in &self.entries {
                    if entry.expires_at_ms.is_some() && entry.last_access_ms < best_access {
                        best_access = entry.last_access_ms;
                        best_key = Some(key.clone());
                    }
                }
                best_key
            }

            MaxmemoryPolicy::VolatileTtl => {
                // Pick the volatile key with the smallest expires_at_ms (soonest to expire).
                let mut best_key: Option<Vec<u8>> = None;
                let mut best_ttl: u64 = u64::MAX;
                for (key, entry) in &self.entries {
                    if let Some(exp) = entry.expires_at_ms
                        && exp < best_ttl
                    {
                        best_ttl = exp;
                        best_key = Some(key.clone());
                    }
                }
                best_key
            }

            MaxmemoryPolicy::AllkeysRandom => {
                let n = self.entries.len();
                if n == 0 {
                    return None;
                }
                let rand_val = self.next_rand();
                let idx = rand_val as usize % n;
                self.entries.keys().nth(idx).cloned()
            }

            MaxmemoryPolicy::VolatileRandom => {
                let n = self
                    .entries
                    .values()
                    .filter(|e| e.expires_at_ms.is_some())
                    .count();
                if n == 0 {
                    return None;
                }
                let rand_val = self.next_rand();
                let idx = rand_val as usize % n;
                self.entries
                    .iter()
                    .filter(|(_, e)| e.expires_at_ms.is_some())
                    .nth(idx)
                    .map(|(k, _)| k.clone())
            }
        }
    }

    /// Load a script into the cache, returning its SHA1 hex digest.
    pub fn script_load(&mut self, script: &[u8]) -> String {
        let sha1_hex = sha1_hex(script);
        self.script_cache.insert(sha1_hex.clone(), script.to_vec());
        self.dirty = self.dirty.saturating_add(1);
        sha1_hex
    }

    /// Check if scripts exist in the cache by SHA1.
    pub fn script_exists(&self, sha1s: &[&[u8]]) -> Vec<bool> {
        sha1s
            .iter()
            .map(|sha1| {
                let hex = String::from_utf8_lossy(sha1).to_ascii_lowercase();
                self.script_cache.contains_key(&hex)
            })
            .collect()
    }

    /// Flush the script cache.
    pub fn script_flush(&mut self) {
        self.script_cache.clear();
        self.dirty = self.dirty.saturating_add(1);
    }

    /// Look up a script body by SHA1 hex.
    pub fn script_get(&self, sha1: &[u8]) -> Option<&[u8]> {
        let hex = String::from_utf8_lossy(sha1).to_ascii_lowercase();
        self.script_cache.get(&hex).map(Vec::as_slice)
    }

    // ── Function library management ─────────────────────────────────

    /// Load a function library. Parses the library code to extract metadata.
    /// Returns the library name on success.
    pub fn function_load(&mut self, code: &[u8], replace: bool) -> Result<String, StoreError> {
        // Parse the library header: #!<engine> name=<name>
        let code_str = std::str::from_utf8(code).map_err(|_| StoreError::WrongType)?;

        let first_line = code_str.lines().next().unwrap_or("");
        if !first_line.starts_with("#!") {
            return Err(StoreError::GenericError(
                "ERR Missing library metadata".to_string(),
            ));
        }

        let header = &first_line[2..].trim();
        let mut engine = String::new();
        let mut lib_name = String::new();

        for (i, part) in header.split_whitespace().enumerate() {
            if i == 0 {
                engine = part.to_ascii_uppercase();
            } else if let Some(name) = part.strip_prefix("name=") {
                lib_name = name.to_string();
            }
        }

        // Upstream functions.c validates the meta header in this
        // order:
        //   1. engine token present (else "Missing library metadata")
        //   2. engine name registered (else "Engine 'X' not found")
        //   3. lib name present (else "Library name was not given")
        //   4. lib name charset (else "Library names can only ...")
        // Match that ordering so peers and conformance fixtures see
        // the same error precedence. (br-frankenredis-r85v)
        if engine.is_empty() {
            return Err(StoreError::GenericError(
                "ERR Missing library metadata".to_string(),
            ));
        }
        if engine != "LUA" {
            return Err(StoreError::GenericError(format!(
                "ERR Engine '{engine}' not found"
            )));
        }
        if lib_name.is_empty() {
            return Err(StoreError::GenericError(
                "ERR Library name was not given".to_string(),
            ));
        }
        if !lib_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(StoreError::GenericError(
                "ERR Library names can only contain letters, numbers, \
                 or underscores(_) and must be at least one character \
                 long"
                    .to_string(),
            ));
        }

        if !replace && self.function_libraries.contains_key(&lib_name) {
            return Err(StoreError::GenericError(format!(
                "ERR Library '{lib_name}' already exists"
            )));
        }

        // Parse function registrations from the code, capturing the
        // optional `description` field from the table-form
        // invocation. Upstream functions.c::functionLibCreateFunction
        // applies functionsVerifyName to each function name and
        // rejects empty / non-[A-Za-z0-9_] names. Match that gate
        // so a library code chunk with a malformed register_function
        // call surfaces the upstream error instead of recording an
        // invalid function entry. (br-frankenredis-r85v)
        let mut functions = Vec::new();
        for line in code_str.lines() {
            let trimmed = line.trim();
            if trimmed.contains("register_function")
                && let Some((name, description)) = extract_function_metadata(trimmed)
            {
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(StoreError::GenericError(
                        "ERR Library names can only contain letters, numbers, \
                         or underscores(_) and must be at least one character \
                         long"
                            .to_string(),
                    ));
                }
                functions.push(FunctionEntry {
                    name,
                    description,
                    flags: Vec::new(),
                });
            }
        }
        functions.sort_by(|a, b| b.name.cmp(&a.name));

        let library = FunctionLibrary {
            name: lib_name.clone(),
            engine,
            description: None,
            code: code.to_vec(),
            functions,
        };

        self.function_libraries.insert(lib_name.clone(), library);
        self.dirty = self.dirty.saturating_add(1);
        Ok(lib_name)
    }

    /// Delete a function library by name.
    pub fn function_delete(&mut self, name: &str) -> Result<(), StoreError> {
        if self.function_libraries.remove(name).is_none() {
            return Err(StoreError::GenericError(
                "ERR Library not found".to_string(),
            ));
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(())
    }

    /// List function libraries, optionally filtered by pattern.
    pub fn function_list(&self, pattern: Option<&str>) -> Vec<&FunctionLibrary> {
        let mut libs: Vec<&FunctionLibrary> = self
            .function_libraries
            .values()
            .filter(|lib| {
                if let Some(pat) = pattern {
                    glob_match_str(pat, &lib.name)
                } else {
                    true
                }
            })
            .collect();
        libs.sort_by(|a, b| a.name.cmp(&b.name));
        libs
    }

    /// Flush all function libraries.
    pub fn function_flush(&mut self) {
        self.function_libraries.clear();
        self.dirty = self.dirty.saturating_add(1);
    }

    /// Upstream functions.c::functionDumpCommand pins payloads to
    /// `RDB_VERSION` (currently 11 for Redis 7.2.x). Our envelope
    /// matches that shape: `[body … | u16 LE version | u64 LE crc64]`.
    /// (br-frankenredis-r83v)
    const FUNCTION_DUMP_RDB_VERSION: u16 = 11;

    /// Dump all function libraries as a serialized blob, wrapped in
    /// the Redis 7+ envelope (body + 2-byte version + 8-byte CRC64).
    ///
    /// Body uses the exact upstream `rdbSaveFunctions` shape: one
    /// `RDB_OPCODE_FUNCTION2` byte followed by an RDB raw string
    /// containing the library code, repeated once per library. The
    /// function metadata is reconstructed by reloading that code.
    /// (br-frankenredis-r85v)
    pub fn function_dump(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for lib in self.function_list(None) {
            buf.push(RDB_OPCODE_FUNCTION2);
            encode_rdb_string(&mut buf, &lib.code);
        }
        buf.extend_from_slice(&Self::FUNCTION_DUMP_RDB_VERSION.to_le_bytes());
        let crc = fr_persist::crc64_redis(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Restore function libraries from a serialized blob.
    pub fn function_restore(&mut self, data: &[u8], policy: &str) -> Result<(), StoreError> {
        let flush = policy.eq_ignore_ascii_case("FLUSH");
        let replace = policy.eq_ignore_ascii_case("REPLACE") || flush;
        let append = policy.eq_ignore_ascii_case("APPEND") || policy.is_empty();

        // Reject unknown policies
        if !flush && !replace && !append {
            return Err(StoreError::GenericError(
                "ERR Wrong restore policy given, value should be either FLUSH, APPEND or REPLACE."
                    .to_string(),
            ));
        }

        // Strip + validate the upstream envelope footer (2-byte
        // version + 8-byte CRC64) before parsing the body. The
        // empty-libraries marker is itself a valid envelope so it
        // round-trips through the same path. (br-frankenredis-r83v)
        // Upstream cluster.c::verifyDumpPayload emits a single error
        // "DUMP payload version or checksum are wrong" for any of:
        // too-short envelope, version-newer-than-RDB_VERSION, or
        // CRC64 mismatch. (br-frankenredis-ngn0)
        const FOOTER_LEN: usize = 10;
        if data.len() < FOOTER_LEN {
            return Err(StoreError::GenericError(
                "ERR DUMP payload version or checksum are wrong".to_string(),
            ));
        }
        let footer_offset = data.len() - FOOTER_LEN;
        // Upstream cluster.c::verifyDumpPayload rejects a payload
        // whose version is newer than the local RDB_VERSION. We
        // mirror that so a future-version FUNCTION DUMP can't
        // sneak past the body parser. (br-frankenredis-r83v)
        let stored_version = u16::from_le_bytes([data[footer_offset], data[footer_offset + 1]]);
        if stored_version > Self::FUNCTION_DUMP_RDB_VERSION {
            return Err(StoreError::GenericError(
                "ERR DUMP payload version or checksum are wrong".to_string(),
            ));
        }
        let stored_crc = u64::from_le_bytes(
            data[footer_offset + 2..]
                .try_into()
                .map_err(|_| StoreError::GenericError("ERR Invalid dump data".to_string()))?,
        );
        let computed_crc = fr_persist::crc64_redis(&data[..footer_offset + 2]);
        if stored_crc != computed_crc {
            return Err(StoreError::GenericError(
                "ERR DUMP payload version or checksum are wrong".to_string(),
            ));
        }
        let data = &data[..footer_offset];
        let mut pos = 0;
        let mut incoming = Store::new();
        while pos < data.len() {
            let opcode = data[pos];
            pos += 1;
            if opcode == 246 {
                return Err(StoreError::GenericError(
                    "ERR Pre-GA function format not supported".to_string(),
                ));
            }
            if opcode != RDB_OPCODE_FUNCTION2 {
                return Err(StoreError::GenericError(
                    "ERR given type is not a function".to_string(),
                ));
            }
            let (code, consumed) = decode_rdb_string(data, pos, data.len()).map_err(|err| {
                if matches!(err, StoreError::InvalidDumpPayload) {
                    StoreError::GenericError("ERR Invalid dump data".to_string())
                } else {
                    err
                }
            })?;
            pos += consumed;
            incoming.function_load(&code, false)?;
        }

        let restored_libraries = incoming.function_libraries;
        if append {
            for name in restored_libraries.keys() {
                if self.function_libraries.contains_key(name) {
                    return Err(StoreError::GenericError(format!(
                        "ERR Library '{name}' already exists"
                    )));
                }
            }
        }

        if flush {
            self.function_libraries.clear();
        }
        self.function_libraries.extend(restored_libraries);
        self.dirty = self.dirty.saturating_add(1);
        Ok(())
    }

    /// Look up a function by name across all libraries. Returns (library, function_entry).
    pub fn function_get(&self, func_name: &str) -> Option<(&FunctionLibrary, &FunctionEntry)> {
        for lib in self.function_libraries.values() {
            for func in &lib.functions {
                if func.name.eq_ignore_ascii_case(func_name) {
                    return Some((lib, func));
                }
            }
        }
        None
    }

    /// Get FUNCTION STATS data.
    pub fn function_stats(&self) -> (usize, usize) {
        let lib_count = self.function_libraries.len();
        let func_count: usize = self
            .function_libraries
            .values()
            .map(|lib| lib.functions.len())
            .sum();
        (lib_count, func_count)
    }

    /// Subscribe to a channel. Returns the total subscription count.
    pub fn subscribe(&mut self, channel: Vec<u8>) -> usize {
        self.subscribed_channels.insert(channel);
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }

    /// Unsubscribe from a channel. Returns the total subscription count.
    pub fn unsubscribe(&mut self, channel: &[u8]) -> usize {
        self.subscribed_channels.remove(channel);
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }

    /// Subscribe to a pattern. Returns the total subscription count.
    pub fn psubscribe(&mut self, pattern: Vec<u8>) -> usize {
        self.subscribed_patterns.insert(pattern);
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }

    /// Unsubscribe from a pattern. Returns the total subscription count.
    pub fn punsubscribe(&mut self, pattern: &[u8]) -> usize {
        self.subscribed_patterns.remove(pattern);
        self.subscribed_channels.len() + self.subscribed_patterns.len()
    }

    /// Publish a message to a channel. Returns number of subscribers that received it.
    /// In Redis, each matching direct subscription gets a `message` push,
    /// and each matching pattern subscription gets a separate `pmessage` push.
    pub fn publish(&mut self, channel: &[u8], message: &[u8]) -> usize {
        let mut receivers = 0;
        // Check direct channel subscriptions
        if self.subscribed_channels.contains(channel) {
            self.pubsub_pending.push(PubSubMessage::Message {
                channel: channel.to_vec(),
                data: message.to_vec(),
            });
            receivers += 1;
        }
        // Check pattern subscriptions — each matching pattern produces a separate pmessage
        let matching_patterns: Vec<Vec<u8>> = self
            .subscribed_patterns
            .iter()
            .filter(|pattern| glob_match(pattern, channel))
            .cloned()
            .collect();
        for pattern in matching_patterns {
            self.pubsub_pending.push(PubSubMessage::PMessage {
                pattern,
                channel: channel.to_vec(),
                data: message.to_vec(),
            });
            receivers += 1;
        }
        receivers
    }

    /// Publish a message to a shard channel. Returns number of shard subscribers
    /// that received it.
    pub fn spublish(&mut self, channel: &[u8], message: &[u8]) -> usize {
        if self.subscribed_shard_channels.contains(channel) {
            self.pubsub_pending.push(PubSubMessage::SMessage {
                channel: channel.to_vec(),
                data: message.to_vec(),
            });
            1
        } else {
            0
        }
    }

    /// Drain all pending Pub/Sub messages.
    pub fn drain_pending_pubsub(&mut self) -> Vec<PubSubMessage> {
        std::mem::take(&mut self.pubsub_pending)
    }

    /// Queue a keyspace notification event for delivery via pub/sub.
    /// `event_type` is the notification class (NOTIFY_STRING, NOTIFY_LIST, etc.).
    /// `event` is the event name (e.g., "set", "del", "expire").
    /// `key` is the affected key.
    /// `db` is the database index.
    pub fn notify_keyspace_event(&mut self, event_type: u32, event: &str, key: &[u8], db: usize) {
        let flags = self.notify_keyspace_events;
        if flags == 0 || (flags & event_type) == 0 {
            return;
        }

        let event_bytes = event.as_bytes();

        // __keyspace@<db>__:<key> → event name
        if flags & NOTIFY_KEYSPACE != 0 {
            let channel = format!("__keyspace@{db}__:");
            let mut chan = channel.into_bytes();
            chan.extend_from_slice(key);
            self.keyspace_notifications
                .push((chan, event_bytes.to_vec()));
        }

        // __keyevent@<db>__:<event> → key name
        if flags & NOTIFY_KEYEVENT != 0 {
            let channel = format!("__keyevent@{db}__:{event}");
            self.keyspace_notifications
                .push((channel.into_bytes(), key.to_vec()));
        }
    }

    /// Drain pending keyspace notifications.
    pub fn drain_keyspace_notifications(&mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        std::mem::take(&mut self.keyspace_notifications)
    }

    /// Return the number of subscribed channels.
    pub fn pubsub_numsub_count(&self, channel: &[u8]) -> usize {
        usize::from(self.subscribed_channels.contains(channel))
    }

    /// Return the number of pattern subscriptions.
    pub fn pubsub_numpat(&self) -> usize {
        self.subscribed_patterns.len()
    }

    /// Return the number of shard subscribers for a shard channel.
    pub fn pubsub_shardnumsub_count(&self, channel: &[u8]) -> usize {
        usize::from(self.subscribed_shard_channels.contains(channel))
    }

    /// Subscribe to a shard channel. Returns the total shard subscription count.
    pub fn ssubscribe(&mut self, channel: Vec<u8>) -> usize {
        self.subscribed_shard_channels.insert(channel);
        self.subscribed_shard_channels.len()
    }

    /// Unsubscribe from a shard channel. Returns the total shard subscription count.
    pub fn sunsubscribe(&mut self, channel: &[u8]) -> usize {
        self.subscribed_shard_channels.remove(channel);
        self.subscribed_shard_channels.len()
    }

    /// Return all subscribed channel names.
    pub fn pubsub_channels(&self) -> Vec<Vec<u8>> {
        self.subscribed_channels.iter().cloned().collect()
    }

    fn dump_stream_consumer_groups(&self, key: &[u8]) -> Vec<fr_persist::RdbStreamConsumerGroup> {
        self.stream_groups
            .get(key)
            .map(|groups| {
                groups
                    .iter()
                    .map(|(name, group)| fr_persist::RdbStreamConsumerGroup {
                        name: name.clone(),
                        last_delivered_id_ms: group.last_delivered_id.0,
                        last_delivered_id_seq: group.last_delivered_id.1,
                        consumers: group.consumers.iter().cloned().collect(),
                        pending: group
                            .pending
                            .iter()
                            .map(|((entry_id_ms, entry_id_seq), pending)| {
                                fr_persist::RdbStreamPendingEntry {
                                    entry_id_ms: *entry_id_ms,
                                    entry_id_seq: *entry_id_seq,
                                    consumer: pending.consumer.clone(),
                                    deliveries: pending.deliveries,
                                    last_delivered_ms: pending.last_delivered_ms,
                                }
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Serialize a key's value for DUMP. Returns None if key doesn't exist.
    /// Format: [type_byte][payload][2-byte RDB version][8-byte CRC64].
    pub fn dump_key(&mut self, key: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        self.drop_if_expired(key, now_ms);
        let entry = self.entries.get(key)?;
        let mut buf = Vec::new();
        match &entry.value {
            Value::String(v) => {
                buf.push(RDB_TYPE_STRING);
                encode_rdb_string(&mut buf, v);
            }
            Value::List(l) => {
                buf.push(RDB_TYPE_LIST_QUICKLIST_2);
                encode_length(&mut buf, 1);
                encode_length(&mut buf, 2);
                let items: Vec<&[u8]> = l.iter().map(Vec::as_slice).collect();
                encode_dump_bulk(&mut buf, &encode_listpack_strings(&items)?);
            }
            Value::Set(s) => {
                let integer_members: Option<Vec<i64>> =
                    s.iter().map(|member| parse_i64(member).ok()).collect();
                if s.len() <= self.set_max_intset_entries {
                    if let Some(mut integers) = integer_members {
                        integers.sort_unstable();
                        buf.push(RDB_TYPE_SET_INTSET);
                        encode_dump_bulk(&mut buf, &encode_intset(&integers)?);
                    } else if s.len() <= self.set_max_listpack_entries
                        && s.iter().all(|member| member.len() <= 64)
                    {
                        buf.push(RDB_TYPE_SET_LISTPACK);
                        let members: Vec<&[u8]> = s.iter().map(Vec::as_slice).collect();
                        encode_dump_bulk(&mut buf, &encode_listpack_strings(&members)?);
                    } else {
                        buf.push(RDB_TYPE_SET);
                        encode_length(&mut buf, s.len());
                        for member in s {
                            encode_dump_bulk(&mut buf, member);
                        }
                    }
                } else if s.len() <= self.set_max_listpack_entries
                    && s.iter().all(|member| member.len() <= 64)
                {
                    buf.push(RDB_TYPE_SET_LISTPACK);
                    let members: Vec<&[u8]> = s.iter().map(Vec::as_slice).collect();
                    encode_dump_bulk(&mut buf, &encode_listpack_strings(&members)?);
                } else {
                    buf.push(RDB_TYPE_SET);
                    encode_length(&mut buf, s.len());
                    for member in s {
                        encode_dump_bulk(&mut buf, member);
                    }
                }
            }
            Value::Hash(h) => {
                if h.len() <= self.hash_max_listpack_entries
                    && h.iter().all(|(field, value)| {
                        field.len() <= self.hash_max_listpack_value
                            && value.len() <= self.hash_max_listpack_value
                    })
                {
                    buf.push(RDB_TYPE_HASH_LISTPACK);
                    let mut pairs = Vec::with_capacity(h.len() * 2);
                    for (field, value) in h {
                        pairs.push(field.as_slice());
                        pairs.push(value.as_slice());
                    }
                    encode_dump_bulk(&mut buf, &encode_listpack_strings(&pairs)?);
                } else {
                    buf.push(RDB_TYPE_HASH);
                    encode_length(&mut buf, h.len());
                    for (field, value) in h {
                        encode_dump_bulk(&mut buf, field);
                        encode_dump_bulk(&mut buf, value);
                    }
                }
            }
            Value::SortedSet(zs) => {
                if zs.len() <= self.zset_max_listpack_entries
                    && zs
                        .keys()
                        .all(|member| member.len() <= self.zset_max_listpack_value)
                {
                    buf.push(RDB_TYPE_ZSET_LISTPACK);
                    let mut pairs = Vec::with_capacity(zs.len() * 2);
                    for (member, score) in zs.iter_asc() {
                        pairs.push(member.clone());
                        pairs.push(score.to_string().into_bytes());
                    }
                    let pair_refs: Vec<&[u8]> = pairs.iter().map(Vec::as_slice).collect();
                    encode_dump_bulk(&mut buf, &encode_listpack_strings(&pair_refs)?);
                } else {
                    buf.push(RDB_TYPE_ZSET_2);
                    encode_length(&mut buf, zs.len());
                    for (member, score) in zs.iter_asc() {
                        encode_dump_bulk(&mut buf, member);
                        buf.extend_from_slice(&score.to_le_bytes());
                    }
                }
            }
            Value::Stream(entries) => {
                let stream_entries = dump_stream_entries(entries);
                let watermark = self
                    .stream_last_ids
                    .get(key)
                    .copied()
                    .or_else(|| entries.keys().next_back().copied())
                    .or(Some((0, 0)));
                let groups = self.dump_stream_consumer_groups(key);
                let payload = fr_persist::encode_upstream_stream_listpacks3_payload(
                    &stream_entries,
                    watermark,
                    &groups,
                )?;
                buf.push(RDB_TYPE_STREAM_LISTPACKS_3);
                buf.extend_from_slice(&payload);
            }
        }
        // Append Redis DUMP footer: 2-byte little-endian RDB version, then CRC64.
        buf.extend_from_slice(&RDB_DUMP_VERSION.to_le_bytes());
        // Compute and append CRC64 over all preceding bytes
        let crc = fr_persist::crc64_redis(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        Some(buf)
    }

    /// Restore a key from a DUMP payload. Returns Ok(()) on success.
    pub fn restore_key(
        &mut self,
        key: &[u8],
        ttl_ms: u64,
        payload: &[u8],
        replace: bool,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if payload.len() < DUMP_TRAILER_LEN + 1 {
            return Err(StoreError::InvalidDumpPayload);
        }
        let version_offset = payload.len() - DUMP_TRAILER_LEN;
        let version = u16::from_le_bytes(
            payload[version_offset..version_offset + DUMP_VERSION_LEN]
                .try_into()
                .map_err(|_| StoreError::InvalidDumpPayload)?,
        );
        if version > RDB_DUMP_VERSION {
            return Err(StoreError::InvalidDumpPayload);
        }

        // Validate CRC64: last 8 bytes are CRC over everything before them
        let crc_offset = payload.len() - DUMP_CRC64_LEN;
        let stored_crc = u64::from_le_bytes(
            payload[crc_offset..crc_offset + DUMP_CRC64_LEN]
                .try_into()
                .map_err(|_| StoreError::InvalidDumpPayload)?,
        );
        let computed_crc = fr_persist::crc64_redis(&payload[..crc_offset]);
        if stored_crc != computed_crc {
            return Err(StoreError::InvalidDumpPayload);
        }
        // Check if key exists and replace flag
        self.drop_if_expired(key, now_ms);
        if !replace && self.entries.contains_key(key) {
            return Err(StoreError::BusyKey);
        }
        let type_byte = payload[0];
        let mut cursor = 1;
        // Data boundary: exclude trailer (2-byte version + 8-byte CRC64).
        let data_end = payload.len() - DUMP_TRAILER_LEN;
        let mut restored_stream_last_id = None;
        let mut restored_stream_groups = None;
        let value = match type_byte {
            RDB_TYPE_STRING => {
                let (v, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                cursor += consumed;
                Value::String(v)
            }
            RDB_TYPE_LIST => {
                // List
                let (count, consumed) = decode_length(payload, cursor)?;
                cursor += consumed;
                let mut list = VecDeque::with_capacity(count.min(1024));
                for _ in 0..count {
                    let (item, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += consumed;
                    list.push_back(item);
                }
                Value::List(list)
            }
            RDB_TYPE_SET => {
                // Set
                let (count, consumed) = decode_length(payload, cursor)?;
                cursor += consumed;
                let mut set = BTreeSet::new();
                for _ in 0..count {
                    let (member, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += consumed;
                    set.insert(member);
                }
                Value::Set(set)
            }
            RDB_TYPE_HASH => {
                // Hash
                let (count, consumed) = decode_length(payload, cursor)?;
                cursor += consumed;
                let mut hash = BTreeMap::new();
                for _ in 0..count {
                    let (field, fc) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += fc;
                    let (value, vc) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += vc;
                    hash.insert(field, value);
                }
                Value::Hash(hash)
            }
            RDB_TYPE_ZSET_2 => {
                // Sorted set
                let (count, consumed) = decode_length(payload, cursor)?;
                cursor += consumed;
                let mut zs = SortedSet::new();
                for _ in 0..count {
                    let (member, mc) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += mc;
                    if cursor + 8 > data_end {
                        return Err(StoreError::InvalidDumpPayload);
                    }
                    let score = f64::from_le_bytes(
                        payload[cursor..cursor + 8]
                            .try_into()
                            .map_err(|_| StoreError::InvalidDumpPayload)?,
                    );
                    cursor += 8;
                    if score.is_nan() {
                        return Err(StoreError::InvalidDumpPayload);
                    }
                    zs.insert(member, score);
                }
                Value::SortedSet(zs)
            }
            RDB_TYPE_STREAM_LISTPACKS
            | RDB_TYPE_STREAM_LISTPACKS_2
            | RDB_TYPE_STREAM_LISTPACKS_3 => {
                let (stream_value, consumed) = fr_persist::decode_upstream_stream_payload(
                    type_byte,
                    &payload[cursor..data_end],
                )
                .ok_or(StoreError::InvalidDumpPayload)?;
                cursor += consumed;
                let fr_persist::RdbValue::Stream(stream_entries, watermark, groups, _) =
                    stream_value
                else {
                    return Err(StoreError::InvalidDumpPayload);
                };
                let mut entries = BTreeMap::new();
                for (ms, seq, fields) in stream_entries {
                    entries.insert((ms, seq), fields);
                }
                restored_stream_last_id = watermark.or_else(|| entries.keys().next_back().copied());
                restored_stream_groups = Some(restore_stream_groups(groups));
                Value::Stream(entries)
            }
            RDB_TYPE_LIST_QUICKLIST_2 => {
                let (node_count, consumed) = decode_length(payload, cursor)?;
                cursor += consumed;
                let mut list = VecDeque::new();
                for _ in 0..node_count {
                    let (container, consumed) = decode_length(payload, cursor)?;
                    cursor += consumed;
                    if container != 2 {
                        return Err(StoreError::InvalidDumpPayload);
                    }
                    let (listpack, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                    cursor += consumed;
                    for item in decode_listpack_strings(&listpack)? {
                        list.push_back(item);
                    }
                }
                Value::List(list)
            }
            RDB_TYPE_HASH_LISTPACK => {
                let (listpack, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                cursor += consumed;
                let entries = decode_listpack_strings(&listpack)?;
                let mut chunks = entries.chunks_exact(2);
                if !chunks.remainder().is_empty() {
                    return Err(StoreError::InvalidDumpPayload);
                }
                let mut hash = BTreeMap::new();
                for pair in &mut chunks {
                    hash.insert(pair[0].clone(), pair[1].clone());
                }
                Value::Hash(hash)
            }
            RDB_TYPE_SET_INTSET => {
                let (intset, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                cursor += consumed;
                let mut set = BTreeSet::new();
                for member in decode_intset_members(&intset)? {
                    set.insert(member);
                }
                Value::Set(set)
            }
            RDB_TYPE_SET_LISTPACK => {
                let (listpack, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                cursor += consumed;
                let members = decode_listpack_strings(&listpack)?;
                let mut set = BTreeSet::new();
                for member in members {
                    set.insert(member);
                }
                Value::Set(set)
            }
            RDB_TYPE_ZSET_LISTPACK => {
                let (listpack, consumed) = decode_rdb_string(payload, cursor, data_end)?;
                cursor += consumed;
                let entries = decode_listpack_strings(&listpack)?;
                let mut chunks = entries.chunks_exact(2);
                if !chunks.remainder().is_empty() {
                    return Err(StoreError::InvalidDumpPayload);
                }
                let mut zs = SortedSet::new();
                for pair in &mut chunks {
                    let score = std::str::from_utf8(&pair[1])
                        .ok()
                        .and_then(|raw| raw.parse::<f64>().ok())
                        .ok_or(StoreError::InvalidDumpPayload)?;
                    zs.insert(pair[0].clone(), score);
                }
                Value::SortedSet(zs)
            }
            _ => return Err(StoreError::InvalidDumpPayload),
        };
        if cursor != data_end {
            return Err(StoreError::InvalidDumpPayload);
        }
        let expires_at_ms = if ttl_ms > 0 {
            Some(now_ms.saturating_add(ttl_ms))
        } else {
            None
        };
        self.internal_entries_remove(key);
        self.stream_groups.remove(key);
        self.stream_last_ids.remove(key);
        self.internal_entries_insert(key.to_vec(), Entry::new(value, expires_at_ms, now_ms));
        if let Some(last_id) = restored_stream_last_id {
            self.stream_last_ids.insert(key.to_vec(), last_id);
        }
        if let Some(groups) = restored_stream_groups
            && !groups.is_empty()
        {
            self.stream_groups.insert(key.to_vec(), groups);
        }
        self.dirty = self.dirty.saturating_add(1);
        Ok(())
    }

    /// Generate AOF-compatible command sequences that reconstruct the entire store.
    ///
    /// Returns a list of command argv vectors. Loaded function libraries are serialized
    /// as deterministic FUNCTION LOAD REPLACE commands first. Non-expired entries are
    /// then serialized as the appropriate write command (SET, HSET, RPUSH, SADD, ZADD,
    /// XADD), followed by PEXPIREAT if the key has an expiry. Expired entries are skipped.
    ///
    /// This is the core of AOF rewrite: the output can be wrapped in `AofRecord`
    /// Return all key names in the store (sorted for determinism).
    #[must_use]
    pub fn all_keys(&self) -> Vec<Vec<u8>> {
        self.ordered_keys.iter().cloned().collect()
    }

    /// Drop a key if it has expired. Public wrapper for RDB/snapshot use.
    pub fn expire_key_if_stale(&mut self, key: &[u8], now_ms: u64) {
        self.drop_if_expired(key, now_ms);
    }

    /// Get a reference to an entry's value and expiry for RDB serialization.
    /// Returns None if the key doesn't exist.
    #[must_use]
    pub fn get_value_and_expiry(&self, key: &[u8]) -> Option<(&Value, Option<u64>)> {
        self.entries
            .get(key)
            .map(|entry| (&entry.value, entry.expires_at_ms))
    }

    /// and encoded/replayed to reconstruct the database from scratch.
    #[must_use]
    pub fn to_aof_commands(&mut self, now_ms: u64) -> Vec<Vec<Vec<u8>>> {
        // Expire stale keys first so they aren't serialized.
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut commands = Vec::new();

        for library in self.function_list(None) {
            commands.push(vec![
                b"FUNCTION".to_vec(),
                b"LOAD".to_vec(),
                b"REPLACE".to_vec(),
                library.code.clone(),
            ]);
        }

        // Snapshot the remaining keys (sorted for deterministic output).
        let mut keys: Vec<(usize, Vec<u8>, Vec<u8>)> = self
            .entries
            .keys()
            .map(|physical| {
                let (db, logical) = decode_db_key(physical).unwrap_or((0, physical.as_slice()));
                (db, logical.to_vec(), physical.clone())
            })
            .collect();
        keys.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        let mut current_db = 0usize;

        for (db, logical_key, physical_key) in keys {
            let Some(entry) = self.entries.get(&physical_key) else {
                continue;
            };

            if db != current_db {
                commands.push(vec![b"SELECT".to_vec(), db.to_string().into_bytes()]);
                current_db = db;
            }

            match &entry.value {
                Value::String(v) => {
                    commands.push(vec![b"SET".to_vec(), logical_key.clone(), v.clone()]);
                }
                Value::Hash(h) => {
                    if !h.is_empty() {
                        let mut argv = vec![b"HSET".to_vec(), logical_key.clone()];
                        // Sort fields for deterministic output.
                        let mut fields: Vec<(&Vec<u8>, &Vec<u8>)> = h.iter().collect();
                        fields.sort_by(|a, b| a.0.cmp(b.0));
                        for (field, value) in fields {
                            argv.push(field.clone());
                            argv.push(value.clone());
                        }
                        commands.push(argv);

                        // Redis 7.4 per-field TTLs: reconstruct each field's
                        // deadline via HPEXPIREAT (absolute ms) so replay
                        // recovers identical state regardless of now_ms at
                        // load time. (br-frankenredis-4bao)
                        let mut field_ttls: Vec<(Vec<u8>, u64)> = self
                            .hash_field_expires
                            .range((physical_key.clone(), Vec::new())..)
                            .take_while(|((k, _), _)| k == &physical_key)
                            .map(|((_, f), &at)| (f.clone(), at))
                            .collect();
                        field_ttls.sort_by(|a, b| a.0.cmp(&b.0));
                        for (field, expires_at_ms) in field_ttls {
                            commands.push(vec![
                                b"HPEXPIREAT".to_vec(),
                                logical_key.clone(),
                                expires_at_ms.to_string().into_bytes(),
                                b"FIELDS".to_vec(),
                                b"1".to_vec(),
                                field,
                            ]);
                        }
                    }
                }
                Value::List(l) => {
                    if !l.is_empty() {
                        let mut argv = vec![b"RPUSH".to_vec(), logical_key.clone()];
                        for item in l {
                            argv.push(item.clone());
                        }
                        commands.push(argv);
                    }
                }
                Value::Set(s) => {
                    if !s.is_empty() {
                        let mut argv = vec![b"SADD".to_vec(), logical_key.clone()];
                        // Sort members for deterministic output.
                        let mut members: Vec<&Vec<u8>> = s.iter().collect();
                        members.sort();
                        for member in members {
                            argv.push(member.clone());
                        }
                        commands.push(argv);
                    }
                }
                Value::SortedSet(zs) => {
                    if !zs.is_empty() {
                        let mut argv = vec![b"ZADD".to_vec(), logical_key.clone()];
                        // Sort by score then member for deterministic output.
                        let mut pairs: Vec<(&Vec<u8>, &f64)> = zs.iter().collect();
                        pairs.sort_by(|a, b| {
                            a.1.partial_cmp(b.1)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.0.cmp(b.0))
                        });
                        for (member, score) in pairs {
                            argv.push(score.to_string().into_bytes());
                            argv.push(member.clone());
                        }
                        commands.push(argv);
                    }
                }
                Value::Stream(entries) => {
                    // Each stream entry becomes a separate XADD command.
                    for ((ms, seq), fields) in entries {
                        let id = format!("{ms}-{seq}");
                        let mut argv = vec![b"XADD".to_vec(), logical_key.clone(), id.into_bytes()];
                        for (fname, fval) in fields {
                            argv.push(fname.clone());
                            argv.push(fval.clone());
                        }
                        commands.push(argv);
                    }

                    // Emit XSETID only if the high watermark exceeds the max entry
                    // (e.g., entries were deleted, or XSETID was explicitly called).
                    if let Some(&watermark) = self.stream_last_ids.get(&physical_key) {
                        let max_entry_id = entries.keys().last().copied();
                        if max_entry_id.is_none_or(|max| watermark > max) {
                            let (ms, seq) = watermark;
                            let id = format!("{ms}-{seq}");
                            commands.push(vec![
                                b"XSETID".to_vec(),
                                logical_key.clone(),
                                id.into_bytes(),
                            ]);
                        }
                    }

                    // Emit XGROUP CREATE for each consumer group.
                    if let Some(groups) = self.stream_groups.get(&physical_key) {
                        let mut group_names: Vec<&Vec<u8>> = groups.keys().collect();
                        group_names.sort();
                        for group_name in group_names {
                            let group = &groups[group_name];
                            let (ms, seq) = group.last_delivered_id;
                            let id = format!("{ms}-{seq}");
                            commands.push(vec![
                                b"XGROUP".to_vec(),
                                b"CREATE".to_vec(),
                                logical_key.clone(),
                                group_name.clone(),
                                id.into_bytes(),
                            ]);

                            for consumer in &group.consumers {
                                commands.push(vec![
                                    b"XGROUP".to_vec(),
                                    b"CREATECONSUMER".to_vec(),
                                    logical_key.clone(),
                                    group_name.clone(),
                                    consumer.clone(),
                                ]);
                            }

                            for ((pending_ms, pending_seq), pending_entry) in &group.pending {
                                let pending_id = format!("{pending_ms}-{pending_seq}");
                                commands.push(vec![
                                    b"XCLAIM".to_vec(),
                                    logical_key.clone(),
                                    group_name.clone(),
                                    pending_entry.consumer.clone(),
                                    b"0".to_vec(),
                                    pending_id.into_bytes(),
                                    b"TIME".to_vec(),
                                    pending_entry.last_delivered_ms.to_string().into_bytes(),
                                    b"RETRYCOUNT".to_vec(),
                                    pending_entry.deliveries.to_string().into_bytes(),
                                    b"FORCE".to_vec(),
                                ]);
                            }
                        }
                    }
                }
            }

            // Emit PEXPIREAT if the key has an expiry timestamp.
            if let Some(exp_ms) = entry.expires_at_ms {
                commands.push(vec![
                    b"PEXPIREAT".to_vec(),
                    logical_key.clone(),
                    exp_ms.to_string().into_bytes(),
                ]);
            }
        }

        commands
    }
}

/// CRC16-CCITT (poly 0x1021) helper retained for older internal fixtures.
#[allow(dead_code)]
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Encode a Redis RDB length.
fn encode_length(buf: &mut Vec<u8>, len: usize) {
    if len < 64 {
        buf.push(len as u8);
    } else if len < 16_384 {
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

fn encode_rdb_string(buf: &mut Vec<u8>, data: &[u8]) {
    if let Some(encoded) = encode_integer_rdb_string(data) {
        buf.extend_from_slice(&encoded);
    } else {
        encode_length(buf, data.len());
        buf.extend_from_slice(data);
    }
}

fn dump_stream_entries(entries: &StreamEntries) -> Vec<fr_persist::StreamEntry> {
    entries
        .iter()
        .map(|((ms, seq), fields)| (*ms, *seq, fields.clone()))
        .collect()
}

fn restore_stream_groups(groups: Vec<fr_persist::RdbStreamConsumerGroup>) -> StreamGroupState {
    groups
        .into_iter()
        .map(|group| {
            let pending = group
                .pending
                .into_iter()
                .map(|pending| {
                    (
                        (pending.entry_id_ms, pending.entry_id_seq),
                        StreamPendingEntry {
                            consumer: pending.consumer,
                            deliveries: pending.deliveries,
                            last_delivered_ms: pending.last_delivered_ms,
                        },
                    )
                })
                .collect();
            (
                group.name,
                StreamGroup {
                    last_delivered_id: (group.last_delivered_id_ms, group.last_delivered_id_seq),
                    consumers: group.consumers.into_iter().collect(),
                    pending,
                },
            )
        })
        .collect()
}

fn encode_integer_rdb_string(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() > 11 {
        return None;
    }

    let value = parse_i64(data).ok()?;
    if let Ok(value) = i8::try_from(value) {
        Some(vec![RDB_ENCVAL | RDB_ENC_INT8, value as u8])
    } else if let Ok(value) = i16::try_from(value) {
        let mut encoded = vec![RDB_ENCVAL | RDB_ENC_INT16];
        encoded.extend_from_slice(&value.to_le_bytes());
        Some(encoded)
    } else if let Ok(value) = i32::try_from(value) {
        let mut encoded = vec![RDB_ENCVAL | RDB_ENC_INT32];
        encoded.extend_from_slice(&value.to_le_bytes());
        Some(encoded)
    } else {
        None
    }
}

/// Decode a Redis RDB length.
fn decode_length(data: &[u8], offset: usize) -> Result<(usize, usize), StoreError> {
    if offset >= data.len() {
        return Err(StoreError::InvalidDumpPayload);
    }
    let first = data[offset];
    match (first & 0xC0) >> 6 {
        0 => Ok(((first & 0x3F) as usize, 1)),
        1 => {
            let second = *data.get(offset + 1).ok_or(StoreError::InvalidDumpPayload)?;
            let len = (((first & 0x3F) as usize) << 8) | usize::from(second);
            Ok((len, 2))
        }
        2 if first == 0x80 => {
            if offset + 5 > data.len() {
                return Err(StoreError::InvalidDumpPayload);
            }
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&data[offset + 1..offset + 5]);
            Ok((u32::from_be_bytes(bytes) as usize, 5))
        }
        2 if first == 0x81 => {
            if offset + 9 > data.len() {
                return Err(StoreError::InvalidDumpPayload);
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[offset + 1..offset + 9]);
            let len = u64::from_be_bytes(bytes);
            let Ok(len) = usize::try_from(len) else {
                return Err(StoreError::InvalidDumpPayload);
            };
            Ok((len, 9))
        }
        _ => Err(StoreError::InvalidDumpPayload),
    }
}

fn decode_rdb_string(
    data: &[u8],
    offset: usize,
    data_end: usize,
) -> Result<(Vec<u8>, usize), StoreError> {
    if offset >= data_end {
        return Err(StoreError::InvalidDumpPayload);
    }

    let first = data[offset];
    if (first & RDB_ENCVAL) == RDB_ENCVAL {
        return decode_encoded_rdb_string(data, offset, data_end);
    }

    decode_dump_bulk(data, offset, data_end)
}

fn decode_encoded_rdb_string(
    data: &[u8],
    offset: usize,
    data_end: usize,
) -> Result<(Vec<u8>, usize), StoreError> {
    let encoding = data[offset] & 0x3F;
    match encoding {
        RDB_ENC_INT8 => {
            let raw = *data
                .get(offset + 1)
                .filter(|_| offset + 2 <= data_end)
                .ok_or(StoreError::InvalidDumpPayload)?;
            let value = i8::from_le_bytes([raw]);
            Ok((value.to_string().into_bytes(), 2))
        }
        RDB_ENC_INT16 => {
            if offset + 3 > data_end {
                return Err(StoreError::InvalidDumpPayload);
            }
            let value = i16::from_le_bytes([data[offset + 1], data[offset + 2]]);
            Ok((value.to_string().into_bytes(), 3))
        }
        RDB_ENC_INT32 => {
            if offset + 5 > data_end {
                return Err(StoreError::InvalidDumpPayload);
            }
            let value = i32::from_le_bytes([
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
            ]);
            Ok((value.to_string().into_bytes(), 5))
        }
        RDB_ENC_LZF => decode_lzf_rdb_string(data, offset, data_end),
        _ => Err(StoreError::InvalidDumpPayload),
    }
}

fn decode_lzf_rdb_string(
    data: &[u8],
    offset: usize,
    data_end: usize,
) -> Result<(Vec<u8>, usize), StoreError> {
    let (compressed_len, compressed_len_bytes) = decode_length(data, offset + 1)?;
    let uncompressed_len_offset = offset
        .checked_add(1)
        .and_then(|pos| pos.checked_add(compressed_len_bytes))
        .ok_or(StoreError::InvalidDumpPayload)?;
    let (uncompressed_len, uncompressed_len_bytes) = decode_length(data, uncompressed_len_offset)?;
    let payload_start = uncompressed_len_offset
        .checked_add(uncompressed_len_bytes)
        .ok_or(StoreError::InvalidDumpPayload)?;
    let payload_end = payload_start
        .checked_add(compressed_len)
        .ok_or(StoreError::InvalidDumpPayload)?;
    if payload_end > data_end {
        return Err(StoreError::InvalidDumpPayload);
    }
    let decompressed = lzf_decompress_string(&data[payload_start..payload_end], uncompressed_len)
        .ok_or(StoreError::InvalidDumpPayload)?;
    Ok((decompressed, payload_end - offset))
}

fn lzf_decompress_string(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    if expected_len > RDB_STRING_MAX_ALLOC {
        return None;
    }

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

fn decode_dump_bulk(
    data: &[u8],
    offset: usize,
    data_end: usize,
) -> Result<(Vec<u8>, usize), StoreError> {
    let (len, len_bytes) = decode_length(data, offset)?;
    let start = offset
        .checked_add(len_bytes)
        .ok_or(StoreError::InvalidDumpPayload)?;
    let end = start
        .checked_add(len)
        .ok_or(StoreError::InvalidDumpPayload)?;
    if end > data_end {
        return Err(StoreError::InvalidDumpPayload);
    }
    Ok((data[start..end].to_vec(), len_bytes + len))
}

fn encode_dump_bulk(buf: &mut Vec<u8>, data: &[u8]) {
    encode_length(buf, data.len());
    buf.extend_from_slice(data);
}

fn encode_intset(values: &[i64]) -> Option<Vec<u8>> {
    let width = if values.iter().all(|value| i16::try_from(*value).is_ok()) {
        2u32
    } else if values.iter().all(|value| i32::try_from(*value).is_ok()) {
        4u32
    } else {
        8u32
    };
    let len = u32::try_from(values.len()).ok()?;
    let mut out = Vec::with_capacity(8 + values.len() * usize::try_from(width).ok()?);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    for value in values {
        match width {
            2 => out.extend_from_slice(&i16::try_from(*value).ok()?.to_le_bytes()),
            4 => out.extend_from_slice(&i32::try_from(*value).ok()?.to_le_bytes()),
            8 => out.extend_from_slice(&value.to_le_bytes()),
            _ => unreachable!("width selected above"),
        }
    }
    Some(out)
}

fn encode_listpack_strings(entries: &[&[u8]]) -> Option<Vec<u8>> {
    let mut encoded_entries = Vec::new();
    for entry in entries {
        encode_listpack_entry(&mut encoded_entries, entry);
    }
    let total_bytes = 6usize
        .checked_add(encoded_entries.len())
        .and_then(|len| len.checked_add(1))?;
    let total_bytes = u32::try_from(total_bytes).ok()?;
    let capacity = usize::try_from(total_bytes).ok()?;
    let mut listpack = Vec::with_capacity(capacity);
    listpack.extend_from_slice(&total_bytes.to_le_bytes());
    let entry_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
    listpack.extend_from_slice(&entry_count.to_le_bytes());
    listpack.extend_from_slice(&encoded_entries);
    listpack.push(0xFF);
    Some(listpack)
}

fn encode_listpack_entry(buf: &mut Vec<u8>, entry: &[u8]) {
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

fn decode_listpack_strings(data: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
    fr_persist::listpack::decode_listpack(data)
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| entry.to_bytes())
                .collect::<Vec<_>>()
        })
        .map_err(|_| StoreError::InvalidDumpPayload)
}

fn decode_intset_members(data: &[u8]) -> Result<Vec<Vec<u8>>, StoreError> {
    if data.len() < 8 {
        return Err(StoreError::InvalidDumpPayload);
    }
    let encoding = u32::from_le_bytes(
        data[0..4]
            .try_into()
            .map_err(|_| StoreError::InvalidDumpPayload)?,
    );
    let len = u32::from_le_bytes(
        data[4..8]
            .try_into()
            .map_err(|_| StoreError::InvalidDumpPayload)?,
    ) as usize;
    let width = match encoding {
        2 => 2,
        4 => 4,
        8 => 8,
        _ => return Err(StoreError::InvalidDumpPayload),
    };
    let expected_len = 8usize
        .checked_add(
            len.checked_mul(width)
                .ok_or(StoreError::InvalidDumpPayload)?,
        )
        .ok_or(StoreError::InvalidDumpPayload)?;
    if data.len() != expected_len {
        return Err(StoreError::InvalidDumpPayload);
    }

    let mut members = Vec::with_capacity(len);
    let mut cursor = 8;
    for _ in 0..len {
        let value = match width {
            2 => {
                let raw = i16::from_le_bytes(
                    data[cursor..cursor + 2]
                        .try_into()
                        .map_err(|_| StoreError::InvalidDumpPayload)?,
                );
                cursor += 2;
                i64::from(raw)
            }
            4 => {
                let raw = i32::from_le_bytes(
                    data[cursor..cursor + 4]
                        .try_into()
                        .map_err(|_| StoreError::InvalidDumpPayload)?,
                );
                cursor += 4;
                i64::from(raw)
            }
            8 => {
                let raw = i64::from_le_bytes(
                    data[cursor..cursor + 8]
                        .try_into()
                        .map_err(|_| StoreError::InvalidDumpPayload)?,
                );
                cursor += 8;
                raw
            }
            _ => unreachable!("width checked above"),
        };
        members.push(value.to_string().into_bytes());
    }
    Ok(members)
}

/// Minimal SHA-1 implementation (pure Rust, no unsafe).
/// Public alias for use by fr-command's Lua evaluator (redis.sha1hex).
pub fn sha1_hex_public(data: &[u8]) -> String {
    sha1_hex(data)
}

/// Generate a 40-character hex run ID (like Redis's run_id).
/// Uses process ID and a timestamp-based seed for uniqueness.
fn generate_run_id() -> String {
    let pid = std::process::id() as u64;
    let seed = pid
        .wrapping_mul(0x5851_f42d_4c95_7f2d)
        .wrapping_add(0xDEAD_BEEF);
    let mut state = seed;
    let mut hex = String::with_capacity(40);
    for _ in 0..5 {
        state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
        hex.push_str(&format!("{state:016x}"));
    }
    hex.truncate(40);
    hex
}

/// Minimal SHA-1 implementation (pure Rust, no unsafe).
fn sha1_hex(data: &[u8]) -> String {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        #[allow(clippy::needless_range_loop)]
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }
    format!("{h0:08x}{h1:08x}{h2:08x}{h3:08x}{h4:08x}")
}

const ENTRY_BASE_OVERHEAD_BYTES: usize = 32;
const EXPIRY_METADATA_BYTES: usize = 8;
const SORTED_SET_SCORE_BYTES: usize = 8;
const STREAM_ID_BYTES: usize = 16;
const HASHMAP_BUCKET_OVERHEAD_BYTES: usize = 16;

fn estimate_entry_memory_usage_bytes(key: &[u8], entry: &Entry) -> usize {
    key.len()
        .saturating_add(ENTRY_BASE_OVERHEAD_BYTES)
        .saturating_add(EXPIRY_METADATA_BYTES)
        .saturating_add(estimate_value_memory_usage_bytes(&entry.value))
}

fn estimate_value_memory_usage_bytes(value: &Value) -> usize {
    match value {
        Value::String(bytes) => bytes.len(),
        Value::Hash(fields) => fields
            .iter()
            .map(|(field, value)| {
                field
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(HASHMAP_BUCKET_OVERHEAD_BYTES)
            })
            .sum(),
        Value::List(items) => items.iter().map(Vec::len).sum(),
        Value::Set(members) => members
            .iter()
            .map(|member| member.len().saturating_add(HASHMAP_BUCKET_OVERHEAD_BYTES))
            .sum(),
        Value::SortedSet(members) => members
            .keys()
            .map(|member| {
                member
                    .len()
                    .saturating_add(SORTED_SET_SCORE_BYTES)
                    .saturating_add(HASHMAP_BUCKET_OVERHEAD_BYTES)
            })
            .sum(),
        Value::Stream(entries) => entries
            .values()
            .map(|fields| {
                STREAM_ID_BYTES.saturating_add(
                    fields
                        .iter()
                        .map(|(field, value)| field.len().saturating_add(value.len()))
                        .sum::<usize>(),
                )
            })
            .sum(),
    }
}

fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn aggregate_scores(a: f64, b: f64, aggregate: &[u8]) -> f64 {
    if eq_ascii_ci(aggregate, b"MIN") {
        a.min(b)
    } else if eq_ascii_ci(aggregate, b"MAX") {
        a.max(b)
    } else {
        // Default is SUM
        a + b
    }
}

fn parse_i64(bytes: &[u8]) -> Result<i64, StoreError> {
    let slen = bytes.len();
    if slen == 0 || slen > 20 {
        return Err(StoreError::ValueNotInteger);
    }
    if slen == 1 && bytes[0] == b'0' {
        return Ok(0);
    }

    let mut p = 0;
    let negative = bytes[0] == b'-';
    if negative {
        p += 1;
        if p == slen {
            return Err(StoreError::ValueNotInteger);
        }
    }

    if bytes[p] >= b'1' && bytes[p] <= b'9' {
        let mut v: u64 = (bytes[p] - b'0') as u64;
        p += 1;
        while p < slen {
            let b = bytes[p];
            if b.is_ascii_digit() {
                if v > (u64::MAX / 10) {
                    return Err(StoreError::ValueNotInteger);
                }
                v *= 10;
                let digit = (b - b'0') as u64;
                if v > (u64::MAX - digit) {
                    return Err(StoreError::ValueNotInteger);
                }
                v += digit;
                p += 1;
            } else {
                return Err(StoreError::ValueNotInteger);
            }
        }

        if negative {
            let limit = (i64::MIN as u64).wrapping_neg();
            if v > limit {
                return Err(StoreError::ValueNotInteger);
            }
            return Ok(v.wrapping_neg() as i64);
        } else {
            if v > i64::MAX as u64 {
                return Err(StoreError::ValueNotInteger);
            }
            return Ok(v as i64);
        }
    }

    Err(StoreError::ValueNotInteger)
}

fn parse_f64(bytes: &[u8]) -> Result<f64, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|_| StoreError::ValueNotFloat)?;
    let val = text
        .trim()
        .parse::<f64>()
        .map_err(|_| StoreError::ValueNotFloat)?;
    if val.is_nan() {
        return Err(StoreError::ValueNotFloat);
    }
    Ok(val)
}

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// CRC16/CCITT for Redis cluster hash slot computation.
#[must_use]
pub fn crc16_slot(key: &[u8]) -> u16 {
    // If key contains {hashtag}, use only the content between first { and next }
    let data = if let Some(start) = key.iter().position(|&b| b == b'{') {
        if let Some(end) = key[start + 1..].iter().position(|&b| b == b'}') {
            if end > 0 {
                &key[start + 1..start + 1 + end]
            } else {
                key
            }
        } else {
            key
        }
    } else {
        key
    };
    let mut crc: u16 = 0;
    for &byte in data {
        crc = ((crc << 8) & 0xFF00) ^ CRC16_TAB[((crc >> 8) as u8 ^ byte) as usize];
    }
    crc % 16384
}

/// CRC16 lookup table from Redis source (CRC-16/XMODEM, poly 0x1021).
#[rustfmt::skip]
const CRC16_TAB: [u16; 256] = [
    0x0000, 0x1021, 0x2042, 0x3063, 0x4084, 0x50a5, 0x60c6, 0x70e7,
    0x8108, 0x9129, 0xa14a, 0xb16b, 0xc18c, 0xd1ad, 0xe1ce, 0xf1ef,
    0x1231, 0x0210, 0x3273, 0x2252, 0x52b5, 0x4294, 0x72f7, 0x62d6,
    0x9339, 0x8318, 0xb37b, 0xa35a, 0xd3bd, 0xc39c, 0xf3ff, 0xe3de,
    0x2462, 0x3443, 0x0420, 0x1401, 0x64e6, 0x74c7, 0x44a4, 0x5485,
    0xa56a, 0xb54b, 0x8528, 0x9509, 0xe5ee, 0xf5cf, 0xc5ac, 0xd58d,
    0x3653, 0x2672, 0x1611, 0x0630, 0x76d7, 0x66f6, 0x5695, 0x46b4,
    0xb75b, 0xa77a, 0x9719, 0x8738, 0xf7df, 0xe7fe, 0xd79d, 0xc7bc,
    0x48c4, 0x58e5, 0x6886, 0x78a7, 0x0840, 0x1861, 0x2802, 0x3823,
    0xc9cc, 0xd9ed, 0xe98e, 0xf9af, 0x8948, 0x9969, 0xa90a, 0xb92b,
    0x5af5, 0x4ad4, 0x7ab7, 0x6a96, 0x1a71, 0x0a50, 0x3a33, 0x2a12,
    0xdbfd, 0xcbdc, 0xfbbf, 0xeb9e, 0x9b79, 0x8b58, 0xbb3b, 0xab1a,
    0x6ca6, 0x7c87, 0x4ce4, 0x5cc5, 0x2c22, 0x3c03, 0x0c60, 0x1c41,
    0xedae, 0xfd8f, 0xcdec, 0xddcd, 0xad2a, 0xbd0b, 0x8d68, 0x9d49,
    0x7e97, 0x6eb6, 0x5ed5, 0x4ef4, 0x3e13, 0x2e32, 0x1e51, 0x0e70,
    0xff9f, 0xefbe, 0xdfdd, 0xcffc, 0xbf1b, 0xaf3a, 0x9f59, 0x8f78,
    0x9188, 0x81a9, 0xb1ca, 0xa1eb, 0xd10c, 0xc12d, 0xf14e, 0xe16f,
    0x1080, 0x00a1, 0x30c2, 0x20e3, 0x5004, 0x4025, 0x7046, 0x6067,
    0x83b9, 0x9398, 0xa3fb, 0xb3da, 0xc33d, 0xd31c, 0xe37f, 0xf35e,
    0x02b1, 0x1290, 0x22f3, 0x32d2, 0x4235, 0x5214, 0x6277, 0x7256,
    0xb5ea, 0xa5cb, 0x95a8, 0x8589, 0xf56e, 0xe54f, 0xd52c, 0xc50d,
    0x34e2, 0x24c3, 0x14a0, 0x0481, 0x7466, 0x6447, 0x5424, 0x4405,
    0xa7db, 0xb7fa, 0x8799, 0x97b8, 0xe75f, 0xf77e, 0xc71d, 0xd73c,
    0x26d3, 0x36f2, 0x0691, 0x16b0, 0x6657, 0x7676, 0x4615, 0x5634,
    0xd94c, 0xc96d, 0xf90e, 0xe92f, 0x99c8, 0x89e9, 0xb98a, 0xa9ab,
    0x5844, 0x4865, 0x7806, 0x6827, 0x18c0, 0x08e1, 0x3882, 0x28a3,
    0xcb7d, 0xdb5c, 0xeb3f, 0xfb1e, 0x8bf9, 0x9bd8, 0xabbb, 0xbb9a,
    0x4a75, 0x5a54, 0x6a37, 0x7a16, 0x0af1, 0x1ad0, 0x2ab3, 0x3a92,
    0xfd2e, 0xed0f, 0xdd6c, 0xcd4d, 0xbdaa, 0xad8b, 0x9de8, 0x8dc9,
    0x7c26, 0x6c07, 0x5c64, 0x4c45, 0x3ca2, 0x2c83, 0x1ce0, 0x0cc1,
    0xef1f, 0xff3e, 0xcf5d, 0xdf7c, 0xaf9b, 0xbfba, 0x8fd9, 0x9ff8,
    0x6e17, 0x7e36, 0x4e55, 0x5e74, 0x2e93, 0x3eb2, 0x0ed1, 0x1ef0,
];

/// Convert a Redis-style index (negative = from end) to a `usize`.
/// Read `bits` bits starting at `bit_offset` from a byte slice (MSB-first bit ordering).
/// Returns the value as i64. If `signed`, sign-extends from the field width.
fn bitfield_read(bytes: &[u8], bit_offset: u64, bits: u8, signed: bool) -> i64 {
    if bits == 0 {
        return 0;
    }
    let mut value: u64 = 0;
    for b in 0..u64::from(bits) {
        let pos = bit_offset.wrapping_add(b);
        let byte_idx = (pos / 8) as usize;
        let bit_idx = 7 - (pos % 8) as u8;
        let bit_val = if byte_idx < bytes.len() {
            (bytes[byte_idx] >> bit_idx) & 1
        } else {
            0
        };
        value = (value << 1) | u64::from(bit_val);
    }
    if signed && bits < 64 {
        // Sign-extend: if the MSB of the field is set, fill upper bits with 1s
        let sign_bit = 1u64 << (bits - 1);
        if value & sign_bit != 0 {
            let mask = u64::MAX << bits;
            value |= mask;
        }
    }
    value as i64
}

/// Write `bits` bits of `value` starting at `bit_offset` in a byte slice (MSB-first).
/// The byte slice must already be large enough to hold the write.
fn bitfield_write(bytes: &mut [u8], bit_offset: u64, bits: u8, value: i64) {
    let value = value as u64;
    for b in 0..u64::from(bits) {
        let pos = bit_offset.wrapping_add(b);
        let byte_idx = (pos / 8) as usize;
        let bit_idx = 7 - (pos % 8) as u8;
        // Extract the bit from value (MSB of the field first)
        let bit_pos = (bits as u64) - 1 - b;
        let bit_val = (value >> bit_pos) & 1;
        if bit_val == 1 {
            bytes[byte_idx] |= 1 << bit_idx;
        } else {
            bytes[byte_idx] &= !(1 << bit_idx);
        }
    }
}

fn normalize_index(index: i64, len: i64) -> i64 {
    if index < 0 {
        len.saturating_add(index)
    } else {
        index
    }
}

/// Compare (score, member) pairs for sorted set ordering.
/// Redis sorts by score first, then by member lexicographically for ties.
fn cmp_score_member(s1: f64, m1: &[u8], s2: f64, m2: &[u8]) -> std::cmp::Ordering {
    canonicalize_zero_score(s1)
        .total_cmp(&canonicalize_zero_score(s2))
        .then_with(|| m1.cmp(m2))
}

/// Returns true if (s1, m1) < (s2, m2) in Redis sorted set ordering.
fn score_member_lt(s1: f64, m1: &[u8], s2: f64, m2: &[u8]) -> bool {
    cmp_score_member(s1, m1, s2, m2) == std::cmp::Ordering::Less
}

fn canonicalize_zero_score(score: f64) -> f64 {
    if score == 0.0 { 0.0 } else { score }
}

/// Check if a member falls within a lex range.
/// Redis lex range format: `-` = neg infinity, `+` = pos infinity,
/// `[value` = inclusive, `(value` = exclusive.
fn lex_in_range(member: &[u8], min: &[u8], max: &[u8]) -> bool {
    let above_min = if min == b"-" {
        true
    } else if min.starts_with(b"(") {
        member > &min[1..]
    } else if min.starts_with(b"[") {
        member >= &min[1..]
    } else {
        member >= min
    };
    let below_max = if max == b"+" {
        true
    } else if max.starts_with(b"(") {
        member < &max[1..]
    } else if max.starts_with(b"[") {
        member <= &max[1..]
    } else {
        member <= max
    };
    above_min && below_max
}

// ── HyperLogLog internals ─────────────────────────────────────────────

const HLL_P: u32 = 14;
const HLL_REGISTERS: usize = 1 << HLL_P; // 16384
const HLL_LEGACY_MAGIC: &[u8] = b"HYLL";
const HLL_MAGIC_V2: &[u8] = b"HYL2";
const HLL_HEADER_SIZE: usize = HLL_MAGIC_V2.len() + 1;
const HLL_DATA_SIZE: usize = HLL_HEADER_SIZE + HLL_REGISTERS; // 16389
const HLL_LEGACY_DATA_SIZE: usize = HLL_LEGACY_MAGIC.len() + HLL_REGISTERS; // 16388
const HLL_REDIS_HEADER_SIZE: usize = 16;
const HLL_SPARSE_VAL_MAX_VALUE: u8 = 32;
const HLL_SPARSE_VAL_MAX_LEN: usize = 4;
const HLL_SPARSE_ZERO_MAX_LEN: usize = 64;
const HLL_SPARSE_XZERO_MAX_LEN: usize = 16_384;
const HLL_REDIS_SPARSE_MAX_BYTES: usize = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HllEncoding {
    Sparse,
    Dense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HllSparseOpcode {
    Zero(usize),
    XZero(usize),
    Val { value: u8, len: usize },
}

impl HllEncoding {
    fn as_byte(self) -> u8 {
        match self {
            Self::Sparse => 0,
            Self::Dense => 1,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Sparse),
            1 => Some(Self::Dense),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Sparse => "sparse",
            Self::Dense => "dense",
        }
    }
}

/// MurmurHash64A-style hash for HyperLogLog element hashing.
/// HyperLogLog accuracy depends on good bit dispersion; the earlier FNV-based
/// implementation produced materially worse estimator error at moderate
/// cardinalities.
fn hll_hash(data: &[u8]) -> u64 {
    const M: u64 = 0xc6a4_a793_5bd1_e995;
    const R: u32 = 47;

    let len = data.len() as u64;
    let mut h = 0xadc8_3b19_u64 ^ len.wrapping_mul(M);

    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let mut k_bytes = [0u8; 8];
        k_bytes.copy_from_slice(chunk);
        let mut k = u64::from_le_bytes(k_bytes);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);

        h ^= k;
        h = h.wrapping_mul(M);
    }

    let tail = chunks.remainder();
    if !tail.is_empty() {
        let mut remaining = 0_u64;
        for (shift, byte) in tail.iter().enumerate() {
            remaining |= u64::from(*byte) << (shift * 8);
        }
        h ^= remaining;
        h = h.wrapping_mul(M);
    }

    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^ (h >> R)
}

/// Position of the leftmost 1-bit in a `(64 - HLL_P)`-bit value, counting from 1.
/// Returns `64 - HLL_P + 1` when `w == 0` (all zeros).
fn hll_rho(w: u64) -> u8 {
    let width = 64 - HLL_P; // 50
    if w == 0 {
        return (width + 1) as u8;
    }
    let tz = w.trailing_zeros();
    (tz + 1) as u8
}

fn hll_parse(data: &[u8]) -> Result<(HllEncoding, Vec<u8>), StoreError> {
    if data.len() == HLL_DATA_SIZE && data.starts_with(HLL_MAGIC_V2) {
        let encoding =
            HllEncoding::from_byte(data[HLL_MAGIC_V2.len()]).ok_or(StoreError::InvalidHllValue)?;
        return Ok((encoding, data[HLL_HEADER_SIZE..].to_vec()));
    }
    if data.len() == HLL_LEGACY_DATA_SIZE && data.starts_with(HLL_LEGACY_MAGIC) {
        return Ok((HllEncoding::Dense, data[HLL_LEGACY_MAGIC.len()..].to_vec()));
    }
    Err(StoreError::InvalidHllValue)
}

fn hll_parse_registers(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    hll_parse(data).map(|(_, registers)| registers)
}

fn hll_encode(registers: &[u8], encoding: HllEncoding) -> Vec<u8> {
    let mut data = Vec::with_capacity(HLL_DATA_SIZE);
    data.extend_from_slice(HLL_MAGIC_V2);
    data.push(encoding.as_byte());
    data.extend_from_slice(registers);
    data
}

fn hll_sparse_opcodes(registers: &[u8]) -> Option<Vec<HllSparseOpcode>> {
    let mut opcodes = Vec::new();
    let mut index = 0;
    while index < registers.len() {
        let value = registers[index];
        let mut run_len = 1usize;
        while index + run_len < registers.len() && registers[index + run_len] == value {
            run_len += 1;
        }
        if value == 0 {
            let mut remaining = run_len;
            while remaining > 0 {
                let chunk = if remaining > HLL_SPARSE_ZERO_MAX_LEN {
                    remaining.min(HLL_SPARSE_XZERO_MAX_LEN)
                } else {
                    remaining
                };
                if chunk > HLL_SPARSE_ZERO_MAX_LEN {
                    opcodes.push(HllSparseOpcode::XZero(chunk));
                } else {
                    opcodes.push(HllSparseOpcode::Zero(chunk));
                }
                remaining -= chunk;
            }
        } else {
            if value > HLL_SPARSE_VAL_MAX_VALUE {
                return None;
            }
            let mut remaining = run_len;
            while remaining > 0 {
                let chunk = remaining.min(HLL_SPARSE_VAL_MAX_LEN);
                opcodes.push(HllSparseOpcode::Val { value, len: chunk });
                remaining -= chunk;
            }
        }
        index += run_len;
    }
    Some(opcodes)
}

fn hll_sparse_storage_len(registers: &[u8]) -> Option<usize> {
    hll_sparse_opcodes(registers).map(|opcodes| {
        HLL_REDIS_HEADER_SIZE
            + opcodes
                .iter()
                .map(|opcode| match opcode {
                    HllSparseOpcode::Zero(_) | HllSparseOpcode::Val { .. } => 1,
                    HllSparseOpcode::XZero(_) => 2,
                })
                .sum::<usize>()
    })
}

fn hll_sparse_should_promote(registers: &[u8]) -> bool {
    match hll_sparse_storage_len(registers) {
        Some(len) => len > HLL_REDIS_SPARSE_MAX_BYTES,
        None => true,
    }
}

fn hll_sparse_decode(registers: &[u8]) -> Result<String, StoreError> {
    let mut segments = Vec::new();
    for opcode in hll_sparse_opcodes(registers).ok_or(StoreError::InvalidHllValue)? {
        match opcode {
            HllSparseOpcode::Zero(len) => segments.push(format!("z:{len}")),
            HllSparseOpcode::XZero(len) => segments.push(format!("Z:{len}")),
            HllSparseOpcode::Val { value, len } => segments.push(format!("v:{value},{len}")),
        }
    }
    Ok(segments.join(" "))
}

fn hll_estimate(registers: &[u8]) -> u64 {
    let m = HLL_REGISTERS as f64;
    let alpha_m = 0.7213 / (1.0 + 1.079 / m);

    let mut sum = 0.0_f64;
    let mut zeros = 0_u32;
    for &reg in registers {
        sum += 2.0_f64.powi(-i32::from(reg));
        if reg == 0 {
            zeros += 1;
        }
    }

    let estimate = alpha_m * m * m / sum;

    // Small-range correction via linear counting
    if estimate <= 2.5 * m && zeros > 0 {
        let lc = m * (m / f64::from(zeros)).ln();
        lc.round() as u64
    } else {
        estimate.round() as u64
    }
}

fn hll_add_to_registers(registers: &mut [u8], element: &[u8]) {
    let hash = hll_hash(element);
    let index = (hash as usize) & (HLL_REGISTERS - 1);
    let w = hash >> HLL_P;
    let count = hll_rho(w);
    if count > registers[index] {
        registers[index] = count;
    }
}

fn hll_run_selftest() -> Result<(), String> {
    const HLL_TEST_CYCLES: usize = 64;

    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next_u64 = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    for _ in 0..HLL_TEST_CYCLES {
        let mut expected = vec![0u8; HLL_REGISTERS];
        for value in &mut expected {
            *value = (next_u64() & 63) as u8;
        }
        let encoded = hll_encode(&expected, HllEncoding::Sparse);
        let decoded = hll_parse_registers(&encoded)
            .map_err(|_| "TESTFAILED encoded register payload did not round-trip".to_string())?;
        if decoded != expected {
            return Err("TESTFAILED register round-trip mismatch".to_string());
        }
    }

    let mut registers = vec![0u8; HLL_REGISTERS];
    let relerr = 1.04 / (HLL_REGISTERS as f64).sqrt();
    let mut checkpoint = 1_u64;
    let mut element = 0_u64;

    while checkpoint <= 1_000_000 {
        while element < checkpoint {
            element += 1;
            hll_add_to_registers(&mut registers, &element.to_le_bytes());
        }
        let estimate = hll_estimate(&registers);
        let abserr = estimate.abs_diff(checkpoint);
        let mut maxerr = (relerr * 6.0 * checkpoint as f64).ceil() as u64;
        if checkpoint == 10 {
            maxerr = 1;
        }
        if abserr > maxerr {
            return Err(format!(
                "TESTFAILED Too big error. card:{checkpoint} abserr:{abserr}"
            ));
        }
        checkpoint *= 10;
    }

    Ok(())
}

/// Redis-compatible glob pattern matching.
///
/// Supports `*` (match any sequence), `?` (match one byte),
/// `[abc]` (character class), `[^abc]` (negated class),
/// and `\x` (escape).
/// String-based glob match wrapper for function library filtering.
fn glob_match_str(pattern: &str, string: &str) -> bool {
    glob_match(pattern.as_bytes(), string.as_bytes())
}

/// Extract a function name from a redis.register_function call.
/// Handles patterns like:
///   redis.register_function('myFunc', function(keys, args) ...)
///   redis.register_function{function_name='myFunc', ...}
/// Pull `(name, description)` out of a `redis.register_function`
/// call. Upstream function_lua.c accepts the `description` field
/// in the table-form invocation (`{function_name='n',
/// description='d', callback=fn}`); previously we extracted only
/// the function name and dropped the description, leaving the
/// FUNCTION LIST `description` slot at nil even when the script
/// explicitly set it. (br-frankenredis-r85v)
fn extract_function_metadata(line: &str) -> Option<(String, Option<String>)> {
    // Pattern 1: redis.register_function('name', callback)
    if let Some(start) = line.find("register_function") {
        let rest = &line[start..];
        if let Some(paren) = rest.find('(') {
            let after = &rest[paren + 1..].trim_start();
            if let Some(name) = extract_quoted_string(after) {
                return Some((name, None));
            }
        }
        // Pattern 2: register_function{function_name='name',
        //              description='d', callback=fn, ...}
        if let Some(brace) = rest.find('{') {
            let after = &rest[brace + 1..];
            // Match key tokens with the `=` attached so that a
            // function_name VALUE containing the substring
            // `description` (e.g. function_name='describe_user')
            // doesn't bleed into the description slot. Tolerate
            // optional whitespace around `=`. (br-frankenredis-r85v)
            let name = extract_table_field(after, "function_name");
            let description = extract_table_field(after, "description");
            if let Some(name) = name {
                return Some((name, description));
            }
        }
    }
    None
}

/// Find `<key> *= *<quoted-string>` in a Lua-table-form argument
/// list and return the quoted value. Skips field assignments where
/// the key matches `<key>` only as a substring of another
/// identifier (e.g. `function_name` contains `description` only as
/// a value-side substring, never as a key). (br-frankenredis-r85v)
fn extract_table_field(buf: &str, key: &str) -> Option<String> {
    let bytes = buf.as_bytes();
    let key_bytes = key.as_bytes();
    let mut i = 0;
    while i + key_bytes.len() <= bytes.len() {
        if &bytes[i..i + key_bytes.len()] == key_bytes {
            // Boundary check on the LEFT — previous byte must not
            // be an ident char (letter/digit/underscore).
            let left_ok =
                i == 0 || !matches!(bytes[i - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
            // Boundary check on the RIGHT — first non-whitespace
            // byte after key must be `=`.
            let mut j = i + key_bytes.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if left_ok && j < bytes.len() && bytes[j] == b'=' {
                let value_start = j + 1;
                let after_eq = buf[value_start..].trim_start();
                if let Some(v) = extract_quoted_string(after_eq) {
                    return Some(v);
                }
            }
        }
        i += 1;
    }
    None
}

fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim();
    let quote = s.as_bytes().first().copied()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let rest = &s[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

pub fn glob_match(pattern: &[u8], string: &[u8]) -> bool {
    glob_match_inner(pattern, string, 0, 0)
}

fn glob_match_inner(pattern: &[u8], string: &[u8], mut pi: usize, mut si: usize) -> bool {
    let mut star_pi = usize::MAX;
    let mut star_si = usize::MAX;

    while si < string.len() {
        if pi < pattern.len() && pattern[pi] == b'\\' && pi + 1 < pattern.len() {
            // Escaped character: must match literally.
            if string[si] == pattern[pi + 1] {
                pi += 2;
                si += 1;
                continue;
            }
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_si = si;
            pi += 1;
            continue;
        } else if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
            continue;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            if let Some((matched, end)) = match_character_class(pattern, pi, string[si])
                && matched
            {
                pi = end;
                si += 1;
                continue;
            }
        } else if pi < pattern.len() && pattern[pi] == string[si] {
            pi += 1;
            si += 1;
            continue;
        }

        // Backtrack to last star.
        if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
            continue;
        }

        return false;
    }

    // Consume trailing stars.
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match a `[...]` character class at `pattern[pi]`.
/// Returns `Some((matched, index_after_bracket))` or `None` if malformed.
fn match_character_class(pattern: &[u8], pi: usize, ch: u8) -> Option<(bool, usize)> {
    debug_assert_eq!(pattern[pi], b'[');
    let mut i = pi + 1;
    let negate = i < pattern.len() && pattern[i] == b'^';
    if negate {
        i += 1;
    }

    let mut matched = false;
    loop {
        if i + 1 < pattern.len() && pattern[i] == b'\\' {
            i += 1;
            if pattern[i] == ch {
                matched = true;
            }
            i += 1;
            continue;
        }

        if i >= pattern.len() {
            // Redis malformed-class behavior: treat the final class byte as the terminator.
            if i > pi + 1 {
                i -= 1;
            }
            break;
        }

        if pattern[i] == b']' {
            break;
        }

        if i + 2 < pattern.len() && pattern[i + 1] == b'-' {
            let mut lo = pattern[i];
            let mut hi = pattern[i + 2];
            if lo > hi {
                std::mem::swap(&mut lo, &mut hi);
            }
            if ch >= lo && ch <= hi {
                matched = true;
            }
            i += 3;
            continue;
        }

        if pattern[i] == ch {
            matched = true;
        }
        i += 1;
    }

    let result = if negate { !matched } else { matched };
    Some((result, (i + 1).min(pattern.len())))
}

#[cfg(test)]
mod tests {
    use super::{
        BitRangeUnit, DUMP_CRC64_LEN, DUMP_TRAILER_LEN, DUMP_VERSION_LEN, EvictionLoopFailure,
        EvictionLoopStatus, EvictionSafetyGateState, ExpireTimeValue, HLL_REGISTERS, LatencySample,
        MaxmemoryPolicy, MaxmemoryPressureLevel, NOTIFY_EVICTED, NOTIFY_EXPIRED, NOTIFY_GENERIC,
        NOTIFY_KEYEVENT, PttlValue, RDB_DUMP_VERSION, RDB_OPCODE_FUNCTION2, RDB_TYPE_HASH,
        RDB_TYPE_HASH_LISTPACK, RDB_TYPE_LIST_QUICKLIST_2, RDB_TYPE_SET, RDB_TYPE_SET_INTSET,
        RDB_TYPE_SET_LISTPACK, RDB_TYPE_STREAM_LISTPACKS_3, RDB_TYPE_STRING, RDB_TYPE_ZSET_2,
        RDB_TYPE_ZSET_LISTPACK, ScoreBound, ScoreMember, Store, StoreError, StreamAutoClaimOptions,
        StreamAutoClaimReply, StreamClaimOptions, StreamClaimReply, StreamGroupReadCursor,
        StreamGroupReadOptions, StreamPendingEntry, Value, ValueType, decode_rdb_string,
        encode_db_key, encode_length, hll_sparse_decode,
    };

    fn group_read_options(
        cursor: StreamGroupReadCursor,
        noack: bool,
        count: Option<usize>,
    ) -> StreamGroupReadOptions {
        StreamGroupReadOptions {
            cursor,
            noack,
            count,
        }
    }

    fn function_library_snapshot(store: &Store) -> Vec<(String, String, Vec<u8>, Vec<String>)> {
        store
            .function_list(None)
            .into_iter()
            .map(|library| {
                let mut function_names: Vec<String> = library
                    .functions
                    .iter()
                    .map(|function| function.name.clone())
                    .collect();
                function_names.sort();
                (
                    library.name.clone(),
                    library.engine.clone(),
                    library.code.clone(),
                    function_names,
                )
            })
            .collect()
    }

    fn sample_function_library(name: &str, first_fn: &str, second_fn: &str) -> Vec<u8> {
        format!(
            "#!lua name={name}\n\
             redis.register_function('{first_fn}', function(keys, args) return #keys + #args end)\n\
             redis.register_function{{function_name='{second_fn}', callback=function(keys, args) return 0 end}}\n"
        )
        .into_bytes()
    }

    fn sample_function_library_from_seed(seed: u16) -> Vec<u8> {
        sample_function_library(
            &format!("seedlib_{seed:04x}"),
            &format!("alpha_{seed:04x}"),
            &format!("beta_{seed:04x}"),
        )
    }

    fn sample_replacement_function_library_from_seed(seed: u16) -> Vec<u8> {
        sample_function_library(
            &format!("seedlib_{seed:04x}"),
            &format!("gamma_{seed:04x}"),
            &format!("delta_{seed:04x}"),
        )
    }

    #[test]
    fn set_get_and_del() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        assert_eq!(store.get(b"k", 100).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.del(&[b"k".to_vec()], 100), 1);
        assert_eq!(store.get(b"k", 100).unwrap(), None);
    }

    #[test]
    fn keyspace_hit_and_miss_counters_follow_store_lookup_paths() {
        let mut store = Store::new();
        store.set(b"s".to_vec(), b"v".to_vec(), None, 0);
        store
            .hset(b"h", b"field".to_vec(), b"value".to_vec(), 0)
            .expect("hset");
        store.rpush(b"list", &[b"item".to_vec()], 0).expect("rpush");
        store.sadd(b"set", &[b"member".to_vec()], 0).expect("sadd");
        store
            .zadd(b"zset", &[(1.0, b"member".to_vec())], 0)
            .expect("zadd");
        store
            .xadd(b"stream", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .expect("xadd");

        assert_eq!(store.get(b"s", 0).unwrap(), Some(b"v".to_vec()));
        assert!(store.exists(b"s", 0));
        assert_eq!(store.get(b"missing", 0).expect("missing string read"), None);
        assert_eq!(
            store.hget(b"h", b"field", 0).expect("existing hash field"),
            Some(b"value".to_vec())
        );
        assert_eq!(
            store.lindex(b"list", 0, 0).expect("list lookup"),
            Some(b"item".to_vec())
        );
        assert!(store.sismember(b"set", b"member", 0).expect("set lookup"));
        assert_eq!(
            store.zscore(b"zset", b"member", 0).expect("zset lookup"),
            Some(1.0)
        );
        assert_eq!(store.xlen(b"stream", 0).expect("stream lookup"), 1);
        assert_eq!(
            store
                .smembers(b"missing-set", 0)
                .expect("missing set lookup"),
            Vec::<Vec<u8>>::new()
        );
        assert_eq!(store.xlen(b"missing-stream", 0).expect("missing stream"), 0);

        assert_eq!(store.stat_keyspace_hits, 7);
        assert_eq!(store.stat_keyspace_misses, 3);
    }

    #[test]
    fn exists_no_touch_updates_stats_without_lru() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        store.reset_info_stats();

        assert!(store.exists_no_touch(b"k", 200));
        assert_eq!(store.stat_keyspace_hits, 1);
        assert_eq!(store.stat_keyspace_misses, 0);
        assert_eq!(
            store
                .entries
                .get(b"k".as_ref())
                .expect("exists entry")
                .last_access_ms,
            100
        );

        assert!(!store.exists_no_touch(b"missing", 200));
        assert_eq!(store.stat_keyspace_misses, 1);
    }

    #[test]
    fn touch_and_sort_update_lru_and_keyspace_stats() {
        let mut store = Store::new();
        store
            .rpush(b"list", &[b"item".to_vec()], 10)
            .expect("rpush");
        store.sadd(b"set", &[b"member".to_vec()], 10).expect("sadd");
        store.reset_info_stats();

        assert_eq!(
            store
                .entries
                .get(b"list".as_ref())
                .expect("list entry")
                .last_access_ms,
            10
        );
        assert_eq!(
            store
                .entries
                .get(b"set".as_ref())
                .expect("set entry")
                .last_access_ms,
            10
        );

        let touched = store.touch(&[b"list", b"missing"], 100);
        assert_eq!(touched, 1);
        assert_eq!(
            store
                .entries
                .get(b"list".as_ref())
                .expect("list entry")
                .last_access_ms,
            100
        );
        assert_eq!(store.stat_keyspace_hits, 1);
        assert_eq!(store.stat_keyspace_misses, 1);

        let elements = store.sort_elements(b"set", 200).expect("sort elements");
        assert_eq!(elements, vec![b"member".to_vec()]);
        assert_eq!(
            store
                .entries
                .get(b"set".as_ref())
                .expect("set entry")
                .last_access_ms,
            200
        );
        assert_eq!(store.stat_keyspace_hits, 2);
        assert_eq!(store.stat_keyspace_misses, 1);
    }

    #[test]
    fn latency_tracker_records_latest_history_and_reset() {
        let mut store = Store::new();

        store.record_latency_sample("command", 5, 10);
        store.record_latency_sample("command", 8, 12);
        store.record_latency_sample("fast-command", 2, 11);

        assert_eq!(
            store.latency_latest(),
            vec![
                (
                    "command".to_string(),
                    LatencySample {
                        timestamp_sec: 12,
                        duration_ms: 8,
                    },
                ),
                (
                    "fast-command".to_string(),
                    LatencySample {
                        timestamp_sec: 11,
                        duration_ms: 2,
                    },
                ),
            ]
        );
        assert_eq!(
            store.latency_history("command"),
            vec![
                LatencySample {
                    timestamp_sec: 10,
                    duration_ms: 5,
                },
                LatencySample {
                    timestamp_sec: 12,
                    duration_ms: 8,
                },
            ]
        );
        assert_eq!(store.latency_reset(&["command"]), 1);
        assert!(store.latency_history("command").is_empty());
        assert_eq!(store.latency_reset(&[]), 1);
        assert!(store.latency_latest().is_empty());
    }

    #[test]
    fn latency_tracker_keeps_only_latest_160_samples_per_event() {
        let mut store = Store::new();

        for idx in 0..161u64 {
            store.record_latency_sample("command", idx, idx);
        }

        let history = store.latency_history("command");
        assert_eq!(history.len(), 160);
        assert_eq!(
            history.first(),
            Some(&LatencySample {
                timestamp_sec: 1,
                duration_ms: 1,
            })
        );
        assert_eq!(
            history.last(),
            Some(&LatencySample {
                timestamp_sec: 160,
                duration_ms: 160,
            })
        );
    }

    #[test]
    fn slowlog_records_reads_and_resets_entries() {
        let mut store = Store::new();
        store.slowlog_log_slower_than_us = 50;
        store.slowlog_max_len = 2;

        store.record_slowlog(&[b"PING".to_vec()], 49, 1_000);
        assert_eq!(store.slowlog_len(), 0);

        store.record_slowlog(&[b"SET".to_vec(), b"a".to_vec(), b"1".to_vec()], 50, 2_000);
        store.record_slowlog(&[b"SET".to_vec(), b"b".to_vec(), b"2".to_vec()], 60, 3_000);
        store.record_slowlog(&[b"SET".to_vec(), b"c".to_vec(), b"3".to_vec()], 70, 4_000);

        let entries = store.get_slowlog(10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 2);
        assert_eq!(entries[0].timestamp_sec, 4);
        assert_eq!(entries[0].duration_us, 70);
        assert_eq!(
            entries[0].argv,
            vec![b"SET".to_vec(), b"c".to_vec(), b"3".to_vec()]
        );
        assert_eq!(entries[1].id, 1);

        store.reset_slowlog();
        assert_eq!(store.slowlog_len(), 0);
        assert!(store.get_slowlog(1).is_empty());
        assert_eq!(store.slowlog_id_counter, 0);
    }

    #[test]
    fn incr_missing_then_existing() {
        let mut store = Store::new();
        assert_eq!(store.incr(b"n", 0).expect("incr"), 1);
        assert_eq!(store.incr(b"n", 0).expect("incr"), 2);
        assert_eq!(store.get(b"n", 0).unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn incr_rejects_minus_zero_string() {
        let mut store = Store::new();
        store.set(b"n".to_vec(), b"-0".to_vec(), None, 0);
        assert_eq!(store.incr(b"n", 0), Err(StoreError::ValueNotInteger));
    }

    #[test]
    fn expire_and_pttl() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert!(store.expire_seconds(b"k", 5, 1_000));
        assert_eq!(store.pttl(b"k", 1_000), PttlValue::Remaining(5_000));
        assert_eq!(store.pttl(b"k", 6_001), PttlValue::KeyMissing);
    }

    #[test]
    fn pttl_updates_keyspace_stats_and_lru() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5_000), 1_000);
        store.reset_info_stats();

        assert_eq!(store.pttl(b"k", 2_000), PttlValue::Remaining(4_000));
        assert_eq!(store.stat_keyspace_hits, 1);
        assert_eq!(store.stat_keyspace_misses, 0);
        assert_eq!(
            store
                .entries
                .get(b"k".as_ref())
                .expect("pttl entry")
                .last_access_ms,
            2_000
        );
    }

    #[test]
    fn expiretime_value_reports_state() {
        let mut store = Store::new();
        assert_eq!(
            store.expiretime_value(b"missing", 0),
            ExpireTimeValue::KeyMissing
        );
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert_eq!(
            store.expiretime_value(b"k", 1_000),
            ExpireTimeValue::NoExpiry
        );
        assert!(store.expire_milliseconds(b"k", 5_000, 1_000));
        assert_eq!(
            store.expiretime_value(b"k", 1_000),
            ExpireTimeValue::ExpiresAt(6_000)
        );
        assert_eq!(
            store.expiretime_value(b"k", 6_001),
            ExpireTimeValue::KeyMissing
        );
    }

    #[test]
    fn pttl_expiry_triggers_stats_and_notifications() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_EXPIRED;
        store.set(b"pttl-exp".to_vec(), b"v".to_vec(), Some(5), 0);
        store.reset_info_stats();

        assert_eq!(store.pttl(b"pttl-exp", 6), PttlValue::KeyMissing);
        assert_eq!(store.stat_expired_keys, 1);
        assert_eq!(store.stat_keyspace_hits, 0);
        assert_eq!(store.stat_keyspace_misses, 1);
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@0__:expired".to_vec(), b"pttl-exp".to_vec())]
        );
    }

    #[test]
    fn expired_key_notifications_use_encoded_db_index() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_EXPIRED;
        let key = encode_db_key(2, b"expiring");
        store.set(key.clone(), b"v".to_vec(), Some(5), 100);

        assert_eq!(store.get(&key, 106).unwrap(), None);
        // Notification should contain the LOGICAL key, not the physical encoded key
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@2__:expired".to_vec(), b"expiring".to_vec())]
        );
    }

    #[test]
    fn randomkey_reaps_expired_keys_with_stats_and_notifications() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_EXPIRED;
        store.set(b"exp".to_vec(), b"v".to_vec(), Some(5), 0);

        assert_eq!(store.randomkey(6), None);
        assert_eq!(store.stat_expired_keys, 1);
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@0__:expired".to_vec(), b"exp".to_vec())]
        );
    }

    #[test]
    fn eviction_notifications_use_encoded_db_index() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_EVICTED;
        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLru;
        let key = encode_db_key(3, b"victim");
        store.set(key.clone(), b"abcdefgh".to_vec(), None, 0);

        let result =
            store.run_bounded_eviction_loop(0, 1, 0, 1, 1, EvictionSafetyGateState::default());
        assert_eq!(result.evicted_keys, 1);
        // Notification should contain the LOGICAL key, not the physical encoded key
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@3__:evicted".to_vec(), b"victim".to_vec())]
        );
    }

    #[test]
    fn expire_milliseconds_honors_ms_precision() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert!(store.expire_milliseconds(b"k", 1_500, 1_000));
        assert_eq!(store.pttl(b"k", 1_000), PttlValue::Remaining(1_500));
        assert_eq!(store.pttl(b"k", 2_501), PttlValue::KeyMissing);
    }

    #[test]
    fn expire_at_milliseconds_sets_absolute_deadline() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert!(store.expire_at_milliseconds(b"k", 5_000, 1_000));
        assert_eq!(store.pttl(b"k", 1_000), PttlValue::Remaining(4_000));
        assert_eq!(store.pttl(b"k", 5_001), PttlValue::KeyMissing);
    }

    #[test]
    fn expire_at_milliseconds_emits_keyevent_notification() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_GENERIC;
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);

        assert!(store.expire_at_milliseconds(b"k", 5_000, 1_000));
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@0__:expire".to_vec(), b"k".to_vec())]
        );
    }

    #[test]
    fn expire_at_milliseconds_deletes_when_deadline_not_in_future() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert!(store.expire_at_milliseconds(b"k", 1_000, 1_000));
        assert_eq!(store.get(b"k", 1_000).unwrap(), None);
    }

    #[test]
    fn expire_at_milliseconds_emits_expired_event_when_deadline_in_past() {
        let mut store = Store::new();
        store.notify_keyspace_events = NOTIFY_KEYEVENT | NOTIFY_EXPIRED;
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);

        // Deadline in the past (500ms < now 1000ms) should emit "expired" not "del"
        assert!(store.expire_at_milliseconds(b"k", 500, 1_000));
        assert_eq!(store.get(b"k", 1_000).unwrap(), None);
        assert_eq!(
            store.drain_keyspace_notifications(),
            vec![(b"__keyevent@0__:expired".to_vec(), b"k".to_vec())]
        );
    }

    #[test]
    fn expire_missing_key_returns_false() {
        let mut store = Store::new();
        assert!(!store.expire_seconds(b"missing", 5, 0));
        assert!(!store.expire_milliseconds(b"missing", 5, 0));
        assert!(!store.expire_at_milliseconds(b"missing", 5_000, 0));
    }

    #[test]
    fn object_encoding_list_honors_legacy_listpack_entry_limit() {
        let mut store = Store::new();
        store.list_max_listpack_size = 1;
        let _ = store.rpush(b"list", &[b"a".to_vec(), b"b".to_vec()], 0);

        assert_eq!(store.object_encoding(b"list", 0), Some("quicklist"));
    }

    #[test]
    fn object_encoding_list_honors_legacy_listpack_byte_limit() {
        let mut store = Store::new();
        store.list_max_listpack_size = -1;
        let large = vec![b'x'; 5000];
        let _ = store.rpush(b"list", &[large], 0);

        assert_eq!(store.object_encoding(b"list", 0), Some("quicklist"));
    }

    #[test]
    fn object_encoding_set_rejects_noncanonical_integer_members() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"01".to_vec()], 0).expect("sadd");
        assert_eq!(store.object_encoding(b"s", 0), Some("listpack"));

        let mut store = Store::new();
        store.sadd(b"t", &[b"1".to_vec()], 0).expect("sadd");
        assert_eq!(store.object_encoding(b"t", 0), Some("intset"));
    }

    #[test]
    fn non_positive_expire_values_delete_immediately_property() {
        for seconds in [0_i64, -1, -30] {
            let mut store = Store::new();
            store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
            assert!(store.expire_seconds(b"k", seconds, 1_000));
            assert_eq!(store.get(b"k", 1_000).unwrap(), None);
        }

        for milliseconds in [0_i64, -1, -500] {
            let mut store = Store::new();
            store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
            assert!(store.expire_milliseconds(b"k", milliseconds, 1_000));
            assert_eq!(store.get(b"k", 1_000).unwrap(), None);
        }
    }

    #[test]
    fn lazy_expiration_evicts_key_at_exact_deadline() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), Some(1_000), 5_000);
        assert!(store.exists(b"k", 5_999));
        assert!(!store.exists(b"k", 6_000));
        assert_eq!(store.get(b"k", 6_000).unwrap(), None);
        assert_eq!(store.stat_expired_keys, 1);
    }

    #[test]
    fn fr_p2c_008_u001_active_expire_cycle_evicts_expired_keys() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), Some(1), 0);
        store.set(b"b".to_vec(), b"2".to_vec(), Some(1), 0);
        store.set(b"c".to_vec(), b"3".to_vec(), None, 0);

        let result = store.run_active_expire_cycle(10, None, 10);
        assert_eq!(result.sampled_keys, 3);
        assert_eq!(result.evicted_keys, 2);
        assert_eq!(store.stat_expired_keys, 2);
        assert_eq!(store.dbsize_in_db(0), 1);
        assert_eq!(store.get(b"c", 10).unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn fr_p2c_008_u002_active_expire_cycle_cursor_is_deterministic() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), Some(1), 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), Some(1), 0);
        store.set(b"d".to_vec(), b"4".to_vec(), None, 0);

        let first = store.run_active_expire_cycle(10, None, 2);
        assert_eq!(first.sampled_keys, 2);
        assert_eq!(first.evicted_keys, 1);
        assert_eq!(first.next_cursor, Some(b"c".to_vec()));

        let second = store.run_active_expire_cycle(10, first.next_cursor.clone(), 2);
        assert_eq!(second.sampled_keys, 2);
        assert_eq!(second.evicted_keys, 1);
    }

    #[test]
    fn fr_p2c_008_u002_count_expiring_keys_ignores_persistent_entries() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), Some(1_000), 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), Some(500), 0);
        assert_eq!(store.count_expiring_keys(), 2);
    }

    #[test]
    fn fr_p2c_008_u010_maxmemory_pressure_excludes_not_counted_bytes() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), vec![b'x'; 64], None, 0);
        store.set(b"b".to_vec(), vec![b'y'; 64], None, 0);

        let pressure = store.classify_maxmemory_pressure(120, 64);
        assert!(pressure.logical_usage_bytes > pressure.counted_usage_bytes);
        assert_eq!(
            pressure.bytes_to_free,
            pressure.counted_usage_bytes.saturating_sub(120)
        );
        assert!(matches!(
            pressure.level,
            MaxmemoryPressureLevel::Soft | MaxmemoryPressureLevel::Hard
        ));
    }

    #[test]
    fn fr_p2c_008_u012_bounded_eviction_loop_reports_running_when_budget_exhausted() {
        let mut store = Store::new();
        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLru;
        for idx in 0..8 {
            let key = format!("fr:p2c:008:evict:{idx}");
            store.set(key.into_bytes(), vec![b'v'; 32], None, 0);
        }

        let result =
            store.run_bounded_eviction_loop(0, 64, 0, 1, 1, EvictionSafetyGateState::default());
        assert_eq!(result.status, EvictionLoopStatus::Running);
        assert!(result.evicted_keys >= 1);
        assert!(store.stat_evicted_keys >= 1);
        assert!(result.bytes_to_free_after > 0);
    }

    #[test]
    fn fr_p2c_008_u012_bounded_eviction_loop_reports_ok_when_pressure_cleared() {
        let mut store = Store::new();
        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLru;
        for idx in 0..6 {
            let key = format!("fr:p2c:008:evict:ok:{idx}");
            store.set(key.into_bytes(), vec![b'v'; 24], None, 0);
        }

        let result =
            store.run_bounded_eviction_loop(0, 64, 0, 2, 16, EvictionSafetyGateState::default());
        assert_eq!(result.status, EvictionLoopStatus::Ok);
        assert!(result.evicted_keys >= 1);
        assert_eq!(store.stat_evicted_keys, result.evicted_keys as u64);
        assert_eq!(result.bytes_to_free_after, 0);
    }

    #[test]
    fn fr_p2c_008_u013_safety_gate_suppresses_eviction() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), vec![b'x'; 96], None, 0);
        store.set(b"b".to_vec(), vec![b'y'; 96], None, 0);
        let before_dbsize = store.dbsize_in_db(0);

        let result = store.run_bounded_eviction_loop(
            0,
            64,
            0,
            4,
            8,
            EvictionSafetyGateState {
                loading: true,
                ..EvictionSafetyGateState::default()
            },
        );

        assert_eq!(result.status, EvictionLoopStatus::Fail);
        assert_eq!(
            result.failure,
            Some(EvictionLoopFailure::SafetyGateSuppressed)
        );
        assert_eq!(store.dbsize_in_db(0), before_dbsize);
        assert_eq!(result.evicted_keys, 0);
    }

    #[test]
    fn state_digest_changes_on_mutation() {
        let mut store = Store::new();
        let digest_a = store.state_digest();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let digest_b = store.state_digest();
        assert_ne!(digest_a, digest_b);
        store.del(&[b"k".to_vec()], 0);
        let digest_c = store.state_digest();
        assert_ne!(digest_b, digest_c);
    }

    #[test]
    fn state_digest_matches_full_scan_after_direct_mutation_paths() {
        fn assert_digest_matches(store: &mut Store) {
            let expected = format!("{:016x}", store.state_digest_full_scan());
            assert_eq!(store.state_digest(), expected);
        }

        let mut store = Store::new();

        store.set(b"ttl".to_vec(), b"value".to_vec(), None, 0);
        store
            .getex(b"ttl", Some(Some(5_000)), 100)
            .expect("getex should set expiry");
        assert_digest_matches(&mut store);

        store
            .rpush(b"list", &[b"a".to_vec(), b"b".to_vec()], 0)
            .expect("rpush");
        store.lpushx(b"list", &[b"c".to_vec()], 0).expect("lpushx");
        store.rpoplpush(b"list", b"list2", 0).expect("rpoplpush");
        store
            .lmove(b"list2", b"list", b"LEFT", b"RIGHT", 0)
            .expect("lmove");
        assert_digest_matches(&mut store);

        store
            .sadd(b"set", &[b"a".to_vec(), b"b".to_vec()], 0)
            .expect("sadd");
        store.spop(b"set", 0).expect("spop");
        store
            .sadd(b"dst", &[b"z".to_vec()], 0)
            .expect("seed destination set");
        store.smove(b"set", b"dst", b"b", 0).expect("smove");
        assert_digest_matches(&mut store);

        store
            .zadd(b"z", &[(1.0, b"one".to_vec()), (2.0, b"two".to_vec())], 0)
            .expect("zadd");
        store
            .zpopmax_count(b"z", 1, 0)
            .expect("zpopmax_count should succeed");
        assert_digest_matches(&mut store);

        store
            .xadd(b"stream", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .expect("xadd 1");
        store
            .xadd(b"stream", (2, 0), &[(b"f".to_vec(), b"v2".to_vec())], 0)
            .expect("xadd 2");
        store.xtrim(b"stream", 1, None, 0).expect("xtrim");
        assert_digest_matches(&mut store);
    }

    #[test]
    fn state_digest_stays_stale_after_incremental_update_follows_direct_mutation() {
        let mut store = Store::new();
        store
            .rpush(b"list", &[b"a".to_vec()], 0)
            .expect("seed list");
        store
            .rpush(b"list", &[b"b".to_vec()], 0)
            .expect("mutate list in place");
        store.set(b"other".to_vec(), b"value".to_vec(), None, 0);

        let expected = format!("{:016x}", store.state_digest_full_scan());
        assert_eq!(store.state_digest(), expected);
    }

    #[test]
    fn smove_same_source_and_destination_is_a_membership_check() {
        let mut store = Store::new();
        store
            .sadd(b"s", &[b"a".to_vec(), b"b".to_vec()], 0)
            .expect("seed set");

        assert!(store.smove(b"s", b"s", b"a", 0).expect("existing member"));
        assert_eq!(
            store.smembers(b"s", 0).expect("members"),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert!(!store.smove(b"s", b"s", b"z", 0).expect("missing member"));
        assert_eq!(store.scard(b"s", 0).expect("set cardinality"), 2);
    }

    #[test]
    fn append_creates_or_extends() {
        let mut store = Store::new();
        assert_eq!(store.append(b"k", b"hello", 0).unwrap(), 5);
        assert_eq!(store.append(b"k", b" world", 0).unwrap(), 11);
        assert_eq!(store.get(b"k", 0).unwrap(), Some(b"hello world".to_vec()));
    }

    #[test]
    fn strlen_returns_length_or_zero() {
        let mut store = Store::new();
        assert_eq!(store.strlen(b"missing", 0).unwrap(), 0);
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 0);
        assert_eq!(store.strlen(b"k", 0).unwrap(), 5);
    }

    #[test]
    fn mget_returns_values_or_none() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), None, 0);
        let result = store.mget(&[b"a", b"b", b"c"], 0);
        assert_eq!(
            result,
            vec![Some(b"1".to_vec()), None, Some(b"3".to_vec()),]
        );
    }

    #[test]
    fn setnx_only_sets_if_absent() {
        let mut store = Store::new();
        assert!(store.setnx(b"k".to_vec(), b"v1".to_vec(), 0));
        assert!(!store.setnx(b"k".to_vec(), b"v2".to_vec(), 0));
        assert_eq!(store.get(b"k", 0).unwrap(), Some(b"v1".to_vec()));
    }

    #[test]
    fn getset_returns_old_and_sets_new() {
        let mut store = Store::new();
        assert_eq!(
            store.getset(b"k".to_vec(), b"v1".to_vec(), 0).unwrap(),
            None
        );
        assert_eq!(
            store.getset(b"k".to_vec(), b"v2".to_vec(), 0).unwrap(),
            Some(b"v1".to_vec())
        );
        assert_eq!(store.get(b"k", 0).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn getset_clears_existing_ttl() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v1".to_vec(), Some(5_000), 1_000);
        assert_eq!(store.pttl(b"k", 1_000), PttlValue::Remaining(5_000));

        assert_eq!(
            store.getset(b"k".to_vec(), b"v2".to_vec(), 2_000).unwrap(),
            Some(b"v1".to_vec())
        );
        assert_eq!(store.get(b"k", 2_000).unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.pttl(b"k", 2_000), PttlValue::NoExpiry);
        assert_eq!(store.get(b"k", 6_001).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn incrby_adds_delta() {
        let mut store = Store::new();
        assert_eq!(store.incrby(b"n", 5, 0).expect("incrby"), 5);
        assert_eq!(store.incrby(b"n", -3, 0).expect("incrby"), 2);
        assert_eq!(store.incrby(b"n", -10, 0).expect("incrby"), -8);
    }

    #[test]
    fn persist_removes_expiry() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5000), 1000);
        assert_eq!(store.pttl(b"k", 1000), PttlValue::Remaining(5000));
        assert!(store.persist(b"k", 1000));
        assert_eq!(store.pttl(b"k", 1000), PttlValue::NoExpiry);
        // persist returns false if no expiry or key missing
        assert!(!store.persist(b"k", 1000));
        assert!(!store.persist(b"missing", 1000));
    }

    #[test]
    fn key_type_returns_string_or_none() {
        let mut store = Store::new();
        assert_eq!(store.key_type(b"missing", 0), None);
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(store.key_type(b"k", 0), Some("string"));
    }

    #[test]
    fn value_type_returns_string_or_none() {
        let mut store = Store::new();
        assert_eq!(store.value_type(b"missing", 0), None);
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(store.value_type(b"k", 0), Some(ValueType::String));
    }

    #[test]
    fn type_and_encoding_update_keyspace_stats_without_touching_lru() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        store.reset_info_stats();

        assert_eq!(store.key_type(b"k", 200), Some("string"));
        assert_eq!(store.stat_keyspace_hits, 1);
        assert_eq!(store.stat_keyspace_misses, 0);
        assert_eq!(
            store
                .entries
                .get(b"k".as_ref())
                .expect("type entry")
                .last_access_ms,
            100
        );

        assert_eq!(store.object_encoding(b"k", 250), Some("embstr"));
        assert_eq!(store.stat_keyspace_hits, 2);
        assert_eq!(
            store
                .entries
                .get(b"k".as_ref())
                .expect("encoding entry")
                .last_access_ms,
            100
        );

        assert_eq!(store.key_type(b"missing", 300), None);
        assert_eq!(store.stat_keyspace_misses, 1);
    }

    #[test]
    fn object_idletime_updates_keyspace_stats_without_touching_lru() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        store.reset_info_stats();

        assert_eq!(store.object_idletime(b"k", 2_100), Some(2));
        assert_eq!(store.stat_keyspace_hits, 1);
        assert_eq!(store.stat_keyspace_misses, 0);
        assert_eq!(
            store
                .entries
                .get(b"k".as_ref())
                .expect("idletime entry")
                .last_access_ms,
            100
        );

        assert_eq!(store.object_idletime(b"missing", 2_100), None);
        assert_eq!(store.stat_keyspace_misses, 1);
    }

    #[test]
    fn object_freq_tracks_lfu_accesses_and_preserves_lru_switch_behavior() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        assert_eq!(store.object_freq(b"k", 150), Some(0));

        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
        assert_eq!(store.object_freq(b"k", 200), Some(0));

        assert_eq!(store.get(b"k", 300).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.object_freq(b"k", 301), Some(1));

        store.set(b"k".to_vec(), b"v2".to_vec(), None, 400);
        assert_eq!(store.object_freq(b"k", 401), Some(2));

        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLru;
        assert_eq!(store.object_freq(b"k", 500), Some(2));
    }

    #[test]
    fn object_freq_decays_before_reporting_and_before_next_access_bump() {
        let mut store = Store::new();
        store.maxmemory_policy = MaxmemoryPolicy::AllkeysLfu;
        store.lfu_decay_time = 1;
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);

        assert_eq!(store.get(b"k", 1).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.object_freq(b"k", 2), Some(1));
        assert_eq!(store.object_freq(b"k", 60_000), Some(0));

        assert_eq!(store.get(b"k", 60_001).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.object_freq(b"k", 60_002), Some(1));

        store.lfu_decay_time = 2;
        assert_eq!(store.object_freq(b"k", 179_999), Some(1));
        assert_eq!(store.object_freq(b"k", 180_000), Some(0));
    }

    #[test]
    fn rename_moves_key() {
        let mut store = Store::new();
        store.set(b"old".to_vec(), b"v".to_vec(), None, 0);
        store.rename(b"old", b"new", 0).expect("rename");
        assert_eq!(store.get(b"old", 0).unwrap(), None);
        assert_eq!(store.get(b"new", 0).unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn rename_retains_expiry_deadline() {
        let mut store = Store::new();
        store.set(b"old".to_vec(), b"v".to_vec(), Some(5_000), 1_000);
        assert_eq!(store.pttl(b"old", 1_000), PttlValue::Remaining(5_000));

        store.rename(b"old", b"new", 1_000).expect("rename");
        assert_eq!(store.get(b"old", 1_000).unwrap(), None);
        assert_eq!(store.pttl(b"new", 1_000), PttlValue::Remaining(5_000));
        assert_eq!(store.pttl(b"new", 5_999), PttlValue::Remaining(1));
        assert_eq!(store.get(b"new", 6_001).unwrap(), None);
    }

    #[test]
    fn rename_missing_key_errors() {
        let mut store = Store::new();
        let err = store
            .rename(b"missing", b"new", 0)
            .expect_err("should fail");
        assert_eq!(err, StoreError::KeyNotFound);
    }

    #[test]
    fn rename_stream_groups_cleaned_on_overwrite() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap();
        assert!(store.stream_groups.contains_key(b"s".as_slice()));

        // Overwrite the stream key with a string via RENAME.
        store.set(b"k".to_vec(), b"string".to_vec(), None, 0);
        store.rename(b"k", b"s", 0).unwrap();

        // Stream groups for "s" should be cleaned up since it's no longer a stream.
        assert!(
            !store.stream_groups.contains_key(b"s".as_slice()),
            "stream groups should be removed when key is overwritten"
        );
    }

    #[test]
    fn renamenx_only_if_newkey_absent() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        assert!(!store.renamenx(b"a", b"b", 0).expect("renamenx"));
        assert_eq!(store.get(b"a", 0).unwrap(), Some(b"1".to_vec()));
        assert!(store.renamenx(b"a", b"c", 0).expect("renamenx"));
        assert_eq!(store.get(b"a", 0).unwrap(), None);
        assert_eq!(store.get(b"c", 0).unwrap(), Some(b"1".to_vec()));
    }

    #[test]
    fn renamenx_missing_key_errors() {
        let mut store = Store::new();
        let err = store.renamenx(b"missing", b"new", 0).expect_err("renamenx");
        assert_eq!(err, StoreError::KeyNotFound);
    }

    #[test]
    fn keys_matching_with_glob() {
        let mut store = Store::new();
        store.set(encode_db_key(0, b"hello"), b"1".to_vec(), None, 0);
        store.set(encode_db_key(0, b"hallo"), b"2".to_vec(), None, 0);
        store.set(encode_db_key(0, b"world"), b"3".to_vec(), None, 0);
        let result = store.keys_matching_in_db(0, b"h?llo", 0);
        assert_eq!(result, vec![b"hallo".to_vec(), b"hello".to_vec()]);
        let result = store.keys_matching_in_db(0, b"*", 0);
        assert_eq!(result.len(), 3);
        let result = store.keys_matching_in_db(0, b"h*", 0);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn keys_matching_malformed_class_contract_matches_redis() {
        let mut store = Store::new();
        store.set(encode_db_key(0, b"a"), b"1".to_vec(), None, 0);
        store.set(encode_db_key(0, b"b"), b"2".to_vec(), None, 0);
        store.set(encode_db_key(0, b"c"), b"3".to_vec(), None, 0);
        store.set(encode_db_key(0, b"[abc"), b"1".to_vec(), None, 0);
        // Redis treats malformed "[abc" as a class of bytes {'a','b','c'}.
        assert_eq!(
            store.keys_matching_in_db(0, b"[abc", 0),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        // The malformed class does not match literal '[' prefixed keys.
        assert!(
            !store
                .keys_matching_in_db(0, b"[abc", 0)
                .iter()
                .any(|k| k == b"[abc")
        );
        // "[a-" is malformed too; with this key set Redis matches only 'a'.
        assert_eq!(store.keys_matching_in_db(0, b"[a-", 0), vec![b"a".to_vec()]);
    }

    #[test]
    fn keys_matching_range_and_escape_contract_matches_redis() {
        let mut store = Store::new();
        store.set(encode_db_key(0, b"!"), b"0".to_vec(), None, 0);
        store.set(encode_db_key(0, b"a"), b"1".to_vec(), None, 0);
        store.set(encode_db_key(0, b"b"), b"6".to_vec(), None, 0);
        store.set(encode_db_key(0, b"m"), b"2".to_vec(), None, 0);
        store.set(encode_db_key(0, b"z"), b"3".to_vec(), None, 0);
        store.set(encode_db_key(0, b"-"), b"4".to_vec(), None, 0);
        store.set(encode_db_key(0, b"]"), b"5".to_vec(), None, 0);

        assert_eq!(
            store.keys_matching_in_db(0, b"[z-a]", 0),
            vec![b"a".to_vec(), b"b".to_vec(), b"m".to_vec(), b"z".to_vec()]
        );
        assert_eq!(
            store.keys_matching_in_db(0, b"[\\-]", 0),
            vec![b"-".to_vec()]
        );
        assert_eq!(
            store.keys_matching_in_db(0, b"[a-]", 0),
            vec![b"]".to_vec(), b"a".to_vec()]
        );
        assert_eq!(
            store.keys_matching_in_db(0, b"[!a]", 0),
            vec![b"!".to_vec(), b"a".to_vec()]
        );
    }

    #[test]
    fn keys_matching_skips_expired_entries() {
        let mut store = Store::new();
        store.set(encode_db_key(0, b"live"), b"1".to_vec(), None, 0);
        store.set(encode_db_key(0, b"soon"), b"2".to_vec(), Some(50), 0);
        store.set(encode_db_key(0, b"later"), b"3".to_vec(), Some(500), 0);

        let result = store.keys_matching_in_db(0, b"*", 100);
        assert_eq!(result, vec![b"later".to_vec(), b"live".to_vec()]);
    }

    #[test]
    fn dbsize_counts_live_keys() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), Some(100), 0);
        assert_eq!(store.dbsize_in_db(0), 2);

        // dbsize is O(1) and does not actively reap expired keys.
        // It should still return 2 even if 'b' is logically expired,
        // until 'b' is actively or lazily reaped.
        assert_eq!(store.dbsize_in_db(0), 2);

        // Lazy reap
        store.get(b"b", 200).unwrap();
        assert_eq!(store.dbsize_in_db(0), 1);
    }

    #[test]
    fn flushdb_clears_all() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.flushdb();
        assert!(store.is_empty());
    }

    #[test]
    fn glob_match_patterns() {
        use super::glob_match;
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"h?llo", b"hello"));
        assert!(glob_match(b"h?llo", b"hallo"));
        assert!(!glob_match(b"h?llo", b"hllo"));
        assert!(glob_match(b"h[ae]llo", b"hello"));
        assert!(glob_match(b"h[ae]llo", b"hallo"));
        assert!(!glob_match(b"h[ae]llo", b"hillo"));
        assert!(glob_match(b"h[^e]llo", b"hallo"));
        assert!(!glob_match(b"h[^e]llo", b"hello"));
        assert!(glob_match(b"h[a-e]llo", b"hcllo"));
        assert!(!glob_match(b"h[a-e]llo", b"hzllo"));
        assert!(glob_match(b"foo*bar", b"fooXYZbar"));
        assert!(glob_match(b"foo*bar", b"foobar"));
        assert!(glob_match(b"\\*literal", b"*literal"));
        assert!(glob_match(b"[z-a]", b"m"));
        assert!(glob_match(b"[\\-]", b"-"));
        assert!(glob_match(b"[a-]", b"]"));
        assert!(glob_match(b"[a-]", b"a"));
        assert!(glob_match(b"[abc", b"a"));
        assert!(glob_match(b"[abc", b"c"));
        assert!(!glob_match(b"[abc", b"["));
        assert!(glob_match(b"[!a]", b"!"));
        assert!(glob_match(b"[!a]", b"a"));
        assert!(!glob_match(b"[!a]", b"b"));
        assert!(!glob_match(b"[literal", b"[literal"));
        assert!(!glob_match(b"[a-", b"[a-"));
        assert!(!glob_match(b"[literal", b"literal"));
    }

    // ── Hash operation tests ────────────────────────────────

    #[test]
    fn hset_and_hget() {
        let mut store = Store::new();
        assert!(store.hset(b"h", b"f1".to_vec(), b"v1".to_vec(), 0).unwrap());
        assert!(!store.hset(b"h", b"f1".to_vec(), b"v2".to_vec(), 0).unwrap());
        assert_eq!(store.hget(b"h", b"f1", 0).unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.hget(b"h", b"missing", 0).unwrap(), None);
        assert_eq!(store.hget(b"nokey", b"f1", 0).unwrap(), None);
    }

    #[test]
    fn hdel_removes_fields_and_cleans_empty_hash() {
        let mut store = Store::new();
        store.hset(b"h", b"f1".to_vec(), b"v1".to_vec(), 0).unwrap();
        store.hset(b"h", b"f2".to_vec(), b"v2".to_vec(), 0).unwrap();
        assert_eq!(store.hdel(b"h", &[b"f1", b"missing"], 0).unwrap(), 1);
        assert_eq!(store.hlen(b"h", 0).unwrap(), 1);
        assert_eq!(store.hdel(b"h", &[b"f2"], 0).unwrap(), 1);
        assert!(!store.exists(b"h", 0));
    }

    #[test]
    fn hexists_and_hlen() {
        let mut store = Store::new();
        assert!(!store.hexists(b"h", b"f1", 0).unwrap());
        assert_eq!(store.hlen(b"h", 0).unwrap(), 0);
        store.hset(b"h", b"f1".to_vec(), b"v1".to_vec(), 0).unwrap();
        assert!(store.hexists(b"h", b"f1", 0).unwrap());
        assert_eq!(store.hlen(b"h", 0).unwrap(), 1);
    }

    #[test]
    fn hgetall_returns_sorted_pairs() {
        let mut store = Store::new();
        store.hset(b"h", b"b".to_vec(), b"2".to_vec(), 0).unwrap();
        store.hset(b"h", b"a".to_vec(), b"1".to_vec(), 0).unwrap();
        let pairs = store.hgetall(b"h", 0).unwrap();
        assert_eq!(
            pairs,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec())
            ]
        );
    }

    #[test]
    fn hkeys_and_hvals() {
        let mut store = Store::new();
        store.hset(b"h", b"b".to_vec(), b"2".to_vec(), 0).unwrap();
        store.hset(b"h", b"a".to_vec(), b"1".to_vec(), 0).unwrap();
        assert_eq!(
            store.hkeys(b"h", 0).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert_eq!(
            store.hvals(b"h", 0).unwrap(),
            vec![b"1".to_vec(), b"2".to_vec()]
        );
    }

    #[test]
    fn hmget_returns_values_or_none() {
        let mut store = Store::new();
        store.hset(b"h", b"a".to_vec(), b"1".to_vec(), 0).unwrap();
        let result = store.hmget(b"h", &[b"a", b"missing"], 0).unwrap();
        assert_eq!(result, vec![Some(b"1".to_vec()), None]);
        let result = store.hmget(b"nokey", &[b"a"], 0).unwrap();
        assert_eq!(result, vec![None]);
    }

    #[test]
    fn hincrby_creates_and_increments() {
        let mut store = Store::new();
        assert_eq!(store.hincrby(b"h", b"n", 5, 0).unwrap(), 5);
        assert_eq!(store.hincrby(b"h", b"n", -3, 0).unwrap(), 2);
    }

    #[test]
    fn hsetnx_only_sets_if_absent() {
        let mut store = Store::new();
        assert!(
            store
                .hsetnx(b"h", b"f".to_vec(), b"v1".to_vec(), 0)
                .unwrap()
        );
        assert!(
            !store
                .hsetnx(b"h", b"f".to_vec(), b"v2".to_vec(), 0)
                .unwrap()
        );
        assert_eq!(store.hget(b"h", b"f", 0).unwrap(), Some(b"v1".to_vec()));
    }

    #[test]
    fn hstrlen_returns_field_length() {
        let mut store = Store::new();
        assert_eq!(store.hstrlen(b"h", b"f", 0).unwrap(), 0);
        store
            .hset(b"h", b"f".to_vec(), b"hello".to_vec(), 0)
            .unwrap();
        assert_eq!(store.hstrlen(b"h", b"f", 0).unwrap(), 5);
    }

    #[test]
    fn hash_type_is_reported_correctly() {
        let mut store = Store::new();
        store.hset(b"h", b"f".to_vec(), b"v".to_vec(), 0).unwrap();
        assert_eq!(store.value_type(b"h", 0), Some(ValueType::Hash));
        assert_eq!(store.key_type(b"h", 0), Some("hash"));
    }

    // ── List operation tests ────────────────────────────────

    #[test]
    fn lpush_rpush_lpop_rpop() {
        let mut store = Store::new();
        assert_eq!(
            store
                .lpush(b"l", &[b"a".to_vec(), b"b".to_vec()], 0)
                .unwrap(),
            2
        );
        assert_eq!(store.rpush(b"l", &[b"c".to_vec()], 0).unwrap(), 3);
        assert_eq!(store.lpop(b"l", 0).unwrap(), Some(b"b".to_vec()));
        assert_eq!(store.rpop(b"l", 0).unwrap(), Some(b"c".to_vec()));
        assert_eq!(store.llen(b"l", 0).unwrap(), 1);
    }

    #[test]
    fn lrange_with_negative_indices() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        assert_eq!(
            store.lrange(b"l", 0, -1, 0).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            store.lrange(b"l", -2, -1, 0).unwrap(),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(store.lrange(b"l", 0, 0, 0).unwrap(), vec![b"a".to_vec()]);
        // Redis parity: stop < -len or stop < start returns empty array
        assert!(store.lrange(b"l", 0, -100, 0).unwrap().is_empty());
        assert!(store.lrange(b"l", 1, 0, 0).unwrap().is_empty());
    }

    #[test]
    fn lindex_and_lset() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        assert_eq!(store.lindex(b"l", 1, 0).unwrap(), Some(b"b".to_vec()));
        assert_eq!(store.lindex(b"l", -1, 0).unwrap(), Some(b"c".to_vec()));
        store.lset(b"l", 1, b"B".to_vec(), 0).unwrap();
        assert_eq!(store.lindex(b"l", 1, 0).unwrap(), Some(b"B".to_vec()));
    }

    #[test]
    fn lpop_rpop_removes_empty_key() {
        let mut store = Store::new();
        store.rpush(b"l", &[b"a".to_vec()], 0).unwrap();
        assert_eq!(store.lpop(b"l", 0).unwrap(), Some(b"a".to_vec()));
        assert!(!store.exists(b"l", 0));
        assert_eq!(store.lpop(b"l", 0).unwrap(), None);
    }

    #[test]
    fn lpop_rpop_count_handles_missing_and_empty() {
        let mut store = Store::new();
        assert_eq!(store.lpop_count(b"missing", 2, 0).unwrap(), None);
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        assert_eq!(
            store.lpop_count(b"l", 2, 0).unwrap(),
            Some(vec![b"a".to_vec(), b"b".to_vec()])
        );
        assert_eq!(
            store.rpop_count(b"l", 5, 0).unwrap(),
            Some(vec![b"c".to_vec()])
        );
        assert!(!store.exists(b"l", 0));
    }

    #[test]
    fn ltrim_keeps_window_and_removes_empty_key() {
        let mut store = Store::new();
        store
            .rpush(
                b"l",
                &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()],
                0,
            )
            .unwrap();

        store.ltrim(b"l", 1, 2, 0).unwrap();
        assert_eq!(
            store.lrange(b"l", 0, -1, 0).unwrap(),
            vec![b"b".to_vec(), b"c".to_vec()]
        );

        // Redis parity: LTRIM 0 -100 clears the list
        store.ltrim(b"l", 0, -100, 0).unwrap();
        assert!(!store.exists(b"l", 0));
    }

    #[test]
    fn lpushx_rpushx_require_existing_key() {
        let mut store = Store::new();
        assert_eq!(store.lpushx(b"missing", &[b"x".to_vec()], 0).unwrap(), 0);
        assert_eq!(store.rpushx(b"missing", &[b"y".to_vec()], 0).unwrap(), 0);
        assert!(!store.exists(b"missing", 0));

        store.rpush(b"l", &[b"a".to_vec()], 0).unwrap();
        assert_eq!(
            store
                .lpushx(b"l", &[b"b".to_vec(), b"c".to_vec()], 0)
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .rpushx(b"l", &[b"d".to_vec(), b"e".to_vec()], 0)
                .unwrap(),
            5
        );
        assert_eq!(
            store.lrange(b"l", 0, -1, 0).unwrap(),
            vec![
                b"c".to_vec(),
                b"b".to_vec(),
                b"a".to_vec(),
                b"d".to_vec(),
                b"e".to_vec()
            ]
        );
    }

    #[test]
    fn lmove_moves_between_lists_and_handles_missing_source() {
        let mut store = Store::new();
        store
            .rpush(b"src", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        store.rpush(b"dst", &[b"x".to_vec()], 0).unwrap();

        let moved = store.lmove(b"src", b"dst", b"LEFT", b"RIGHT", 0).unwrap();
        assert_eq!(moved, Some(b"a".to_vec()));

        let moved = store.lmove(b"src", b"dst", b"RIGHT", b"LEFT", 0).unwrap();
        assert_eq!(moved, Some(b"c".to_vec()));

        assert_eq!(store.lrange(b"src", 0, -1, 0).unwrap(), vec![b"b".to_vec()]);
        assert_eq!(
            store.lrange(b"dst", 0, -1, 0).unwrap(),
            vec![b"c".to_vec(), b"x".to_vec(), b"a".to_vec()]
        );

        let moved = store
            .lmove(b"missing", b"dst", b"LEFT", b"RIGHT", 0)
            .unwrap();
        assert_eq!(moved, None);
    }

    #[test]
    fn lmove_wrongtype_destination_is_non_mutating() {
        let mut store = Store::new();
        store
            .rpush(b"src", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        store.set(b"dst".to_vec(), b"value".to_vec(), None, 0);

        let err = store.lmove(b"src", b"dst", b"LEFT", b"RIGHT", 0);
        assert_eq!(err, Err(StoreError::WrongType));
        assert_eq!(
            store.lrange(b"src", 0, -1, 0).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn list_type_is_reported_correctly() {
        let mut store = Store::new();
        store.rpush(b"l", &[b"a".to_vec()], 0).unwrap();
        assert_eq!(store.value_type(b"l", 0), Some(ValueType::List));
        assert_eq!(store.key_type(b"l", 0), Some("list"));
    }

    // ── Set operation tests ─────────────────────────────────

    #[test]
    fn sadd_srem_scard_sismember() {
        let mut store = Store::new();
        assert_eq!(
            store
                .sadd(b"s", &[b"a".to_vec(), b"b".to_vec(), b"a".to_vec()], 0)
                .unwrap(),
            2
        );
        assert_eq!(store.scard(b"s", 0).unwrap(), 2);
        assert!(store.sismember(b"s", b"a", 0).unwrap());
        assert!(!store.sismember(b"s", b"c", 0).unwrap());
        assert_eq!(store.srem(b"s", &[b"a", b"missing"], 0).unwrap(), 1);
        assert_eq!(store.scard(b"s", 0).unwrap(), 1);
    }

    #[test]
    fn smembers_returns_sorted() {
        let mut store = Store::new();
        store
            .sadd(b"s", &[b"c".to_vec(), b"a".to_vec(), b"b".to_vec()], 0)
            .unwrap();
        assert_eq!(
            store.smembers(b"s", 0).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn srem_removes_empty_set_key() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"a".to_vec()], 0).unwrap();
        store.srem(b"s", &[b"a"], 0).unwrap();
        assert!(!store.exists(b"s", 0));
    }

    #[test]
    fn set_type_is_reported_correctly() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"a".to_vec()], 0).unwrap();
        assert_eq!(store.value_type(b"s", 0), Some(ValueType::Set));
        assert_eq!(store.key_type(b"s", 0), Some("set"));
    }

    // ── WrongType tests ─────────────────────────────────────

    #[test]
    fn wrongtype_string_on_hash() {
        let mut store = Store::new();
        store.hset(b"h", b"f".to_vec(), b"v".to_vec(), 0).unwrap();
        assert_eq!(store.get(b"h", 0), Err(StoreError::WrongType));
        assert_eq!(store.append(b"h", b"x", 0), Err(StoreError::WrongType));
        assert_eq!(store.strlen(b"h", 0), Err(StoreError::WrongType));
        assert_eq!(store.incr(b"h", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn wrongtype_hash_on_string() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(store.hget(b"k", b"f", 0), Err(StoreError::WrongType));
        assert_eq!(
            store.hset(b"k", b"f".to_vec(), b"v".to_vec(), 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(store.hlen(b"k", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn wrongtype_list_on_string() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(
            store.lpush(b"k", &[b"x".to_vec()], 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(
            store.rpush(b"k", &[b"x".to_vec()], 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(store.llen(b"k", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn wrongtype_set_on_string() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(
            store.sadd(b"k", &[b"x".to_vec()], 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(store.scard(b"k", 0), Err(StoreError::WrongType));
        assert_eq!(store.sismember(b"k", b"x", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn zadd_and_zscore() {
        let mut store = Store::new();
        let added = store
            .zadd(b"z", &[(1.0, b"a".to_vec()), (2.0, b"b".to_vec())], 0)
            .unwrap();
        assert_eq!(added, 2);
        assert_eq!(store.zscore(b"z", b"a", 0).unwrap(), Some(1.0));
        assert_eq!(store.zscore(b"z", b"b", 0).unwrap(), Some(2.0));
        assert_eq!(store.zscore(b"z", b"c", 0).unwrap(), None);
        // Update existing member score: count stays 0
        let added2 = store.zadd(b"z", &[(3.0, b"a".to_vec())], 0).unwrap();
        assert_eq!(added2, 0);
        assert_eq!(store.zscore(b"z", b"a", 0).unwrap(), Some(3.0));
    }

    #[test]
    fn zadd_canonicalizes_negative_zero_scores() {
        let mut store = Store::new();
        assert_eq!(store.zadd(b"z", &[(-0.0, b"a".to_vec())], 0).unwrap(), 1);

        let score = store.zscore(b"z", b"a", 0).unwrap().unwrap();
        assert_eq!(score, 0.0);
        assert!(!score.is_sign_negative());

        assert_eq!(store.zadd(b"z", &[(0.0, b"a".to_vec())], 0).unwrap(), 0);
        let pairs = store
            .zrangebyscore_withscores(
                b"z",
                ScoreBound::Inclusive(f64::NEG_INFINITY),
                ScoreBound::Inclusive(f64::INFINITY),
                0,
            )
            .unwrap();
        assert_eq!(pairs, vec![(b"a".to_vec(), 0.0)]);
    }

    #[test]
    fn zrangebyscore_treats_negative_zero_bounds_as_zero() {
        let mut store = Store::new();
        store.zadd(b"z", &[(0.0, b"a".to_vec())], 0).unwrap();

        let pairs = store
            .zrangebyscore_withscores(
                b"z",
                ScoreBound::Inclusive(-0.0),
                ScoreBound::Inclusive(-0.0),
                0,
            )
            .unwrap();
        assert_eq!(pairs, vec![(b"a".to_vec(), 0.0)]);
    }

    #[test]
    fn zrangebyscore_long_member_bug_repro() {
        let mut store = Store::new();
        let long_member = vec![255; 2000];
        store
            .zadd(
                b"z",
                &[
                    (10.0, b"short".to_vec()),
                    (10.0, long_member.clone()),
                    (20.0, b"twenty".to_vec()),
                ],
                0,
            )
            .unwrap();

        // 1. Exclusive lower bound bug: score > 10.0
        let range = store
            .zrangebyscore(
                b"z",
                ScoreBound::Exclusive(10.0),
                ScoreBound::Inclusive(25.0),
                0,
            )
            .unwrap();
        assert_eq!(
            range,
            vec![b"twenty".to_vec()],
            "Exclusive lower bound failed for long member"
        );

        // 2. Inclusive upper bound bug: score <= 10.0
        let range2 = store
            .zrangebyscore(
                b"z",
                ScoreBound::Inclusive(0.0),
                ScoreBound::Inclusive(10.0),
                0,
            )
            .unwrap();
        assert_eq!(
            range2,
            vec![b"short".to_vec(), long_member],
            "Inclusive upper bound failed for long member"
        );
    }

    #[test]
    fn zrem_and_zcard() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                    (3.0, b"c".to_vec()),
                ],
                0,
            )
            .unwrap();
        assert_eq!(store.zcard(b"z", 0).unwrap(), 3);
        let removed = store.zrem(b"z", &[b"a", b"d"], 0).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.zcard(b"z", 0).unwrap(), 2);
    }

    #[test]
    fn zrank_and_zrevrank() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                    (3.0, b"c".to_vec()),
                ],
                0,
            )
            .unwrap();
        assert_eq!(store.zrank(b"z", b"a", 0).unwrap(), Some(0));
        assert_eq!(store.zrank(b"z", b"b", 0).unwrap(), Some(1));
        assert_eq!(store.zrank(b"z", b"c", 0).unwrap(), Some(2));
        assert_eq!(store.zrank(b"z", b"d", 0).unwrap(), None);
        assert_eq!(store.zrevrank(b"z", b"c", 0).unwrap(), Some(0));
        assert_eq!(store.zrevrank(b"z", b"b", 0).unwrap(), Some(1));
        assert_eq!(store.zrevrank(b"z", b"a", 0).unwrap(), Some(2));
    }

    #[test]
    fn zrange_and_zrevrange() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (3.0, b"c".to_vec()),
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                ],
                0,
            )
            .unwrap();
        let range = store.zrange(b"z", 0, -1, 0).unwrap();
        assert_eq!(range, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
        let rev = store.zrevrange(b"z", 0, -1, 0).unwrap();
        assert_eq!(rev, vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
        let sub = store.zrange(b"z", 0, 1, 0).unwrap();
        assert_eq!(sub, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn zrangebyscore_and_zcount() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                    (3.0, b"c".to_vec()),
                    (4.0, b"d".to_vec()),
                ],
                0,
            )
            .unwrap();
        let range = store
            .zrangebyscore(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(range, vec![b"b".to_vec(), b"c".to_vec()]);
        let count = store
            .zcount(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn zset_score_range_paths_ignore_corrupted_sentinel_entries() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                    (3.0, b"c".to_vec()),
                    (4.0, b"d".to_vec()),
                ],
                0,
            )
            .unwrap();

        let entry = store.entries.get_mut(b"z".as_slice()).expect("zset entry");
        assert!(matches!(entry.value, Value::SortedSet(_)));
        let zs = match &mut entry.value {
            Value::SortedSet(zs) => zs,
            _ => return,
        };
        zs.ordered.insert(ScoreMember::min_for_score(2.0), ());
        zs.ordered.insert(ScoreMember::max_for_score(3.0), ());

        let range = store
            .zrangebyscore(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(range, vec![b"b".to_vec(), b"c".to_vec()]);

        let with_scores = store
            .zrangebyscore_withscores(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(
            with_scores,
            vec![(b"b".to_vec(), 2.0), (b"c".to_vec(), 3.0)]
        );

        let rev_with_scores = store
            .zrevrangebyscore_withscores(b"z", 3.0, 2.0, 0)
            .unwrap();
        assert_eq!(
            rev_with_scores,
            vec![(b"c".to_vec(), 3.0), (b"b".to_vec(), 2.0)]
        );

        let count = store
            .zcount(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(count, 2);

        let removed = store
            .zremrangebyscore(
                b"z",
                ScoreBound::Inclusive(2.0),
                ScoreBound::Inclusive(3.0),
                0,
            )
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(
            store.zrange(b"z", 0, -1, 0).unwrap(),
            vec![b"a".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn zincrby_creates_and_increments() {
        let mut store = Store::new();
        let score = store.zincrby(b"z", b"m".to_vec(), 5.0, 0).unwrap();
        assert_eq!(score, 5.0);
        let score = store.zincrby(b"z", b"m".to_vec(), 2.5, 0).unwrap();
        assert_eq!(score, 7.5);
        let inf = store
            .zincrby(b"z", b"m".to_vec(), f64::INFINITY, 0)
            .unwrap();
        assert_eq!(inf, f64::INFINITY);
        let nan_err = store
            .zincrby(b"z", b"m".to_vec(), f64::NEG_INFINITY, 0)
            .unwrap_err();
        assert_eq!(nan_err, StoreError::IncrFloatNaN);
    }

    #[test]
    fn zpopmin_and_zpopmax() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (3.0, b"c".to_vec()),
                    (2.0, b"b".to_vec()),
                ],
                0,
            )
            .unwrap();
        let min = store.zpopmin(b"z", 0).unwrap();
        assert_eq!(min, Some((b"a".to_vec(), 1.0)));
        let max = store.zpopmax(b"z", 0).unwrap();
        assert_eq!(max, Some((b"c".to_vec(), 3.0)));
        assert_eq!(store.zcard(b"z", 0).unwrap(), 1);
    }

    #[test]
    fn zset_iter_and_pop_paths_discard_corrupted_sentinel_entries() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"a".to_vec()),
                    (2.0, b"b".to_vec()),
                    (3.0, b"c".to_vec()),
                ],
                0,
            )
            .unwrap();

        let entry = store.entries.get_mut(b"z".as_slice()).expect("zset entry");
        assert!(matches!(entry.value, Value::SortedSet(_)));
        let zs = match &mut entry.value {
            Value::SortedSet(zs) => zs,
            _ => return,
        };
        zs.ordered.insert(ScoreMember::min_for_score(0.0), ());
        zs.ordered.insert(ScoreMember::max_for_score(10.0), ());

        assert_eq!(
            store.zrange(b"z", 0, -1, 0).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            store.zrevrange(b"z", 0, -1, 0).unwrap(),
            vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]
        );

        let min = store.zpopmin(b"z", 0).unwrap();
        assert_eq!(min, Some((b"a".to_vec(), 1.0)));
        let max = store.zpopmax(b"z", 0).unwrap();
        assert_eq!(max, Some((b"c".to_vec(), 3.0)));

        let entry = store
            .entries
            .get(b"z".as_slice())
            .expect("remaining zset entry");
        assert!(matches!(entry.value, Value::SortedSet(_)));
        let zs = match &entry.value {
            Value::SortedSet(zs) => zs,
            _ => return,
        };
        assert!(!zs.ordered.contains_key(&ScoreMember::min_for_score(0.0)));
        assert!(!zs.ordered.contains_key(&ScoreMember::max_for_score(10.0)));
        assert_eq!(store.zrange(b"z", 0, -1, 0).unwrap(), vec![b"b".to_vec()]);
    }

    #[test]
    fn zset_type_is_reported_correctly() {
        let mut store = Store::new();
        store.zadd(b"z", &[(1.0, b"a".to_vec())], 0).unwrap();
        assert_eq!(store.key_type(b"z", 0), Some("zset"));
        assert_eq!(store.value_type(b"z", 0), Some(ValueType::ZSet));
    }

    #[test]
    fn wrongtype_zset_on_string() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        assert_eq!(
            store.zadd(b"k", &[(1.0, b"a".to_vec())], 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(
            store.zincrby(b"k", b"a".to_vec(), 1.0, 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(store.zpopmin(b"k", 0), Err(StoreError::WrongType));
        assert_eq!(store.zpopmax(b"k", 0), Err(StoreError::WrongType));
        assert_eq!(store.zscore(b"k", b"a", 0), Err(StoreError::WrongType));
        assert_eq!(store.zcard(b"k", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn zset_score_ordering_with_ties() {
        let mut store = Store::new();
        // Same score -> sorted by member lexicographically
        store
            .zadd(
                b"z",
                &[
                    (1.0, b"b".to_vec()),
                    (1.0, b"a".to_vec()),
                    (1.0, b"c".to_vec()),
                ],
                0,
            )
            .unwrap();
        let range = store.zrange(b"z", 0, -1, 0).unwrap();
        assert_eq!(range, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn stream_add_len_last_id_and_type() {
        let mut store = Store::new();
        assert_eq!(store.xlen(b"s", 0).unwrap(), 0);
        assert_eq!(store.xlast_id(b"s", 0).unwrap(), None);

        store
            .xadd(
                b"s",
                (1_000, 0),
                &[(b"field1".to_vec(), b"value1".to_vec())],
                0,
            )
            .unwrap();
        // Intentionally insert an older ID after a newer one; store ordering should stay by ID.
        store
            .xadd(
                b"s",
                (999, 9),
                &[(b"field0".to_vec(), b"value0".to_vec())],
                0,
            )
            .unwrap();
        store
            .xadd(
                b"s",
                (1_000, 1),
                &[
                    (b"field2".to_vec(), b"value2".to_vec()),
                    (b"field3".to_vec(), b"value3".to_vec()),
                ],
                0,
            )
            .unwrap();

        assert_eq!(store.xlen(b"s", 0).unwrap(), 3);
        assert_eq!(store.xlast_id(b"s", 0).unwrap(), Some((1_000, 1)));
        assert_eq!(store.key_type(b"s", 0), Some("stream"));
        assert_eq!(store.value_type(b"s", 0), Some(ValueType::Stream));
    }

    #[test]
    fn stream_wrongtype_on_string_key() {
        let mut store = Store::new();
        store.set(b"s".to_vec(), b"value".to_vec(), None, 0);

        assert_eq!(store.xlast_id(b"s", 0), Err(StoreError::WrongType));
        assert_eq!(
            store.xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0),
            Err(StoreError::WrongType)
        );
        assert_eq!(store.xlen(b"s", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn stream_xrange_orders_and_filters_entries() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let all = store
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 0)
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, (1000, 0));
        assert_eq!(all[1].0, (1000, 1));
        assert_eq!(all[2].0, (1001, 0));

        let window = store.xrange(b"s", (1000, 1), (1001, 0), None, 0).unwrap();
        assert_eq!(window.len(), 2);
        assert_eq!(window[0].0, (1000, 1));
        assert_eq!(window[1].0, (1001, 0));
    }

    #[test]
    fn stream_xrange_count_limit_and_wrongtype() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let limited = store
            .xrange(b"s", (1000, 0), (u64::MAX, u64::MAX), Some(2), 0)
            .unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].0, (1000, 0));
        assert_eq!(limited[1].0, (1000, 1));

        assert_eq!(
            store
                .xrange(b"s", (1001, 0), (1000, 0), None, 0)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store
                .xrange(b"missing", (0, 0), (u64::MAX, u64::MAX), None, 0)
                .unwrap()
                .len(),
            0
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xrange(b"str", (0, 0), (u64::MAX, u64::MAX), None, 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xrevrange_orders_descending_and_respects_count() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let all = store
            .xrevrange(b"s", (u64::MAX, u64::MAX), (0, 0), None, 0)
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, (1001, 0));
        assert_eq!(all[1].0, (1000, 1));
        assert_eq!(all[2].0, (1000, 0));

        let limited = store
            .xrevrange(b"s", (1001, 0), (1000, 0), Some(1), 0)
            .unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].0, (1001, 0));
    }

    #[test]
    fn stream_xrevrange_empty_and_wrongtype() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();

        assert_eq!(
            store
                .xrevrange(b"s", (1000, 0), (2000, 0), None, 0)
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store
                .xrevrange(b"missing", (u64::MAX, u64::MAX), (0, 0), None, 0)
                .unwrap()
                .len(),
            0
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xrevrange(b"str", (u64::MAX, u64::MAX), (0, 0), None, 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xdel_removes_existing_ids_and_ignores_missing() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let removed = store
            .xdel(b"s", &[(1000, 1), (9999, 0), (1000, 1)], 0)
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 2);

        let remaining = store
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 0)
            .unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].0, (1000, 0));
        assert_eq!(remaining[1].0, (1001, 0));
    }

    #[test]
    fn stream_xdel_missing_key_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(store.xdel(b"missing", &[(1, 0)], 0).unwrap(), 0);

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(store.xdel(b"str", &[(1, 0)], 0), Err(StoreError::WrongType));
    }

    #[test]
    fn stream_xtrim_maxlen_removes_oldest_entries() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let removed = store.xtrim(b"s", 2, None, 0).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 2);

        let remaining = store
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 0)
            .unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].0, (1000, 1));
        assert_eq!(remaining[1].0, (1001, 0));
    }

    #[test]
    fn stream_xtrim_zero_missing_and_wrongtype() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();

        assert_eq!(store.xtrim(b"s", 0, None, 0).unwrap(), 2);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 0);
        assert_eq!(store.key_type(b"s", 0), Some("stream"));

        assert_eq!(store.xtrim(b"missing", 1, None, 0).unwrap(), 0);

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(store.xtrim(b"str", 1, None, 0), Err(StoreError::WrongType));
    }

    /// XTRIM ... LIMIT n cap: when the requested removal count
    /// exceeds the LIMIT, only `n` oldest entries are removed and
    /// the rest survive, even though the resulting stream length
    /// stays above `max_len`. Mirrors upstream
    /// t_stream.c::streamTrim's LIMIT path semantics, which the
    /// approximate (`~`) trim form caps at the user-supplied bound.
    #[test]
    fn stream_xtrim_maxlen_with_limit_caps_removal() {
        let mut store = Store::new();
        for i in 0..10u64 {
            store
                .xadd(b"s", (1000 + i, 0), &[(b"f".to_vec(), vec![i as u8])], 0)
                .unwrap();
        }
        // Without LIMIT: trims all the way down to max_len=2 (8
        // removals).
        let mut bare = Store::new();
        for i in 0..10u64 {
            bare.xadd(b"s", (1000 + i, 0), &[(b"f".to_vec(), vec![i as u8])], 0)
                .unwrap();
        }
        assert_eq!(bare.xtrim(b"s", 2, None, 0).unwrap(), 8);
        assert_eq!(bare.xlen(b"s", 0).unwrap(), 2);

        // With LIMIT=3: cap removal at 3 even though 8 entries
        // exceed max_len. Stream length remains 7, NOT 2.
        let removed = store.xtrim(b"s", 2, Some(3), 0).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 7);

        // The removed entries must be the OLDEST three — verify
        // the surviving range starts at id (1003, 0).
        let remaining = store
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 0)
            .unwrap();
        assert_eq!(remaining[0].0, (1003, 0));
        assert_eq!(remaining.last().unwrap().0, (1009, 0));
    }

    /// Same cap semantics for the MINID variant.
    #[test]
    fn stream_xtrim_minid_with_limit_caps_removal() {
        let mut store = Store::new();
        for i in 0..10u64 {
            store
                .xadd(b"s", (1000 + i, 0), &[(b"f".to_vec(), vec![i as u8])], 0)
                .unwrap();
        }
        // MINID=1006 would normally remove ids (1000..=1005) = 6
        // entries. Cap at 2 → only the 2 oldest go.
        let removed = store.xtrim_minid(b"s", (1006, 0), Some(2), 0).unwrap();
        assert_eq!(removed, 2);
        let remaining = store
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 0)
            .unwrap();
        assert_eq!(remaining[0].0, (1002, 0));
    }

    /// LIMIT=0 must be a true no-op even when entries exceed
    /// max_len — exercises the boundary where the approximate-trim
    /// path is asked to do zero work.
    #[test]
    fn stream_xtrim_with_limit_zero_is_noop() {
        let mut store = Store::new();
        for i in 0..5u64 {
            store
                .xadd(b"s", (1000 + i, 0), &[(b"f".to_vec(), vec![i as u8])], 0)
                .unwrap();
        }
        let removed = store.xtrim(b"s", 1, Some(0), 0).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 5);
    }

    #[test]
    fn stream_xread_returns_entries_after_id_and_respects_count() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        let all_after = store.xread(b"s", (1000, 0), None, 0).unwrap();
        assert_eq!(all_after.len(), 2);
        assert_eq!(all_after[0].0, (1000, 1));
        assert_eq!(all_after[1].0, (1001, 0));

        let limited = store.xread(b"s", (0, 0), Some(1), 0).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].0, (1000, 0));

        let none = store.xread(b"s", (u64::MAX, u64::MAX), None, 0).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn stream_xread_missing_key_and_wrongtype() {
        let mut store = Store::new();
        assert!(store.xread(b"missing", (0, 0), None, 0).unwrap().is_empty());

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xread(b"str", (0, 0), None, 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xreadgroup_new_entries_advances_cursor_and_tracks_consumer() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        let first = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].0, (1000, 0));
        assert_eq!(first[1].0, (1000, 1));
        assert_eq!(
            store.xinfo_groups(b"s", 0).unwrap().expect("groups"),
            vec![(b"g1".to_vec(), 1, 2, (1000, 1))]
        );

        let second = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert!(second.is_empty());

        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();
        let third = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].0, (1001, 0));
        assert_eq!(
            store.xinfo_groups(b"s", 0).unwrap().expect("groups"),
            vec![(b"g1".to_vec(), 1, 3, (1001, 0))]
        );
    }

    #[test]
    fn stream_xreadgroup_missing_group_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(
            store
                .xreadgroup(
                    b"missing",
                    b"g1",
                    b"c1",
                    group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                    0
                )
                .unwrap(),
            None
        );

        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert_eq!(
            store
                .xreadgroup(
                    b"s",
                    b"g1",
                    b"c1",
                    group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                    0
                )
                .unwrap(),
            None
        );

        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
        let explicit = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::Id((0, 0)), false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert!(explicit.is_empty());
        assert_eq!(
            store.xinfo_groups(b"s", 0).unwrap().expect("groups"),
            vec![(b"g1".to_vec(), 1, 0, (0, 0))]
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xreadgroup(
                b"str",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                0
            ),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xreadgroup_replays_only_owner_pending_and_respects_noack() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v0".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 2), &[(b"f".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        let first = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, (1000, 0));

        let owner_history = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::Id((0, 0)), false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(owner_history.len(), 1);
        assert_eq!(owner_history[0].0, (1000, 0));

        let other_history = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c2",
                group_read_options(StreamGroupReadCursor::Id((0, 0)), false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert!(other_history.is_empty());

        let noack_batch = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, true, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(noack_batch.len(), 2);
        assert_eq!(noack_batch[0].0, (1000, 1));
        assert_eq!(noack_batch[1].0, (1000, 2));

        let owner_history_after_noack = store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::Id((0, 0)), false, None),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(owner_history_after_noack.len(), 1);
        assert_eq!(owner_history_after_noack[0].0, (1000, 0));

        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 2, 1, (1000, 2))]);
    }

    #[test]
    fn stream_xpending_summary_and_entries_track_idle_and_delivery_count() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v0".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                5,
            )
            .unwrap()
            .expect("new entries");
        store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::Id((0, 0)), false, None),
                20,
            )
            .unwrap()
            .expect("pending replay");
        store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                25,
            )
            .unwrap()
            .expect("second new entry");

        let summary = store
            .xpending_summary(b"s", b"g1", 30)
            .unwrap()
            .expect("pending summary");
        assert_eq!(
            summary,
            (
                2,
                Some((1000, 0)),
                Some((1000, 1)),
                vec![(b"c1".to_vec(), 2)],
            )
        );

        let all_entries = store
            .xpending_entries(b"s", b"g1", ((0, 0), (u64::MAX, u64::MAX)), 10, None, 30, 0)
            .unwrap()
            .expect("pending entries");
        assert_eq!(
            all_entries,
            vec![
                // Pending replay does not increment delivery count or update idle
                ((1000, 0), b"c1".to_vec(), 25, 1),
                ((1000, 1), b"c1".to_vec(), 5, 1),
            ]
        );

        let filtered = store
            .xpending_entries(
                b"s",
                b"g1",
                ((0, 0), (u64::MAX, u64::MAX)),
                10,
                Some(b"c2"),
                30,
                0,
            )
            .unwrap()
            .expect("filtered entries");
        assert!(filtered.is_empty());
    }

    #[test]
    fn stream_xclaim_transfers_pending_owner_and_supports_justid() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v0".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
        store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                10,
            )
            .unwrap()
            .expect("seed pending");

        let claimed = store
            .xclaim(
                b"s",
                b"g1",
                b"c2",
                &[(1000, 0)],
                StreamClaimOptions {
                    min_idle_time_ms: 5,
                    idle_ms: None,
                    time_ms: None,
                    retry_count: None,
                    force: false,
                    justid: false,
                    last_id: None,
                },
                30,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(
            claimed,
            StreamClaimReply::Entries(vec![((1000, 0), vec![(b"f".to_vec(), b"v0".to_vec())],)])
        );

        let summary = store
            .xpending_summary(b"s", b"g1", 40)
            .unwrap()
            .expect("summary");
        assert_eq!(
            summary,
            (
                2,
                Some((1000, 0)),
                Some((1000, 1)),
                vec![(b"c1".to_vec(), 1), (b"c2".to_vec(), 1)],
            )
        );

        let justid = store
            .xclaim(
                b"s",
                b"g1",
                b"c2",
                &[(1000, 1)],
                StreamClaimOptions {
                    min_idle_time_ms: 5,
                    idle_ms: None,
                    time_ms: None,
                    retry_count: None,
                    force: false,
                    justid: true,
                    last_id: None,
                },
                50,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(justid, StreamClaimReply::Ids(vec![(1000, 1)]));
    }

    #[test]
    fn stream_xautoclaim_claims_entries_by_cursor_and_tracks_deleted_ids() {
        let mut store = Store::new();
        for (id, value) in [((1000, 0), b"v0"), ((1000, 1), b"v1"), ((1000, 2), b"v2")] {
            store
                .xadd(b"s", id, &[(b"f".to_vec(), value.to_vec())], 0)
                .unwrap();
        }
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
        store
            .xreadgroup(
                b"s",
                b"g1",
                b"c1",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                10,
            )
            .unwrap()
            .expect("seed pending");

        // Simulate a deleted entry lingering in the pending table.
        assert_eq!(store.xdel(b"s", &[(1000, 1)], 15).unwrap(), 1);
        if let Some(groups) = store.stream_groups.get_mut(b"s".as_slice())
            && let Some(group_state) = groups.get_mut(b"g1".as_slice())
        {
            group_state.pending.insert(
                (1000, 1),
                StreamPendingEntry {
                    consumer: b"c1".to_vec(),
                    deliveries: 1,
                    last_delivered_ms: 10,
                },
            );
        }

        let first = store
            .xautoclaim(
                b"s",
                b"g1",
                b"c2",
                (0, 0),
                StreamAutoClaimOptions {
                    min_idle_time_ms: 5,
                    count: 2,
                    justid: false,
                },
                30,
            )
            .unwrap()
            .expect("group exists");
        // Upstream xautoclaimCommand decrements `count` on BOTH
        // claimed and deleted entries (see t_stream.c:3441,3478),
        // so a count=2 budget covers 1 claim + 1 delete and the
        // cursor stops at (1000, 2) ready for the next call.
        // (br-frankenredis-r82v)
        assert_eq!(
            first,
            StreamAutoClaimReply::Entries {
                next_start: (1000, 2),
                entries: vec![((1000, 0), vec![(b"f".to_vec(), b"v0".to_vec())]),],
                deleted_ids: vec![(1000, 1)],
            }
        );

        let second = store
            .xautoclaim(
                b"s",
                b"g1",
                b"c3",
                (0, 0),
                StreamAutoClaimOptions {
                    min_idle_time_ms: 0,
                    count: 10,
                    justid: true,
                },
                31,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(
            second,
            StreamAutoClaimReply::Ids {
                next_start: (0, 0),
                ids: vec![(1000, 0), (1000, 2)],
                deleted_ids: vec![],
            }
        );
    }

    #[test]
    fn stream_xtrim_preserves_pending_for_xautoclaim_deleted_ids() {
        fn seed_pending_store() -> Store {
            let mut store = Store::new();
            for (id, value) in [((1000, 0), b"v0"), ((1000, 1), b"v1")] {
                store
                    .xadd(b"s", id, &[(b"f".to_vec(), value.to_vec())], 0)
                    .unwrap();
            }
            assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
            let read = store
                .xreadgroup(
                    b"s",
                    b"g1",
                    b"c1",
                    group_read_options(StreamGroupReadCursor::NewEntries, false, Some(2)),
                    10,
                )
                .unwrap()
                .expect("seed pending");
            assert_eq!(read.len(), 2);
            store
        }

        let mut maxlen_store = seed_pending_store();
        assert_eq!(maxlen_store.xtrim(b"s", 0, None, 20).unwrap(), 2);
        assert_eq!(
            maxlen_store
                .xpending_summary(b"s", b"g1", 30)
                .unwrap()
                .expect("pending summary")
                .0,
            2,
            "XTRIM MAXLEN must leave PEL rows for XAUTOCLAIM deleted_ids"
        );
        let maxlen_claim = maxlen_store
            .xautoclaim(
                b"s",
                b"g1",
                b"c2",
                (0, 0),
                StreamAutoClaimOptions {
                    min_idle_time_ms: 0,
                    count: 10,
                    justid: false,
                },
                40,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(
            maxlen_claim,
            StreamAutoClaimReply::Entries {
                next_start: (0, 0),
                entries: Vec::new(),
                deleted_ids: vec![(1000, 0), (1000, 1)],
            }
        );

        let mut minid_store = seed_pending_store();
        assert_eq!(
            minid_store.xtrim_minid(b"s", (1000, 1), None, 20).unwrap(),
            1
        );
        assert_eq!(
            minid_store
                .xpending_summary(b"s", b"g1", 30)
                .unwrap()
                .expect("pending summary")
                .0,
            2,
            "XTRIM MINID must leave PEL rows for XAUTOCLAIM deleted_ids"
        );
        let minid_claim = minid_store
            .xautoclaim(
                b"s",
                b"g1",
                b"c2",
                (0, 0),
                StreamAutoClaimOptions {
                    min_idle_time_ms: 0,
                    count: 10,
                    justid: false,
                },
                40,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(
            minid_claim,
            StreamAutoClaimReply::Entries {
                next_start: (0, 0),
                entries: vec![((1000, 1), vec![(b"f".to_vec(), b"v1".to_vec())])],
                deleted_ids: vec![(1000, 0)],
            }
        );
    }

    #[test]
    fn stream_xinfo_returns_len_and_entry_bounds() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();

        let info = store.xinfo_stream(b"s", 0).unwrap().expect("stream info");
        assert_eq!(info.0, 2);
        assert_eq!(info.1.expect("first").0, (1000, 0));
        assert_eq!(info.2.expect("last").0, (1001, 0));
        assert_eq!(store.stream_max_deleted_id(b"s"), None);
    }

    #[test]
    fn stream_xinfo_missing_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(store.xinfo_stream(b"missing", 0).unwrap(), None);

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(store.xinfo_stream(b"str", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn stream_tracks_max_deleted_entry_id_across_xdel_and_xtrim() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f1".to_vec(), b"v1".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1000, 1), &[(b"f2".to_vec(), b"v2".to_vec())], 0)
            .unwrap();
        store
            .xadd(b"s", (1001, 0), &[(b"f3".to_vec(), b"v3".to_vec())], 0)
            .unwrap();

        assert_eq!(store.xdel(b"s", &[(1000, 1)], 0).unwrap(), 1);
        assert_eq!(store.stream_max_deleted_id(b"s"), Some((1000, 1)));

        assert_eq!(store.xtrim_minid(b"s", (1001, 0), None, 0).unwrap(), 1);
        assert_eq!(store.stream_max_deleted_id(b"s"), Some((1000, 1)));

        assert_eq!(store.xtrim(b"s", 0, None, 0).unwrap(), 1);
        assert_eq!(store.stream_max_deleted_id(b"s"), Some((1001, 0)));
    }

    #[test]
    fn stream_xgroup_create_and_xinfo_groups() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();

        let created = store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap();
        assert!(created);
        let duplicate = store.xgroup_create(b"s", b"g1", (1, 0), false, 0).unwrap();
        assert!(!duplicate);

        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 0, 0, (0, 0))]);
    }

    #[test]
    fn stream_xlast_id_with_existence_reports_empty_streams() {
        let mut store = Store::new();
        assert_eq!(
            store.xlast_id_with_existence(b"s", 0).unwrap(),
            (false, None)
        );

        assert!(store.xgroup_create(b"s", b"g1", (0, 0), true, 0).unwrap());
        assert_eq!(
            store.xlast_id_with_existence(b"s", 0).unwrap(),
            (true, None)
        );

        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert_eq!(
            store.xlast_id_with_existence(b"s", 0).unwrap(),
            (true, Some((1000, 0)))
        );
    }

    #[test]
    fn stream_xgroup_create_mkstream_missing_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(
            store.xgroup_create(b"missing", b"g1", (0, 0), false, 0),
            Err(StoreError::KeyNotFound)
        );
        assert!(
            store
                .xgroup_create(b"missing", b"g1", (0, 0), true, 0)
                .unwrap()
        );
        assert_eq!(store.xlen(b"missing", 0).unwrap(), 0);

        let removed = store.del(&[b"missing".to_vec()], 0);
        assert_eq!(removed, 1);
        store
            .xadd(b"missing", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        let groups = store.xinfo_groups(b"missing", 0).unwrap().expect("groups");
        assert!(groups.is_empty());

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xgroup_create(b"str", b"g1", (0, 0), true, 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xgroup_destroy_existing_missing_and_wrongtype() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
        assert!(store.xgroup_create(b"s", b"g2", (0, 0), false, 0).unwrap());

        assert!(store.xgroup_destroy(b"s", b"g1", 0).unwrap());
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g2".to_vec(), 0, 0, (0, 0))]);

        assert!(store.xgroup_destroy(b"s", b"g2", 0).unwrap());
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert!(groups.is_empty());

        assert!(!store.xgroup_destroy(b"s", b"missing", 0).unwrap());
        assert!(!store.xgroup_destroy(b"missing", b"g1", 0).unwrap());

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xgroup_destroy(b"str", b"g1", 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xgroup_setid_updates_existing_group_cursor() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        assert!(store.xgroup_setid(b"s", b"g1", (1000, 0), 0).unwrap());
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 0, 0, (1000, 0))]);

        assert!(!store.xgroup_setid(b"s", b"missing", (1000, 0), 0).unwrap());
    }

    #[test]
    fn stream_xgroup_setid_missing_key_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(
            store.xgroup_setid(b"missing", b"g1", (0, 0), 0),
            Err(StoreError::KeyNotFound)
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xgroup_setid(b"str", b"g1", (0, 0), 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xgroup_createconsumer_tracks_consumers_and_errors() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        assert_eq!(
            store
                .xgroup_createconsumer(b"s", b"g1", b"alice", 0)
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            store
                .xgroup_createconsumer(b"s", b"g1", b"alice", 0)
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            store.xgroup_createconsumer(b"s", b"g1", b"bob", 0).unwrap(),
            Some(true)
        );

        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 2, 0, (0, 0))]);

        assert_eq!(
            store
                .xgroup_createconsumer(b"s", b"missing", b"alice", 0)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .xgroup_createconsumer(b"missing", b"g1", b"alice", 0)
                .unwrap(),
            None
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xgroup_createconsumer(b"str", b"g1", b"alice", 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xgroup_delconsumer_returns_pending_count_and_updates_membership() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());
        assert_eq!(
            store
                .xgroup_createconsumer(b"s", b"g1", b"alice", 0)
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            store.xgroup_createconsumer(b"s", b"g1", b"bob", 0).unwrap(),
            Some(true)
        );
        let pending_read = store
            .xreadgroup(
                b"s",
                b"g1",
                b"alice",
                group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                0,
            )
            .unwrap()
            .expect("group exists");
        assert_eq!(pending_read.len(), 1);

        assert_eq!(
            store.xgroup_delconsumer(b"s", b"g1", b"alice", 0).unwrap(),
            Some(1)
        );
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 1, 0, (1000, 0))]);

        assert_eq!(
            store
                .xgroup_delconsumer(b"s", b"g1", b"missing_consumer", 0)
                .unwrap(),
            Some(0)
        );
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 1, 0, (1000, 0))]);

        assert_eq!(
            store
                .xgroup_delconsumer(b"s", b"missing", b"alice", 0)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .xgroup_delconsumer(b"missing", b"g1", b"alice", 0)
                .unwrap(),
            None
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xgroup_delconsumer(b"str", b"g1", b"alice", 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn stream_xinfo_consumers_returns_membership_and_errors() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap());

        let empty = store
            .xinfo_consumers(b"s", b"g1", 0)
            .unwrap()
            .expect("consumers");
        assert!(empty.is_empty());

        assert_eq!(
            store.xgroup_createconsumer(b"s", b"g1", b"c2", 0).unwrap(),
            Some(true)
        );
        assert_eq!(
            store.xgroup_createconsumer(b"s", b"g1", b"c1", 0).unwrap(),
            Some(true)
        );
        let consumers = store
            .xinfo_consumers(b"s", b"g1", 0)
            .unwrap()
            .expect("consumers");
        assert_eq!(
            consumers
                .into_iter()
                .map(|(name, _pending, _idle)| name)
                .collect::<Vec<_>>(),
            vec![b"c1".to_vec(), b"c2".to_vec()]
        );

        assert_eq!(store.xinfo_consumers(b"s", b"missing", 0).unwrap(), None);
        assert_eq!(
            store.xinfo_consumers(b"missing", b"g1", 0),
            Err(StoreError::KeyNotFound)
        );

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(
            store.xinfo_consumers(b"str", b"g1", 0),
            Err(StoreError::WrongType)
        );
    }

    // ── String extension store tests ────────────────────────────────────

    #[test]
    fn incrbyfloat_basic() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"10.5".to_vec(), None, 0);
        let v = store.incrbyfloat(b"k", 0.1, 0).unwrap();
        assert!((v - 10.6).abs() < 1e-10);
    }

    #[test]
    fn incrbyfloat_missing_key() {
        let mut store = Store::new();
        let v = store.incrbyfloat(b"k", 3.5, 0).unwrap();
        assert!((v - 3.5).abs() < 1e-10);
    }

    #[test]
    fn incrbyfloat_wrongtype() {
        let mut store = Store::new();
        store.sadd(b"k", &[b"m".to_vec()], 0).unwrap();
        assert_eq!(store.incrbyfloat(b"k", 1.0, 0), Err(StoreError::WrongType));
    }

    #[test]
    fn getdel_returns_and_removes() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 0);
        let v = store.getdel(b"k", 0).unwrap();
        assert_eq!(v, Some(b"v".to_vec()));
        assert_eq!(store.get(b"k", 0).unwrap(), None);
    }

    #[test]
    fn getdel_missing_key() {
        let mut store = Store::new();
        assert_eq!(store.getdel(b"k", 0).unwrap(), None);
    }

    #[test]
    fn getdel_wrongtype() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"member".to_vec()], 0).unwrap();
        assert_eq!(store.getdel(b"s", 0), Err(StoreError::WrongType));
    }

    #[test]
    fn getrange_basic() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"Hello, World!".to_vec(), None, 0);
        assert_eq!(store.getrange(b"k", 0, 4, 0).unwrap(), b"Hello".to_vec());
        assert_eq!(store.getrange(b"k", -6, -1, 0).unwrap(), b"World!".to_vec());
    }

    #[test]
    fn getrange_missing_key() {
        let mut store = Store::new();
        assert_eq!(store.getrange(b"k", 0, 10, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn setrange_basic() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"Hello World".to_vec(), None, 0);
        let len = store.setrange(b"k", 6, b"Redis", 0).unwrap();
        assert_eq!(len, 11);
        assert_eq!(store.get(b"k", 0).unwrap(), Some(b"Hello Redis".to_vec()));
    }

    #[test]
    fn setrange_extends_with_zeros() {
        let mut store = Store::new();
        let len = store.setrange(b"k", 5, b"Hi", 0).unwrap();
        assert_eq!(len, 7);
        let v = store.get(b"k", 0).unwrap().unwrap();
        assert_eq!(&v[..5], &[0, 0, 0, 0, 0]);
        assert_eq!(&v[5..], b"Hi");
    }

    // ── Set algebra store tests ─────────────────────────────────────────

    #[test]
    fn sinter_basic() {
        let mut store = Store::new();
        store
            .sadd(b"s1", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        store
            .sadd(b"s2", &[b"b".to_vec(), b"c".to_vec(), b"d".to_vec()], 0)
            .unwrap();
        let result = store.sinter(&[b"s1", b"s2"], 0).unwrap();
        assert_eq!(result, vec![b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn sunion_basic() {
        let mut store = Store::new();
        store
            .sadd(b"s1", &[b"a".to_vec(), b"b".to_vec()], 0)
            .unwrap();
        store
            .sadd(b"s2", &[b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        let result = store.sunion(&[b"s1", b"s2"], 0).unwrap();
        assert_eq!(result, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn sdiff_basic() {
        let mut store = Store::new();
        store
            .sadd(b"s1", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        store.sadd(b"s2", &[b"b".to_vec()], 0).unwrap();
        let result = store.sdiff(&[b"s1", b"s2"], 0).unwrap();
        assert_eq!(result, vec![b"a".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn spop_removes_member() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"a".to_vec()], 0).unwrap();
        let m = store.spop(b"s", 0).unwrap();
        assert_eq!(m, Some(b"a".to_vec()));
        assert_eq!(store.scard(b"s", 0).unwrap(), 0);
        // Key should be removed when set becomes empty (Redis semantics)
        assert!(!store.exists(b"s", 0));
    }

    #[test]
    fn srandmember_does_not_remove() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"a".to_vec()], 0).unwrap();
        let m = store.srandmember(b"s", 0).unwrap();
        assert_eq!(m, Some(b"a".to_vec()));
        assert_eq!(store.scard(b"s", 0).unwrap(), 1);
    }

    #[test]
    fn sinter_with_missing_key() {
        let mut store = Store::new();
        store
            .sadd(b"s1", &[b"a".to_vec(), b"b".to_vec()], 0)
            .unwrap();
        let result = store.sinter(&[b"s1", b"missing"], 0).unwrap();
        assert!(result.is_empty());
    }

    // ── Bitmap store tests ──────────────────────────────────────────────

    #[test]
    fn setbit_and_getbit() {
        let mut store = Store::new();
        assert!(!store.setbit(b"bm", 7, true, 0).unwrap());
        assert!(store.getbit(b"bm", 7, 0).unwrap());
        assert!(!store.getbit(b"bm", 0, 0).unwrap());
    }

    #[test]
    fn setbit_auto_extends() {
        let mut store = Store::new();
        store.setbit(b"bm", 20, true, 0).unwrap();
        // byte index 2, bit 4 -> byte 2 should exist
        let v = store.get(b"bm", 0).unwrap().unwrap();
        assert_eq!(v.len(), 3);
        assert!(store.getbit(b"bm", 20, 0).unwrap());
    }

    #[test]
    fn bitcount_basic() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"\xff".to_vec(), None, 0); // 8 bits set
        assert_eq!(
            store
                .bitcount(b"k", None, None, BitRangeUnit::Byte, 0)
                .unwrap(),
            8
        );
    }

    #[test]
    fn bitcount_bit_range_matches_redis_semantics() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"foobar".to_vec(), None, 0);
        assert_eq!(
            store
                .bitcount(b"k", Some(10), Some(14), BitRangeUnit::Bit, 0)
                .unwrap(),
            4
        );
        assert_eq!(
            store
                .bitcount(b"k", Some(32), Some(87), BitRangeUnit::Bit, 0)
                .unwrap(),
            7
        );
    }

    #[test]
    fn bitcount_negative_reverse_range_returns_zero() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"xxxx".to_vec(), None, 0);
        assert_eq!(
            store
                .bitcount(b"k", Some(-6), Some(-7), BitRangeUnit::Byte, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .bitcount(b"k", Some(-6), Some(-15), BitRangeUnit::Bit, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn bitpos_finds_first_set_bit() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0x00, 0x80], None, 0); // bit 8 set (MSB of byte 1)
        assert_eq!(
            store
                .bitpos(b"k", true, None, None, BitRangeUnit::Byte, 0)
                .unwrap(),
            8
        );
    }

    #[test]
    fn bitpos_finds_first_clear_bit() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0xff, 0xff], None, 0); // all bits set
        // Without explicit end, returns position past end
        assert_eq!(
            store
                .bitpos(b"k", false, None, None, BitRangeUnit::Byte, 0)
                .unwrap(),
            16
        );
    }

    /// Redis 7.0+ BITPOS BIT modifier: start/end are interpreted as
    /// bit indices instead of byte indices. Frankenredis previously
    /// rejected the 5-arg-after-key form (`argv.len() == 6`) with a
    /// WrongArity error, blocking 7.0 clients from this dialect. The
    /// fix added a `BitRangeUnit` parameter that mirrors the
    /// existing BITCOUNT path.
    #[test]
    fn bitpos_bit_unit_finds_set_bit_in_subbyte_range() {
        let mut store = Store::new();
        // bytes[0] = 0b00010000 — bit 3 set (the only set bit).
        // BIT-mode search for `1` in [bit 0, bit 3] must report 3.
        store.set(b"k".to_vec(), vec![0b0001_0000], None, 0);
        assert_eq!(
            store
                .bitpos(b"k", true, Some(0), Some(3), BitRangeUnit::Bit, 0)
                .unwrap(),
            3
        );
        // Same byte but search restricted to [0, 2] (excludes bit 3):
        // no set bit in range → -1.
        assert_eq!(
            store
                .bitpos(b"k", true, Some(0), Some(2), BitRangeUnit::Bit, 0)
                .unwrap(),
            -1
        );
        // Search [4, 7] also has no set bit → -1.
        assert_eq!(
            store
                .bitpos(b"k", true, Some(4), Some(7), BitRangeUnit::Bit, 0)
                .unwrap(),
            -1
        );
    }

    #[test]
    fn bitpos_bit_unit_finds_clear_bit_in_subbyte_range() {
        let mut store = Store::new();
        // bytes[0] = 0b11111101 — only bit 6 is clear.
        // BIT-mode search for `0` in [4, 7] must report 6.
        store.set(b"k".to_vec(), vec![0b1111_1101], None, 0);
        assert_eq!(
            store
                .bitpos(b"k", false, Some(4), Some(7), BitRangeUnit::Bit, 0)
                .unwrap(),
            6
        );
        // Search [0, 5] (excludes the only zero at bit 6) → -1
        // because end is explicit, no past-end fallback.
        assert_eq!(
            store
                .bitpos(b"k", false, Some(0), Some(5), BitRangeUnit::Bit, 0)
                .unwrap(),
            -1
        );
    }

    #[test]
    fn bitpos_bit_unit_negative_indices_are_relative_to_total_bits() {
        let mut store = Store::new();
        // 2 bytes = 16 bits. bytes[1] = 0b00000010 → bit 14 set.
        store.set(b"k".to_vec(), vec![0x00, 0b0000_0010], None, 0);
        // Negative bit indices: -1 = last bit (15), -8 = bit 8.
        // Search [bit 8, last bit] for `1` → bit 14.
        assert_eq!(
            store
                .bitpos(b"k", true, Some(-8), Some(-1), BitRangeUnit::Bit, 0)
                .unwrap(),
            14
        );
    }

    #[test]
    fn bitpos_bit_unit_spans_multiple_bytes() {
        let mut store = Store::new();
        // 3 bytes; only the very last bit is set.
        // bytes = [0x00, 0x00, 0b0000_0001] → bit 23 set.
        store.set(b"k".to_vec(), vec![0x00, 0x00, 0b0000_0001], None, 0);
        // BIT-mode search [4, 23] must report 23.
        assert_eq!(
            store
                .bitpos(b"k", true, Some(4), Some(23), BitRangeUnit::Bit, 0)
                .unwrap(),
            23
        );
        // BIT-mode search [4, 22] must report -1 (last bit excluded).
        assert_eq!(
            store
                .bitpos(b"k", true, Some(4), Some(22), BitRangeUnit::Bit, 0)
                .unwrap(),
            -1
        );
    }

    /// BIT mode with no end omits the past-end-of-string fallback in
    /// the same way BYTE mode does — when the caller did not supply
    /// `end_given`, looking for `0` in an all-1s range returns the
    /// bit position just past the last covered byte. Verifying the
    /// fallback uses bit semantics keeps `(e_byte + 1) * 8` honest
    /// for both units.
    #[test]
    fn bitpos_bit_unit_no_end_zero_search_returns_past_end() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0xff, 0xff], None, 0);
        assert_eq!(
            store
                .bitpos(b"k", false, Some(0), None, BitRangeUnit::Bit, 0)
                .unwrap(),
            16
        );
    }

    /// Backwards-compat: the BYTE path must continue to behave exactly
    /// as before, including the past-end fallback when end is omitted.
    #[test]
    fn bitpos_byte_unit_back_compat_holds_after_unit_param_added() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0x00, 0x80, 0x00], None, 0);
        // Looking for 1 anywhere → bit 8 (MSB of byte 1).
        assert_eq!(
            store
                .bitpos(b"k", true, None, None, BitRangeUnit::Byte, 0)
                .unwrap(),
            8
        );
        // Looking for 1 in just byte 0 → -1.
        assert_eq!(
            store
                .bitpos(b"k", true, Some(0), Some(0), BitRangeUnit::Byte, 0)
                .unwrap(),
            -1
        );
        // Looking for 0 with no end and all bytes are 0xff → past end.
        store.set(b"all1".to_vec(), vec![0xff, 0xff, 0xff], None, 0);
        assert_eq!(
            store
                .bitpos(b"all1", false, None, None, BitRangeUnit::Byte, 0)
                .unwrap(),
            24
        );
    }

    // ── Extended List store tests ───────────────────────────────────────

    #[test]
    fn lpos_basic() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        assert_eq!(store.lpos(b"l", b"b", 0).unwrap(), Some(1));
        assert_eq!(store.lpos(b"l", b"x", 0).unwrap(), None);
    }

    #[test]
    fn linsert_before_and_after() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        let len = store.linsert_before(b"l", b"c", b"b".to_vec(), 0).unwrap();
        assert_eq!(len, 3);
        let range = store.lrange(b"l", 0, -1, 0).unwrap();
        assert_eq!(range, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

        let len2 = store.linsert_after(b"l", b"c", b"d".to_vec(), 0).unwrap();
        assert_eq!(len2, 4);
    }

    #[test]
    fn lrem_count_positive() {
        let mut store = Store::new();
        store
            .rpush(
                b"l",
                &[
                    b"a".to_vec(),
                    b"b".to_vec(),
                    b"a".to_vec(),
                    b"c".to_vec(),
                    b"a".to_vec(),
                ],
                0,
            )
            .unwrap();
        let removed = store.lrem(b"l", 2, b"a", 0).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.llen(b"l", 0).unwrap(), 3);
    }

    #[test]
    fn lrem_count_zero_removes_all() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"a".to_vec()], 0)
            .unwrap();
        let removed = store.lrem(b"l", 0, b"a", 0).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.llen(b"l", 0).unwrap(), 1);
    }

    #[test]
    fn rpoplpush_basic() {
        let mut store = Store::new();
        store
            .rpush(b"src", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 0)
            .unwrap();
        let val = store.rpoplpush(b"src", b"dst", 0).unwrap();
        assert_eq!(val, Some(b"c".to_vec()));
        assert_eq!(store.llen(b"src", 0).unwrap(), 2);
        assert_eq!(store.llen(b"dst", 0).unwrap(), 1);
    }

    // ── HyperLogLog store tests ───────────────────────────────────────────

    #[test]
    fn pfadd_creates_key_and_reports_modified() {
        let mut store = Store::new();
        assert!(
            store
                .pfadd(b"hll", &[b"a".to_vec(), b"b".to_vec()], 0)
                .unwrap()
        );
        // Adding same elements again should not modify
        assert!(
            !store
                .pfadd(b"hll", &[b"a".to_vec(), b"b".to_vec()], 0)
                .unwrap()
        );
    }

    #[test]
    fn pfadd_no_elements_creates_key() {
        let mut store = Store::new();
        // Creating the key with no elements reports creation
        assert!(store.pfadd(b"hll", &[], 0).unwrap());
        // Second call with no elements, key already exists, no change
        assert!(!store.pfadd(b"hll", &[], 0).unwrap());
    }

    #[test]
    fn pfcount_empty_key_is_zero() {
        let mut store = Store::new();
        assert_eq!(store.pfcount(&[b"missing"], 0).unwrap(), 0);
    }

    #[test]
    fn pfcount_after_adds() {
        let mut store = Store::new();
        let elements: Vec<Vec<u8>> = (0..100).map(|i| format!("elem{i}").into_bytes()).collect();
        store.pfadd(b"hll", &elements, 0).unwrap();
        let count = store.pfcount(&[b"hll"], 0).unwrap();
        // HLL is approximate; allow 100 ± 10
        assert!((90..=110).contains(&count), "count={count}, expected ~100");
    }

    #[test]
    fn pfmerge_combines_two_hlls() {
        let mut store = Store::new();
        let e1: Vec<Vec<u8>> = (0..50).map(|i| format!("a{i}").into_bytes()).collect();
        let e2: Vec<Vec<u8>> = (50..100).map(|i| format!("b{i}").into_bytes()).collect();
        store.pfadd(b"h1", &e1, 0).unwrap();
        store.pfadd(b"h2", &e2, 0).unwrap();
        store.pfmerge(b"merged", &[b"h1", b"h2"], 0).unwrap();
        let count = store.pfcount(&[b"merged"], 0).unwrap();
        assert!((90..=110).contains(&count), "count={count}, expected ~100");
    }

    #[test]
    fn pfadd_wrong_type_returns_error() {
        let mut store = Store::new();
        store.sadd(b"s", &[b"x".to_vec()], 0).unwrap();
        assert_eq!(
            store.pfadd(b"s", &[b"a".to_vec()], 0),
            Err(StoreError::WrongType)
        );
    }

    #[test]
    fn pfadd_on_regular_string_returns_invalid_hll() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 0);
        assert_eq!(
            store.pfadd(b"k", &[b"a".to_vec()], 0),
            Err(StoreError::InvalidHllValue)
        );
    }

    #[test]
    fn hll_debug_getreg_returns_full_register_vector_and_promotes_sparse_to_dense() {
        let mut store = Store::new();
        store
            .pfadd(b"hll", &[b"a".to_vec(), b"b".to_vec()], 0)
            .unwrap();
        assert_eq!(store.hll_debug_encoding(b"hll", 0).unwrap(), Some("sparse"));
        let registers = store.hll_debug_getreg(b"hll", 0).unwrap().unwrap();
        assert_eq!(registers.len(), 16_384);
        assert!(registers.iter().any(|value| *value != 0));
        assert_eq!(store.hll_debug_encoding(b"hll", 0).unwrap(), Some("dense"));
    }

    #[test]
    fn hll_debug_sparse_encoding_decode_and_todense_match_redis_contract() {
        let mut store = Store::new();
        store.pfadd(b"hll", &[], 0).unwrap();
        assert_eq!(store.hll_debug_encoding(b"hll", 0).unwrap(), Some("sparse"));
        assert_eq!(
            store.hll_debug_decode(b"hll", 0).unwrap(),
            Some("Z:16384".to_string())
        );
        assert_eq!(store.hll_debug_todense(b"hll", 0).unwrap(), Some(true));
        assert_eq!(store.hll_debug_encoding(b"hll", 0).unwrap(), Some("dense"));
        assert_eq!(store.hll_debug_todense(b"hll", 0).unwrap(), Some(false));
    }

    #[test]
    fn hll_sparse_decode_matches_redis_opcode_split_limits() {
        let mut registers = vec![0u8; HLL_REGISTERS];
        registers[0..5].fill(1);
        registers[5..8].fill(0);
        registers[8..13].fill(2);
        assert_eq!(
            hll_sparse_decode(&registers).unwrap(),
            "v:1,4 v:1,1 z:3 v:2,4 v:2,1 Z:16371"
        );
    }

    #[test]
    fn hll_selftest_passes() {
        let store = Store::new();
        store.hll_selftest().unwrap();
    }

    #[test]
    fn zrevrangebylex_returns_reversed_order() {
        let mut store = Store::new();
        store
            .zadd(
                b"z",
                &[
                    (0.0, b"a".to_vec()),
                    (0.0, b"b".to_vec()),
                    (0.0, b"c".to_vec()),
                    (0.0, b"d".to_vec()),
                ],
                0,
            )
            .unwrap();
        let result = store.zrevrangebylex(b"z", b"+", b"-", 0).unwrap();
        assert_eq!(
            result,
            vec![b"d".to_vec(), b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]
        );
        // Subset range
        let result = store.zrevrangebylex(b"z", b"[c", b"[a", 0).unwrap();
        assert_eq!(result, vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
    }

    #[test]
    fn spop_cleans_up_empty_set() {
        let mut store = Store::new();
        store
            .sadd(b"s", &[b"x".to_vec(), b"y".to_vec()], 0)
            .unwrap();
        store.spop(b"s", 0).unwrap();
        store.spop(b"s", 0).unwrap();
        // After popping all members, the key should be removed
        assert!(!store.exists(b"s", 0));
        assert_eq!(store.spop(b"s", 0).unwrap(), None);
    }

    #[test]
    fn dump_restore_stream_leak() {
        let mut store = Store::new();
        // Create a stream and a group
        store
            .xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        store.xgroup_create(b"s", b"g1", (0, 0), false, 0).unwrap();

        // Create a string and dump it
        store.set(b"k".to_vec(), b"string".to_vec(), None, 0);
        let payload = store.dump_key(b"k", 0).unwrap();

        // Restore string over the stream
        store.restore_key(b"s", 0, &payload, true, 0).unwrap();

        assert!(
            !store.stream_groups.contains_key(b"s".as_slice()),
            "Leaked groups!"
        );
        assert!(
            !store.stream_last_ids.contains_key(b"s".as_slice()),
            "Leaked last_ids!"
        );
    }

    #[test]
    fn dump_restore_string_round_trip() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 100);
        let payload = store.dump_key(b"k", 100).unwrap();
        let mut store2 = Store::new();
        store2.restore_key(b"k", 0, &payload, false, 100).unwrap();
        assert_eq!(store2.get(b"k", 100).unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn dump_payload_uses_redis_dump_footer() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"hello".to_vec(), Some(1_100), 100);
        let payload = store.dump_key(b"k", 100).unwrap();

        let version_offset = payload.len() - DUMP_TRAILER_LEN;
        let crc_offset = payload.len() - DUMP_CRC64_LEN;
        assert_eq!(payload[0], 0);
        assert_eq!(payload[1], 5);
        assert_eq!(&payload[2..version_offset], b"hello");
        assert_eq!(
            &payload[version_offset..crc_offset],
            &RDB_DUMP_VERSION.to_le_bytes()
        );
        let stored_crc = u64::from_le_bytes(payload[crc_offset..].try_into().expect("crc bytes"));
        assert_eq!(stored_crc, fr_persist::crc64_redis(&payload[..crc_offset]));
    }

    #[test]
    fn restore_rejects_unsupported_dump_version() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 100);
        let mut payload = store.dump_key(b"k", 100).unwrap();

        let version_offset = payload.len() - DUMP_TRAILER_LEN;
        payload[version_offset..version_offset + DUMP_VERSION_LEN]
            .copy_from_slice(&(RDB_DUMP_VERSION + 1).to_le_bytes());
        let crc_offset = payload.len() - DUMP_CRC64_LEN;
        let crc = fr_persist::crc64_redis(&payload[..crc_offset]);
        payload[crc_offset..].copy_from_slice(&crc.to_le_bytes());

        let mut store2 = Store::new();
        assert_eq!(
            store2.restore_key(b"k", 0, &payload, false, 100),
            Err(StoreError::InvalidDumpPayload)
        );
    }

    fn append_dump_footer(mut body: Vec<u8>) -> Vec<u8> {
        body.extend_from_slice(&RDB_DUMP_VERSION.to_le_bytes());
        let crc = fr_persist::crc64_redis(&body);
        body.extend_from_slice(&crc.to_le_bytes());
        body
    }

    #[test]
    fn dump_string_uses_upstream_integer_string_encoding() {
        let mut store = Store::new();
        store.set(b"i8".to_vec(), b"123".to_vec(), None, 100);
        store.set(b"i16".to_vec(), b"-129".to_vec(), None, 100);
        store.set(b"i32".to_vec(), b"2147483647".to_vec(), None, 100);
        store.set(b"raw".to_vec(), b"00123".to_vec(), None, 100);

        let i8_payload = store.dump_key(b"i8", 100).unwrap();
        assert_eq!(&i8_payload[..3], &[RDB_TYPE_STRING, 0xC0, 123]);

        let i16_payload = store.dump_key(b"i16", 100).unwrap();
        assert_eq!(i16_payload[0], RDB_TYPE_STRING);
        assert_eq!(i16_payload[1], 0xC1);
        assert_eq!(
            &i16_payload[2..4],
            &i16::try_from(-129).unwrap().to_le_bytes()
        );

        let i32_payload = store.dump_key(b"i32", 100).unwrap();
        assert_eq!(i32_payload[0], RDB_TYPE_STRING);
        assert_eq!(i32_payload[1], 0xC2);
        assert_eq!(&i32_payload[2..6], &2147483647i32.to_le_bytes());

        let raw_payload = store.dump_key(b"raw", 100).unwrap();
        assert_eq!(
            &raw_payload[..7],
            &[RDB_TYPE_STRING, 5, b'0', b'0', b'1', b'2', b'3']
        );
    }

    #[test]
    fn restore_accepts_upstream_integer_encoded_strings() {
        let int8_payload = append_dump_footer(vec![RDB_TYPE_STRING, 0xC0, 42]);

        let mut int16_body = vec![RDB_TYPE_STRING, 0xC1];
        int16_body.extend_from_slice(&(-129i16).to_le_bytes());
        let int16_payload = append_dump_footer(int16_body);

        let mut int32_body = vec![RDB_TYPE_STRING, 0xC2];
        int32_body.extend_from_slice(&2147483647i32.to_le_bytes());
        let int32_payload = append_dump_footer(int32_body);

        let mut store = Store::new();
        store
            .restore_key(b"i8", 0, &int8_payload, false, 100)
            .unwrap();
        store
            .restore_key(b"i16", 0, &int16_payload, false, 100)
            .unwrap();
        store
            .restore_key(b"i32", 0, &int32_payload, false, 100)
            .unwrap();

        assert_eq!(store.get(b"i8", 100).unwrap(), Some(b"42".to_vec()));
        assert_eq!(store.get(b"i16", 100).unwrap(), Some(b"-129".to_vec()));
        assert_eq!(
            store.get(b"i32", 100).unwrap(),
            Some(b"2147483647".to_vec())
        );
    }

    #[test]
    fn restore_accepts_upstream_lzf_encoded_string() {
        let compressed_hello = [4, b'h', b'e', b'l', b'l', b'o'];
        let mut body = vec![RDB_TYPE_STRING, 0xC3];
        encode_length(&mut body, compressed_hello.len());
        encode_length(&mut body, 5);
        body.extend_from_slice(&compressed_hello);
        let payload = append_dump_footer(body);

        let mut store = Store::new();
        store.restore_key(b"lzf", 0, &payload, false, 100).unwrap();

        assert_eq!(store.get(b"lzf", 100).unwrap(), Some(b"hello".to_vec()));
    }

    fn append_raw_dump_bulk(buf: &mut Vec<u8>, data: &[u8]) {
        encode_length(buf, data.len());
        buf.extend_from_slice(data);
    }

    #[test]
    fn dump_payload_uses_upstream_container_tags() {
        let mut store = Store::new();
        store
            .rpush(b"list", &[b"a".to_vec(), b"b".to_vec()], 100)
            .unwrap();
        store.sadd(b"set", &[b"a".to_vec()], 100).unwrap();
        store.sadd(b"intset", &[b"1".to_vec()], 100).unwrap();
        store
            .hset(b"hash", b"f".to_vec(), b"v".to_vec(), 100)
            .unwrap();
        store.zadd(b"zset", &[(1.5, b"a".to_vec())], 100).unwrap();
        // raw_set/raw_zset use 129 to push past their 128-entry listpack
        // threshold; raw_hash uses 513 to clear hash_max_listpack_entries
        // (512, upstream Redis 7.2 default).
        for i in 0..129 {
            store
                .sadd(b"raw_set", &[format!("member-{i}").into_bytes()], 100)
                .unwrap();
        }
        for i in 0..513 {
            store
                .hset(
                    b"raw_hash",
                    format!("field-{i}").into_bytes(),
                    b"v".to_vec(),
                    100,
                )
                .unwrap();
        }
        let raw_zset_members: Vec<(f64, Vec<u8>)> = (0..129)
            .map(|i| (f64::from(i), format!("member-{i}").into_bytes()))
            .collect();
        store.zadd(b"raw_zset", &raw_zset_members, 100).unwrap();

        assert_eq!(
            store.dump_key(b"list", 100).unwrap()[0],
            RDB_TYPE_LIST_QUICKLIST_2
        );
        assert_eq!(
            store.dump_key(b"intset", 100).unwrap()[0],
            RDB_TYPE_SET_INTSET
        );
        assert_eq!(
            store.dump_key(b"set", 100).unwrap()[0],
            RDB_TYPE_SET_LISTPACK
        );
        assert_eq!(
            store.dump_key(b"hash", 100).unwrap()[0],
            RDB_TYPE_HASH_LISTPACK
        );
        assert_eq!(
            store.dump_key(b"zset", 100).unwrap()[0],
            RDB_TYPE_ZSET_LISTPACK
        );
        assert_eq!(store.dump_key(b"raw_set", 100).unwrap()[0], RDB_TYPE_SET);
        assert_eq!(store.dump_key(b"raw_hash", 100).unwrap()[0], RDB_TYPE_HASH);
        assert_eq!(
            store.dump_key(b"raw_zset", 100).unwrap()[0],
            RDB_TYPE_ZSET_2
        );
    }

    #[test]
    fn dump_restore_accepts_upstream_compact_encodings() {
        let upstream_string = [
            0x00, 0x05, b'h', b'e', b'l', b'l', b'o', 0x0B, 0x00, 0x0A, 0xAD, 0x62, 0x05, 0x98,
            0xAB, 0xC9, 0x83,
        ];
        let upstream_list = [
            0x12, 0x01, 0x02, 0x0D, 0x0D, 0x00, 0x00, 0x00, 0x02, 0x00, 0x81, b'a', 0x02, 0x81,
            b'b', 0x02, 0xFF, 0x0B, 0x00, 0x01, 0x34, 0xB7, 0xFA, 0xEE, 0xDE, 0x52, 0x38,
        ];
        let upstream_set = [
            0x14, 0x0D, 0x0D, 0x00, 0x00, 0x00, 0x02, 0x00, 0x81, b'a', 0x02, 0x81, b'b', 0x02,
            0xFF, 0x0B, 0x00, 0x0A, 0xEC, 0x0A, 0xB4, 0x49, 0xA3, 0xD6, 0x54,
        ];
        let upstream_hash = [
            0x10, 0x0D, 0x0D, 0x00, 0x00, 0x00, 0x02, 0x00, 0x81, b'f', 0x02, 0x81, b'v', 0x02,
            0xFF, 0x0B, 0x00, 0x49, 0x2E, 0x80, 0x37, 0xDE, 0xCB, 0xE1, 0x14,
        ];
        let upstream_lzf_hash = [
            0x10, 0xC3, 0x21, 0x27, 0x13, 0x27, 0x00, 0x00, 0x00, 0x04, 0x00, 0x86, b'f', b'i',
            b'e', b'l', b'd', b'1', 0x07, 0x86, b'v', b'a', b'l', b'u', b'e', 0x20, 0x07, 0x60,
            0x0F, 0x00, 0x32, 0xA0, 0x0F, 0x02, 0x32, 0x07, 0xFF, 0x0B, 0x00, 0x5B, 0xCB, 0x7E,
            0x31, 0x6C, 0xB5, 0xBB, 0xFD,
        ];
        let upstream_zset = [
            0x11, 0x0F, 0x0F, 0x00, 0x00, 0x00, 0x02, 0x00, 0x81, b'a', 0x02, 0x83, b'1', b'.',
            b'5', 0x04, 0xFF, 0x0B, 0x00, 0x61, 0xD3, 0xD3, 0x6A, 0x7D, 0x11, 0x94, 0x11,
        ];

        let mut store = Store::new();
        store
            .restore_key(b"str", 0, &upstream_string, false, 100)
            .unwrap();
        store
            .restore_key(b"list", 0, &upstream_list, false, 100)
            .unwrap();
        store
            .restore_key(b"set", 0, &upstream_set, false, 100)
            .unwrap();
        store
            .restore_key(b"hash", 0, &upstream_hash, false, 100)
            .unwrap();
        store
            .restore_key(b"lzf_hash", 0, &upstream_lzf_hash, false, 100)
            .unwrap();
        store
            .restore_key(b"zset", 0, &upstream_zset, false, 100)
            .unwrap();

        assert_eq!(store.get(b"str", 100).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(
            store.lrange(b"list", 0, -1, 100).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert!(store.sismember(b"set", b"a", 100).unwrap());
        assert!(store.sismember(b"set", b"b", 100).unwrap());
        assert_eq!(store.hget(b"hash", b"f", 100).unwrap(), Some(b"v".to_vec()));
        assert_eq!(
            store.hget(b"lzf_hash", b"field1", 100).unwrap(),
            Some(b"value1".to_vec())
        );
        assert_eq!(
            store.hget(b"lzf_hash", b"field2", 100).unwrap(),
            Some(b"value2".to_vec())
        );
        assert_eq!(store.zscore(b"zset", b"a", 100).unwrap(), Some(1.5));
    }

    #[test]
    fn restore_accepts_upstream_large_and_intset_encodings() {
        let mut raw_set = vec![RDB_TYPE_SET];
        encode_length(&mut raw_set, 2);
        append_raw_dump_bulk(&mut raw_set, b"a");
        append_raw_dump_bulk(&mut raw_set, b"b");
        let raw_set = append_dump_footer(raw_set);

        let mut intset = Vec::new();
        intset.extend_from_slice(&2u32.to_le_bytes());
        intset.extend_from_slice(&2u32.to_le_bytes());
        intset.extend_from_slice(&1i16.to_le_bytes());
        intset.extend_from_slice(&2i16.to_le_bytes());
        let mut intset_set = vec![RDB_TYPE_SET_INTSET];
        append_raw_dump_bulk(&mut intset_set, &intset);
        let intset_set = append_dump_footer(intset_set);

        let mut raw_hash = vec![RDB_TYPE_HASH];
        encode_length(&mut raw_hash, 1);
        append_raw_dump_bulk(&mut raw_hash, b"f");
        append_raw_dump_bulk(&mut raw_hash, b"v");
        let raw_hash = append_dump_footer(raw_hash);

        let mut raw_zset = vec![RDB_TYPE_ZSET_2];
        encode_length(&mut raw_zset, 1);
        append_raw_dump_bulk(&mut raw_zset, b"a");
        raw_zset.extend_from_slice(&1.5f64.to_le_bytes());
        let raw_zset = append_dump_footer(raw_zset);

        let mut store = Store::new();
        store
            .restore_key(b"raw_set", 0, &raw_set, false, 100)
            .unwrap();
        store
            .restore_key(b"intset", 0, &intset_set, false, 100)
            .unwrap();
        store
            .restore_key(b"raw_hash", 0, &raw_hash, false, 100)
            .unwrap();
        store
            .restore_key(b"raw_zset", 0, &raw_zset, false, 100)
            .unwrap();

        assert!(store.sismember(b"raw_set", b"a", 100).unwrap());
        assert!(store.sismember(b"intset", b"1", 100).unwrap());
        assert_eq!(
            store.hget(b"raw_hash", b"f", 100).unwrap(),
            Some(b"v".to_vec())
        );
        assert_eq!(store.zscore(b"raw_zset", b"a", 100).unwrap(), Some(1.5));
    }

    #[test]
    fn dump_restore_list_round_trip() {
        let mut store = Store::new();
        store
            .rpush(b"l", &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()], 100)
            .unwrap();
        let payload = store.dump_key(b"l", 100).unwrap();
        let mut store2 = Store::new();
        store2.restore_key(b"l", 0, &payload, false, 100).unwrap();
        assert_eq!(
            store2.lrange(b"l", 0, -1, 100).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn dump_restore_set_round_trip() {
        let mut store = Store::new();
        store
            .sadd(b"s", &[b"x".to_vec(), b"y".to_vec()], 100)
            .unwrap();
        let payload = store.dump_key(b"s", 100).unwrap();
        let mut store2 = Store::new();
        store2.restore_key(b"s", 0, &payload, false, 100).unwrap();
        assert!(store2.sismember(b"s", b"x", 100).unwrap());
        assert!(store2.sismember(b"s", b"y", 100).unwrap());
        assert_eq!(store2.scard(b"s", 100).unwrap(), 2);
    }

    #[test]
    fn dump_restore_hash_round_trip() {
        let mut store = Store::new();
        store
            .hset(b"h", b"f1".to_vec(), b"v1".to_vec(), 100)
            .unwrap();
        store
            .hset(b"h", b"f2".to_vec(), b"v2".to_vec(), 100)
            .unwrap();
        let payload = store.dump_key(b"h", 100).unwrap();
        let mut store2 = Store::new();
        store2.restore_key(b"h", 0, &payload, false, 100).unwrap();
        assert_eq!(store2.hget(b"h", b"f1", 100).unwrap(), Some(b"v1".to_vec()));
        assert_eq!(store2.hget(b"h", b"f2", 100).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn dump_restore_sorted_set_round_trip() {
        let mut store = Store::new();
        store
            .zadd(b"z", &[(1.5, b"a".to_vec()), (2.5, b"b".to_vec())], 100)
            .unwrap();
        let payload = store.dump_key(b"z", 100).unwrap();
        let mut store2 = Store::new();
        store2.restore_key(b"z", 0, &payload, false, 100).unwrap();
        assert_eq!(store2.zscore(b"z", b"a", 100).unwrap(), Some(1.5));
        assert_eq!(store2.zscore(b"z", b"b", 100).unwrap(), Some(2.5));
    }

    #[test]
    fn dump_restore_stream_round_trip() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"name".to_vec(), b"alice".to_vec())], 100)
            .unwrap();
        store
            .xadd(
                b"s",
                (2, 0),
                &[
                    (b"name".to_vec(), b"bob".to_vec()),
                    (b"age".to_vec(), b"30".to_vec()),
                ],
                100,
            )
            .unwrap();
        store.xgroup_create(b"s", b"g", (0, 0), false, 100).unwrap();
        let read = store
            .xreadgroup(
                b"s",
                b"g",
                b"alice",
                StreamGroupReadOptions {
                    cursor: StreamGroupReadCursor::NewEntries,
                    noack: false,
                    count: None,
                },
                150,
            )
            .unwrap()
            .expect("stream group read");
        assert_eq!(read.len(), 2);
        let payload = store.dump_key(b"s", 100).unwrap();
        assert_eq!(payload[0], RDB_TYPE_STREAM_LISTPACKS_3);
        let mut store2 = Store::new();
        store2.restore_key(b"s", 0, &payload, false, 100).unwrap();
        let entries = store2
            .xrange(b"s", (0, 0), (u64::MAX, u64::MAX), None, 100)
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, (1, 0));
        assert_eq!(entries[0].1, vec![(b"name".to_vec(), b"alice".to_vec())]);
        assert_eq!(entries[1].0, (2, 0));
        let groups = store2.xinfo_groups(b"s", 100).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g".to_vec(), 1, 2, (2, 0))]);
        let pending = store2
            .xpending_summary(b"s", b"g", 200)
            .unwrap()
            .expect("pending summary");
        assert_eq!(pending.0, 2);
        assert_eq!(pending.1, Some((1, 0)));
        assert_eq!(pending.2, Some((2, 0)));
        assert_eq!(pending.3, vec![(b"alice".to_vec(), 2)]);
    }

    #[test]
    fn dump_restore_with_ttl() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"val".to_vec(), Some(200), 100);
        let payload = store.dump_key(b"k", 100).unwrap();
        let mut store2 = Store::new();
        // Restore with explicit TTL of 50ms
        store2.restore_key(b"k", 50, &payload, false, 100).unwrap();
        assert_eq!(store2.get(b"k", 100).unwrap(), Some(b"val".to_vec()));
        // After 50ms the key should be expired
        assert_eq!(store2.get(b"k", 151).unwrap(), None);
    }

    #[test]
    fn dump_restore_busy_key() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"old".to_vec(), None, 100);
        store.set(b"k2".to_vec(), b"val".to_vec(), None, 100);
        let payload = store.dump_key(b"k2", 100).unwrap();
        // Without REPLACE, should fail with BusyKey
        assert_eq!(
            store.restore_key(b"k", 0, &payload, false, 100),
            Err(StoreError::BusyKey)
        );
        // With REPLACE, should succeed
        store.restore_key(b"k", 0, &payload, true, 100).unwrap();
        assert_eq!(store.get(b"k", 100).unwrap(), Some(b"val".to_vec()));
    }

    #[test]
    fn dump_restore_invalid_crc_rejected() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        let mut payload = store.dump_key(b"k", 100).unwrap();
        // Corrupt the CRC bytes
        let last = payload.len() - 1;
        payload[last] ^= 0xFF;
        let mut store2 = Store::new();
        assert_eq!(
            store2.restore_key(b"k", 0, &payload, false, 100),
            Err(StoreError::InvalidDumpPayload)
        );
    }

    #[test]
    fn dump_nonexistent_key_returns_none() {
        let mut store = Store::new();
        assert!(store.dump_key(b"nope", 100).is_none());
    }

    // ── AOF rewrite serialization tests ─────────────────────────────────

    #[test]
    fn aof_commands_empty_store() {
        let mut store = Store::new();
        let cmds = store.to_aof_commands(100);
        assert!(cmds.is_empty());
    }

    #[test]
    fn aof_commands_string_key() {
        let mut store = Store::new();
        store.set(b"hello".to_vec(), b"world".to_vec(), None, 100);
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"SET");
        assert_eq!(cmds[0][1], b"hello");
        assert_eq!(cmds[0][2], b"world");
    }

    #[test]
    fn aof_commands_string_with_expiry() {
        let mut store = Store::new();
        // set with px_ttl_ms=5000 at now_ms=100 → expires_at_ms=5100
        store.set(b"k".to_vec(), b"v".to_vec(), Some(5000), 100);
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0][0], b"SET");
        assert_eq!(cmds[1][0], b"PEXPIREAT");
        assert_eq!(cmds[1][1], b"k");
        assert_eq!(cmds[1][2], b"5100");
    }

    #[test]
    fn aof_commands_expired_key_skipped() {
        let mut store = Store::new();
        // set with px_ttl_ms=50 at now_ms=100 → expires_at_ms=150
        store.set(b"k".to_vec(), b"v".to_vec(), Some(50), 100);
        // At now_ms=200, key has expired (200 > 150)
        let cmds = store.to_aof_commands(200);
        assert!(cmds.is_empty());
    }

    #[test]
    fn aof_commands_hash_key() {
        let mut store = Store::new();
        store
            .hset(b"myhash", b"field1".to_vec(), b"val1".to_vec(), 100)
            .unwrap();
        store
            .hset(b"myhash", b"field2".to_vec(), b"val2".to_vec(), 100)
            .unwrap();
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"HSET");
        assert_eq!(cmds[0][1], b"myhash");
        // Fields are sorted deterministically
        assert_eq!(cmds[0].len(), 6); // HSET key f1 v1 f2 v2
    }

    #[test]
    fn aof_commands_list_key() {
        let mut store = Store::new();
        let _ = store.rpush(
            b"mylist",
            &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
            100,
        );
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"RPUSH");
        assert_eq!(cmds[0][1], b"mylist");
        assert_eq!(cmds[0][2], b"a");
        assert_eq!(cmds[0][3], b"b");
        assert_eq!(cmds[0][4], b"c");
    }

    #[test]
    fn aof_commands_set_key() {
        let mut store = Store::new();
        let _ = store.sadd(b"myset", &[b"x".to_vec(), b"y".to_vec()], 100);
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"SADD");
        assert_eq!(cmds[0][1], b"myset");
        // Members are sorted deterministically
        assert_eq!(cmds[0].len(), 4); // SADD key x y
    }

    #[test]
    fn aof_commands_sorted_set_key() {
        let mut store = Store::new();
        store
            .zadd(
                b"myzset",
                &[(1.5, b"alice".to_vec()), (2.0, b"bob".to_vec())],
                100,
            )
            .unwrap();
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"ZADD");
        assert_eq!(cmds[0][1], b"myzset");
        // Score-member pairs sorted by score then member
        assert_eq!(cmds[0].len(), 6); // ZADD key score1 member1 score2 member2
    }

    #[test]
    fn aof_commands_stream_key() {
        let mut store = Store::new();
        store
            .xadd(
                b"mystream",
                (1000, 0),
                &[(b"name".to_vec(), b"val".to_vec())],
                100,
            )
            .unwrap();
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0][0], b"XADD");
        assert_eq!(cmds[0][1], b"mystream");
        assert_eq!(cmds[0][2], b"1000-0");
        assert_eq!(cmds[0][3], b"name");
        assert_eq!(cmds[0][4], b"val");
    }

    #[test]
    fn aof_commands_stream_with_xsetid() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"k".to_vec(), b"v".to_vec())], 100)
            .unwrap();
        // XSETID sets a last-generated-id higher than any entry
        store.xsetid(b"s", (999, 0), 100).unwrap();
        let cmds = store.to_aof_commands(100);
        // XADD + XSETID
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0][0], b"XADD");
        assert_eq!(cmds[1][0], b"XSETID");
        assert_eq!(cmds[1][1], b"s");
        assert_eq!(cmds[1][2], b"999-0");
    }

    #[test]
    fn aof_commands_stream_with_consumer_group() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"k".to_vec(), b"v".to_vec())], 100)
            .unwrap();
        store
            .xgroup_create(b"s", b"grp1", (0, 0), false, 100)
            .unwrap();
        store
            .xgroup_create(b"s", b"grp2", (1, 0), false, 100)
            .unwrap();
        let cmds = store.to_aof_commands(100);
        // XADD + 2x XGROUP CREATE (no XSETID since none was explicitly set)
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0][0], b"XADD");
        // Groups sorted by name
        assert_eq!(cmds[1][0], b"XGROUP");
        assert_eq!(cmds[1][1], b"CREATE");
        assert_eq!(cmds[1][2], b"s");
        assert_eq!(cmds[1][3], b"grp1");
        assert_eq!(cmds[1][4], b"0-0");
        assert_eq!(cmds[2][0], b"XGROUP");
        assert_eq!(cmds[2][1], b"CREATE");
        assert_eq!(cmds[2][2], b"s");
        assert_eq!(cmds[2][3], b"grp2");
        assert_eq!(cmds[2][4], b"1-0");
    }

    #[test]
    fn aof_commands_stream_with_consumers_and_pending_entries() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"k".to_vec(), b"v".to_vec())], 100)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g", (0, 0), false, 100).unwrap());
        assert_eq!(
            store
                .xgroup_createconsumer(b"s", b"g", b"idle", 110)
                .expect("group must exist"),
            Some(true)
        );
        store
            .xreadgroup(
                b"s",
                b"g",
                b"alice",
                group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                120,
            )
            .expect("pending seed")
            .expect("group must exist");
        let _ = store
            .xclaim(
                b"s",
                b"g",
                b"bob",
                &[(1, 0)],
                StreamClaimOptions {
                    min_idle_time_ms: 0,
                    idle_ms: None,
                    time_ms: Some(250),
                    retry_count: Some(7),
                    force: false,
                    justid: false,
                    last_id: None,
                },
                130,
            )
            .expect("xclaim must succeed");

        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 6);
        assert_eq!(cmds[0][0], b"XADD");
        assert_eq!(
            cmds[1],
            vec![
                b"XGROUP".to_vec(),
                b"CREATE".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"1-0".to_vec(),
            ]
        );
        assert_eq!(
            cmds[2],
            vec![
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"alice".to_vec(),
            ]
        );
        assert_eq!(
            cmds[3],
            vec![
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"bob".to_vec(),
            ]
        );
        assert_eq!(
            cmds[4],
            vec![
                b"XGROUP".to_vec(),
                b"CREATECONSUMER".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"idle".to_vec(),
            ]
        );
        assert_eq!(
            cmds[5],
            vec![
                b"XCLAIM".to_vec(),
                b"s".to_vec(),
                b"g".to_vec(),
                b"bob".to_vec(),
                b"0".to_vec(),
                b"1-0".to_vec(),
                b"TIME".to_vec(),
                b"250".to_vec(),
                b"RETRYCOUNT".to_vec(),
                b"7".to_vec(),
                b"FORCE".to_vec(),
            ]
        );
    }

    #[test]
    fn aof_commands_deterministic_key_order() {
        let mut store = Store::new();
        store.set(b"z_key".to_vec(), b"1".to_vec(), None, 100);
        store.set(b"a_key".to_vec(), b"2".to_vec(), None, 100);
        store.set(b"m_key".to_vec(), b"3".to_vec(), None, 100);
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 3);
        // Keys should be sorted alphabetically
        assert_eq!(cmds[0][1], b"a_key");
        assert_eq!(cmds[1][1], b"m_key");
        assert_eq!(cmds[2][1], b"z_key");
    }

    #[test]
    fn aof_commands_mixed_types() {
        let mut store = Store::new();
        store.set(b"str".to_vec(), b"val".to_vec(), None, 100);
        store
            .hset(b"hash", b"f".to_vec(), b"v".to_vec(), 100)
            .unwrap();
        let _ = store.rpush(b"list", &[b"item".to_vec()], 100);
        let _ = store.sadd(b"set", &[b"member".to_vec()], 100);
        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 4);
        let commands: Vec<&[u8]> = cmds.iter().map(|c| c[0].as_slice()).collect();
        // All data types represented
        assert!(commands.contains(&b"SET".as_slice()));
        assert!(commands.contains(&b"HSET".as_slice()));
        assert!(commands.contains(&b"RPUSH".as_slice()));
        assert!(commands.contains(&b"SADD".as_slice()));
    }

    #[test]
    fn aof_commands_include_function_libraries_before_keys() {
        let alpha = sample_function_library("alpha", "afn1", "afn2");
        let beta = sample_function_library("beta", "bfn1", "bfn2");
        let mut store = Store::new();
        store
            .function_load(&beta, false)
            .expect("beta library must load");
        store
            .function_load(&alpha, false)
            .expect("alpha library must load");
        store.set(b"key".to_vec(), b"value".to_vec(), None, 100);

        let cmds = store.to_aof_commands(100);
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0][0], b"FUNCTION");
        assert_eq!(cmds[0][1], b"LOAD");
        assert_eq!(cmds[0][2], b"REPLACE");
        assert_eq!(cmds[0][3], alpha);
        assert_eq!(cmds[1][0], b"FUNCTION");
        assert_eq!(cmds[1][1], b"LOAD");
        assert_eq!(cmds[1][2], b"REPLACE");
        assert_eq!(cmds[1][3], beta);
        assert_eq!(cmds[2][0], b"SET");
        assert_eq!(cmds[2][1], b"key");
        assert_eq!(cmds[2][2], b"value");
    }

    #[test]
    fn aof_commands_function_libraries_precede_multidb_selects_and_keys() {
        let alpha = sample_function_library("alpha", "afn1", "afn2");
        let beta = sample_function_library("beta", "bfn1", "bfn2");
        let mut store = Store::new();
        store
            .function_load(&beta, false)
            .expect("beta library must load");
        store
            .function_load(&alpha, false)
            .expect("alpha library must load");
        store.set(b"db0".to_vec(), b"v0".to_vec(), None, 100);
        store.set(encode_db_key(1, b"db1"), b"v1".to_vec(), None, 100);

        let cmds = store.to_aof_commands(100);
        assert_eq!(
            cmds,
            vec![
                vec![
                    b"FUNCTION".to_vec(),
                    b"LOAD".to_vec(),
                    b"REPLACE".to_vec(),
                    alpha,
                ],
                vec![
                    b"FUNCTION".to_vec(),
                    b"LOAD".to_vec(),
                    b"REPLACE".to_vec(),
                    beta,
                ],
                vec![b"SET".to_vec(), b"db0".to_vec(), b"v0".to_vec()],
                vec![b"SELECT".to_vec(), b"1".to_vec()],
                vec![b"SET".to_vec(), b"db1".to_vec(), b"v1".to_vec()],
            ]
        );
        assert!(
            !cmds
                .iter()
                .any(|argv| argv.len() == 2 && argv[0] == b"SELECT" && argv[1] == b"0"),
            "DB 0 must remain implicit even when function libraries precede multi-DB keys"
        );
    }

    #[test]
    fn aof_commands_function_libraries_precede_multidb_selects_keys_and_expiries() {
        let alpha = sample_function_library("alpha", "afn1", "afn2");
        let beta = sample_function_library("beta", "bfn1", "bfn2");
        let mut store = Store::new();
        store
            .function_load(&beta, false)
            .expect("beta library must load");
        store
            .function_load(&alpha, false)
            .expect("alpha library must load");
        store.set(b"a0".to_vec(), b"va0".to_vec(), Some(7000), 100);
        store.set(encode_db_key(1, b"b1"), b"vb1".to_vec(), Some(6000), 100);

        let cmds = store.to_aof_commands(100);
        assert_eq!(
            cmds,
            vec![
                vec![
                    b"FUNCTION".to_vec(),
                    b"LOAD".to_vec(),
                    b"REPLACE".to_vec(),
                    alpha,
                ],
                vec![
                    b"FUNCTION".to_vec(),
                    b"LOAD".to_vec(),
                    b"REPLACE".to_vec(),
                    beta,
                ],
                vec![b"SET".to_vec(), b"a0".to_vec(), b"va0".to_vec()],
                vec![b"PEXPIREAT".to_vec(), b"a0".to_vec(), b"7100".to_vec()],
                vec![b"SELECT".to_vec(), b"1".to_vec()],
                vec![b"SET".to_vec(), b"b1".to_vec(), b"vb1".to_vec()],
                vec![b"PEXPIREAT".to_vec(), b"b1".to_vec(), b"6100".to_vec()],
            ]
        );
        assert!(
            !cmds
                .iter()
                .any(|argv| argv.len() == 2 && argv[0] == b"SELECT" && argv[1] == b"0"),
            "DB 0 must remain implicit when function libraries precede multi-DB expiries"
        );
    }

    #[test]
    fn aof_commands_multidb_select_boundaries_are_minimal_and_stable() {
        let mut store = Store::new();
        store.set(b"z0".to_vec(), b"vz0".to_vec(), None, 100);
        store.set(b"a0".to_vec(), b"va0".to_vec(), Some(7000), 100);
        store.set(encode_db_key(1, b"b1"), b"vb1".to_vec(), Some(6000), 100);
        store.set(encode_db_key(1, b"a1"), b"va1".to_vec(), None, 100);
        store.set(encode_db_key(2, b"a2"), b"va2".to_vec(), None, 100);

        let cmds = store.to_aof_commands(100);
        assert_eq!(
            cmds,
            vec![
                vec![b"SET".to_vec(), b"a0".to_vec(), b"va0".to_vec()],
                vec![b"PEXPIREAT".to_vec(), b"a0".to_vec(), b"7100".to_vec()],
                vec![b"SET".to_vec(), b"z0".to_vec(), b"vz0".to_vec()],
                vec![b"SELECT".to_vec(), b"1".to_vec()],
                vec![b"SET".to_vec(), b"a1".to_vec(), b"va1".to_vec()],
                vec![b"SET".to_vec(), b"b1".to_vec(), b"vb1".to_vec()],
                vec![b"PEXPIREAT".to_vec(), b"b1".to_vec(), b"6100".to_vec()],
                vec![b"SELECT".to_vec(), b"2".to_vec()],
                vec![b"SET".to_vec(), b"a2".to_vec(), b"va2".to_vec()],
            ]
        );
        assert!(
            !cmds
                .iter()
                .any(|argv| argv.len() == 2 && argv[0] == b"SELECT" && argv[1] == b"0"),
            "DB 0 must stay implicit in rewritten AOF output"
        );
    }

    #[test]
    fn xreadgroup_increments_dirty_on_new_entries() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1000, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g", (0, 0), false, 0).unwrap());
        let before = store.dirty;
        store
            .xreadgroup(
                b"s",
                b"g",
                b"c",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                10,
            )
            .unwrap();
        assert!(
            store.dirty > before,
            "XREADGROUP with new entries must increment dirty for AOF"
        );
    }

    #[test]
    fn xclaim_increments_dirty() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        assert!(store.xgroup_create(b"s", b"g", (0, 0), false, 0).unwrap());
        store
            .xreadgroup(
                b"s",
                b"g",
                b"old_consumer",
                group_read_options(StreamGroupReadCursor::NewEntries, false, None),
                100,
            )
            .unwrap();
        let before = store.dirty;
        let opts = StreamClaimOptions {
            min_idle_time_ms: 0,
            idle_ms: None,
            time_ms: None,
            retry_count: None,
            force: true,
            justid: false,
            last_id: None,
        };
        let _ = store.xclaim(b"s", b"g", b"new_consumer", &[(1, 0)], opts, 200);
        assert!(store.dirty > before, "XCLAIM must increment dirty for AOF");
    }

    #[test]
    fn xgroup_create_increments_dirty() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        let before = store.dirty;
        assert!(store.xgroup_create(b"s", b"g", (0, 0), false, 0).unwrap());
        assert!(
            store.dirty > before,
            "XGROUP CREATE must increment dirty for AOF"
        );
    }

    #[test]
    fn xgroup_destroy_increments_dirty() {
        let mut store = Store::new();
        store
            .xadd(b"s", (1, 0), &[(b"f".to_vec(), b"v".to_vec())], 0)
            .unwrap();
        store.xgroup_create(b"s", b"g", (0, 0), false, 0).unwrap();
        let before = store.dirty;
        assert!(store.xgroup_destroy(b"s", b"g", 0).unwrap());
        assert!(
            store.dirty > before,
            "XGROUP DESTROY must increment dirty for AOF"
        );
    }

    #[test]
    fn ops_sec_sampling_computes_average() {
        let mut store = Store::new();
        // Simulate 100 commands processed over 100ms intervals.
        for i in 1..=16 {
            store.stat_total_commands_processed = i * 100;
            store.record_ops_sec_sample(100);
        }
        // Each sample: 100 ops in 100ms = 1000 ops/sec.
        assert_eq!(store.instantaneous_ops_per_sec(), 1000);
    }

    #[test]
    fn ops_sec_ring_buffer_wraps() {
        let mut store = Store::new();
        // Fill the 16-slot ring buffer with 500 ops/sec samples.
        // Each tick: 50 ops in 100ms = 500 ops/sec.
        for i in 1..=16 {
            store.stat_total_commands_processed = i * 50;
            store.record_ops_sec_sample(100);
        }
        assert_eq!(store.instantaneous_ops_per_sec(), 500);

        // Now overwrite all 16 slots with 1000 ops/sec samples.
        // Each tick: 100 ops in 100ms = 1000 ops/sec.
        let base = store.stat_total_commands_processed;
        for i in 1..=16 {
            store.stat_total_commands_processed = base + i * 100;
            store.record_ops_sec_sample(100);
        }
        assert_eq!(store.instantaneous_ops_per_sec(), 1000);
    }

    #[test]
    fn network_byte_counters_track_throughput() {
        let mut store = Store::new();
        store.stat_total_net_input_bytes = 10240;
        store.stat_total_net_output_bytes = 20480;
        store.record_ops_sec_sample(1000); // 1 second elapsed

        // 10240 bytes/sec = 10.0 KiB/sec input
        assert!((store.instantaneous_input_kbps() - 10.0 / 16.0).abs() < 0.01);
        // 20480 bytes/sec = 20.0 KiB/sec output
        assert!((store.instantaneous_output_kbps() - 20.0 / 16.0).abs() < 0.01);
    }

    #[test]
    fn periodic_sampling_updates_rss_and_peak_memory_stats() {
        let mut store = Store::new();
        store.set(b"rss".to_vec(), b"value".to_vec(), None, 0);

        store.record_ops_sec_sample(100);

        assert!(store.stat_used_memory_rss > 0);
        assert_eq!(store.stat_used_memory_peak, store.stat_used_memory_rss);
    }

    #[test]
    fn reset_info_stats_clears_network_counters() {
        let mut store = Store::new();
        store.stat_total_net_input_bytes = 1000;
        store.stat_total_net_output_bytes = 2000;
        store.stat_used_memory_rss = 3000;
        store.stat_used_memory_peak = 4000;
        store.stat_total_commands_processed = 500;
        store.record_command_histogram("GET", 123);
        store.record_ops_sec_sample(100);
        store.reset_info_stats();
        assert_eq!(store.stat_total_net_input_bytes, 0);
        assert_eq!(store.stat_total_net_output_bytes, 0);
        assert_eq!(store.stat_used_memory_rss, 0);
        assert_eq!(store.stat_used_memory_peak, 0);
        assert_eq!(store.instantaneous_ops_per_sec(), 0);
        assert_eq!(store.instantaneous_input_kbps(), 0.0);
        assert!(store.all_command_histograms().is_empty());
    }

    #[test]
    fn function_dump_restore_roundtrip_preserves_library_snapshot() {
        let mut original = Store::new();
        let library = sample_function_library("seedlib", "alpha", "beta");
        original
            .function_load(&library, false)
            .expect("seed library must load");
        let expected = function_library_snapshot(&original);
        let dumped = original.function_dump();

        let mut restored = Store::new();
        restored
            .function_restore(&dumped, "REPLACE")
            .expect("self-generated FUNCTION DUMP payload must restore");

        assert_eq!(function_library_snapshot(&restored), expected);
    }

    #[test]
    fn function_dump_body_uses_upstream_function2_records() {
        let mut original = Store::new();
        let library = sample_function_library("seedlib", "alpha", "beta");
        original
            .function_load(&library, false)
            .expect("seed library must load");

        let dumped = original.function_dump();
        let body_end = dumped.len() - 10;
        let body = &dumped[..body_end];

        assert_eq!(
            body[0], RDB_OPCODE_FUNCTION2,
            "FUNCTION DUMP body must start with upstream FUNCTION2 opcode"
        );
        let (code, consumed) =
            decode_rdb_string(body, 1, body.len()).expect("decode function library code");
        assert_eq!(1 + consumed, body.len());
        assert_eq!(code, library);
    }

    #[test]
    fn function_load_rejects_invalid_library_name_chars_with_upstream_wording() {
        // Upstream functions.c::functionsVerifyName restricts
        // library names to [A-Za-z0-9_] and rejects everything
        // else with the documented wording. (br-frankenredis-r85v)
        let mut store = Store::new();
        // Note: shebang names with whitespace get truncated at the
        // first space by the split_whitespace tokenizer (so
        // "lib bad" parses as "lib" which is valid). The cases
        // below all contain forbidden chars in a single token.
        for bad in [
            "lib-with-dashes",
            "lib.with.dots",
            "lib!",
            "café",
            "lib/slash",
        ] {
            let code =
                format!("#!lua name={bad}\nredis.register_function('fn', function() return 1 end)");
            let err = store
                .function_load(code.as_bytes(), false)
                .expect_err("invalid name must be rejected");
            assert_eq!(
                err,
                StoreError::GenericError(
                    "ERR Library names can only contain letters, numbers, or underscores(_) and must be at least one character long"
                        .to_string()
                ),
                "name {bad:?} should hit the verify-name gate"
            );
        }
        // Valid names still pass.
        let code = b"#!lua name=lib_123_OK\nredis.register_function('fn', function() return 1 end)";
        store
            .function_load(code, false)
            .expect("valid library name must load");
    }

    #[test]
    fn function_load_rejects_unregistered_engine_with_upstream_wording() {
        // Upstream functions.c::functionsCreateWithLibraryCtx emits
        // "Engine '<name>' not found" when the meta header refers
        // to an engine that wasn't registered. We only model LUA;
        // any other engine name must hit the same gate, not silently
        // load. (br-frankenredis-r85v)
        let mut store = Store::new();
        let code = b"#!python name=lib\nprint('not lua')";
        let err = store
            .function_load(code, false)
            .expect_err("non-lua engine must be rejected");
        assert_eq!(
            err,
            StoreError::GenericError("ERR Engine 'PYTHON' not found".to_string())
        );

        // Plain LUA still loads.
        let lua_code = b"#!lua name=ok\nredis.register_function('fn', function() return 1 end)";
        store
            .function_load(lua_code, false)
            .expect("lua engine must load");
    }

    #[test]
    fn function_load_rejects_invalid_function_name_chars() {
        // Upstream functions.c::functionLibCreateFunction (line 249)
        // applies functionsVerifyName to per-function names. A
        // forged register_function call with a hyphen must fail
        // loudly, not record an invalid function entry.
        // (br-frankenredis-r85v)
        let mut store = Store::new();
        let code =
            b"#!lua name=lib_fnv\nredis.register_function('bad-name', function() return 1 end)";
        let err = store
            .function_load(code, false)
            .expect_err("invalid function name must be rejected");
        assert_eq!(
            err,
            StoreError::GenericError(
                "ERR Library names can only contain letters, numbers, or underscores(_) and must be at least one character long"
                    .to_string()
            )
        );
        assert!(
            !store.function_libraries.contains_key("lib_fnv"),
            "library must not be registered when a function name is invalid"
        );
    }

    #[test]
    fn function_load_meta_header_validation_order_matches_upstream() {
        // Upstream functions.c validates the meta header as
        // engine-present → engine-registered → name-present →
        // name-charset. Confirm our gates fire in the same order
        // so a script with multiple defects surfaces the upstream-
        // expected error first. (br-frankenredis-r85v)
        let mut store = Store::new();

        // No engine, no name → "Missing library metadata".
        let err = store
            .function_load(b"#!\nx", false)
            .expect_err("missing engine must error");
        assert_eq!(
            err,
            StoreError::GenericError("ERR Missing library metadata".to_string())
        );

        // Bad engine + bad name → engine error fires first.
        let err = store
            .function_load(b"#!python name=lib-bad\nx", false)
            .expect_err("bad engine must error first");
        assert_eq!(
            err,
            StoreError::GenericError("ERR Engine 'PYTHON' not found".to_string())
        );

        // Good engine + missing name → name-not-given fires.
        let err = store
            .function_load(b"#!lua\nx", false)
            .expect_err("missing name must error");
        assert_eq!(
            err,
            StoreError::GenericError("ERR Library name was not given".to_string())
        );

        // Good engine + bad-charset name → charset error fires.
        let err = store
            .function_load(b"#!lua name=lib-bad\nx", false)
            .expect_err("bad name charset must error");
        assert_eq!(
            err,
            StoreError::GenericError(
                "ERR Library names can only contain letters, numbers, or underscores(_) and must be at least one character long"
                    .to_string()
            )
        );
    }

    #[test]
    fn function_load_table_form_does_not_confuse_value_substring_with_key() {
        // Regression: when function_name='describe_user', the
        // `description` substring inside the value used to bleed
        // into the description slot via a naive find('description').
        // The new key-boundary parser ignores substring matches
        // that aren't real `key=` tokens. (br-frankenredis-r85v)
        let mut store = Store::new();
        let code = b"#!lua name=lib_dbu\nredis.register_function{function_name='describe_user', callback=function() return 1 end}";
        store.function_load(code, false).expect("library must load");

        let lib = store
            .function_libraries
            .get("lib_dbu")
            .expect("library present");
        assert_eq!(lib.functions.len(), 1);
        assert_eq!(lib.functions[0].name, "describe_user");
        assert_eq!(
            lib.functions[0].description, None,
            "description must remain None when the table omits the field"
        );
    }

    #[test]
    fn function_load_extracts_per_function_description_from_table_form() {
        // Upstream function_lua.c accepts `description` in
        // redis.register_function{function_name=...,
        //   description='d', callback=fn}. We previously dropped
        // the description, leaving FUNCTION LIST description=nil
        // even when the script set it. (br-frankenredis-r85v)
        let mut store = Store::new();
        let code = b"#!lua name=desclib\nredis.register_function{function_name='myfn', description='hello world', callback=function() return 1 end}";
        store.function_load(code, false).expect("library must load");

        let lib = store
            .function_libraries
            .get("desclib")
            .expect("library present");
        assert_eq!(lib.functions.len(), 1);
        assert_eq!(lib.functions[0].name, "myfn");
        assert_eq!(
            lib.functions[0].description.as_deref(),
            Some("hello world"),
            "description from register_function table-form must be captured"
        );
    }

    #[test]
    fn function_dump_restore_roundtrip_preserves_function_description_from_code() {
        // Upstream FUNCTION DUMP persists library code only; function
        // descriptions survive because restoring reloads the same
        // `redis.register_function{..., description=...}` code.
        // (br-frankenredis-r85v)
        let mut original = Store::new();
        let library = b"#!lua name=desclib\n\
            redis.register_function{function_name='fn1', description='the description', callback=function(keys, args) return 1 end}\n";
        original
            .function_load(library, false)
            .expect("seed library must load");

        let dumped = original.function_dump();
        let mut restored = Store::new();
        restored
            .function_restore(&dumped, "REPLACE")
            .expect("self-generated FUNCTION DUMP payload must restore");

        let lib = restored
            .function_libraries
            .get("desclib")
            .expect("library should be present");
        assert_eq!(lib.functions.len(), 1);
        assert_eq!(lib.functions[0].name, "fn1");
        assert_eq!(
            lib.functions[0].description.as_deref(),
            Some("the description")
        );
    }

    #[test]
    fn function_dump_restore_roundtrip_preserves_multilibrary_snapshot_order() {
        let alpha = sample_function_library("alpha", "afn1", "afn2");
        let beta = sample_function_library("beta", "bfn1", "bfn2");
        let mut original = Store::new();
        original
            .function_load(&beta, false)
            .expect("beta library must load");
        original
            .function_load(&alpha, false)
            .expect("alpha library must load");
        let expected = function_library_snapshot(&original);
        let dumped = original.function_dump();

        let mut restored = Store::new();
        restored
            .function_restore(&dumped, "REPLACE")
            .expect("self-generated multi-library FUNCTION DUMP payload must restore");

        assert_eq!(function_library_snapshot(&restored), expected);
        assert_eq!(restored.function_dump(), dumped);
    }

    #[test]
    fn function_restore_invalid_dump_is_atomic_even_with_flush_policy() {
        let mut store = Store::new();
        let library = sample_function_library("sentinel", "alpha", "beta");
        store
            .function_load(&library, false)
            .expect("sentinel library must load");
        let before = function_library_snapshot(&store);
        let invalid_dump = vec![1, 0, 0, 0, 4, 0, 0, 0, b'n', b'a', b'm', b'e'];

        let err = store
            .function_restore(&invalid_dump, "FLUSH")
            .expect_err("truncated function dump must fail");
        // After wrapping FUNCTION DUMP in the upstream version+CRC64
        // envelope (br-frankenredis-r83v), a payload without the
        // 10-byte footer fails the checksum gate first with the
        // upstream wording rather than the body-parse error.
        assert_eq!(
            err,
            StoreError::GenericError("ERR DUMP payload version or checksum are wrong".to_string())
        );
        assert_eq!(
            function_library_snapshot(&store),
            before,
            "failed FUNCTION RESTORE must not clear or partially mutate state"
        );
    }

    #[test]
    fn function_restore_rejects_payload_with_future_rdb_version() {
        // Upstream cluster.c::verifyDumpPayload rejects RDB versions
        // higher than the local RDB_VERSION. We mirror that so a
        // forged future-version FUNCTION DUMP can't slip past the
        // body parser. (br-frankenredis-r83v)
        let mut store = Store::new();
        let library = sample_function_library("vguard", "alpha", "beta");
        store
            .function_load(&library, false)
            .expect("seed library must load");
        let mut dump = store.function_dump();
        // Bump the version field (last 10 bytes are version+crc64;
        // version is bytes [..2] of the footer).
        let footer_offset = dump.len() - 10;
        // Replace version with FUNCTION_DUMP_RDB_VERSION + 1.
        let bumped = (Store::FUNCTION_DUMP_RDB_VERSION + 1).to_le_bytes();
        dump[footer_offset] = bumped[0];
        dump[footer_offset + 1] = bumped[1];
        // Recompute CRC so this fails the version gate, not the CRC
        // gate — proves the version check is the gate that fires.
        let new_crc = fr_persist::crc64_redis(&dump[..footer_offset + 2]);
        dump[footer_offset + 2..].copy_from_slice(&new_crc.to_le_bytes());

        let err = store
            .function_restore(&dump, "FLUSH")
            .expect_err("future-version dump must fail");
        assert_eq!(
            err,
            StoreError::GenericError("ERR DUMP payload version or checksum are wrong".to_string())
        );
    }

    #[test]
    fn function_restore_flush_replaces_existing_libraries() {
        let mut source = Store::new();
        source
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        let source_snapshot = function_library_snapshot(&source);
        let dumped = source.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("stale", "gamma", "delta"), false)
            .expect("stale library must load");

        restored
            .function_restore(&dumped, "FLUSH")
            .expect("FUNCTION RESTORE FLUSH must replace existing libraries");

        let restored_snapshot = function_library_snapshot(&restored);
        assert_eq!(restored_snapshot.len(), 1);
        assert_eq!(restored_snapshot[0].0, source_snapshot[0].0);
        assert_eq!(restored_snapshot[0].1, source_snapshot[0].1);
        assert_eq!(restored_snapshot[0].2, source_snapshot[0].2);
        let mut function_names = restored_snapshot[0].3.clone();
        function_names.sort();
        assert_eq!(
            function_names,
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn function_restore_unknown_policy_is_atomic() {
        let mut store = Store::new();
        store
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        let before_snapshot = function_library_snapshot(&store);
        let before_dump = store.function_dump();

        let err = store
            .function_restore(&before_dump, "BROKEN")
            .expect_err("unknown FUNCTION RESTORE policy must fail");
        assert_eq!(
            err,
            StoreError::GenericError(
                "ERR Wrong restore policy given, value should be either FLUSH, APPEND or REPLACE."
                    .to_string()
            )
        );
        assert_eq!(function_library_snapshot(&store), before_snapshot);
        assert_eq!(store.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_empty_policy_defaults_to_append_for_disjoint_payload() {
        let mut payload_store = Store::new();
        payload_store
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("payload addon library must load");
        let payload_dump = payload_store.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("existing seed library must load");

        restored
            .function_restore(&payload_dump, "")
            .expect("empty FUNCTION RESTORE policy must behave like APPEND");

        let mut expected = Store::new();
        expected
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("expected seed library must load");
        expected
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("expected addon library must load");

        assert_eq!(
            function_library_snapshot(&restored),
            function_library_snapshot(&expected)
        );
        assert_eq!(restored.function_dump(), expected.function_dump());
    }

    #[test]
    fn function_restore_empty_policy_collision_is_atomic() {
        let mut payload_store = Store::new();
        payload_store
            .function_load(&sample_function_library("seedlib", "gamma", "delta"), false)
            .expect("replacement seed library must load");
        payload_store
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("addon library must load");
        let payload_dump = payload_store.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("original seed library must load");
        restored
            .function_load(&sample_function_library("keepme", "theta", "iota"), false)
            .expect("disjoint existing library must load");
        let before_snapshot = function_library_snapshot(&restored);
        let before_dump = restored.function_dump();

        let err = restored
            .function_restore(&payload_dump, "")
            .expect_err("empty policy must default to APPEND collision semantics");
        assert!(
            matches!(err, StoreError::GenericError(ref message) if message.contains("already exists")),
            "unexpected empty-policy collision error: {err:?}"
        );
        assert_eq!(function_library_snapshot(&restored), before_snapshot);
        assert_eq!(restored.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_empty_policy_empty_dump_is_identity() {
        let empty_dump = Store::new().function_dump();
        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        restored
            .function_load(&sample_function_library("keepme", "theta", "iota"), false)
            .expect("disjoint existing library must load");
        let before_snapshot = function_library_snapshot(&restored);
        let before_dump = restored.function_dump();

        restored
            .function_restore(&empty_dump, "")
            .expect("empty policy with empty FUNCTION DUMP must behave like APPEND");

        assert_eq!(function_library_snapshot(&restored), before_snapshot);
        assert_eq!(restored.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_append_of_disjoint_payload_is_union() {
        let mut payload_store = Store::new();
        payload_store
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("payload addon library must load");
        let payload_dump = payload_store.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("existing seed library must load");

        restored
            .function_restore(&payload_dump, "APPEND")
            .expect("APPEND with disjoint library payload must succeed");

        let mut expected = Store::new();
        expected
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("expected seed library must load");
        expected
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("expected addon library must load");

        assert_eq!(
            function_library_snapshot(&restored),
            function_library_snapshot(&expected)
        );
        assert_eq!(restored.function_dump(), expected.function_dump());
    }

    #[test]
    fn function_restore_append_collision_is_atomic() {
        let mut payload_store = Store::new();
        payload_store
            .function_load(&sample_function_library("seedlib", "gamma", "delta"), false)
            .expect("replacement seed library must load");
        payload_store
            .function_load(&sample_function_library("addon", "epsilon", "zeta"), false)
            .expect("addon library must load");
        let payload_dump = payload_store.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("original seed library must load");
        restored
            .function_load(&sample_function_library("keepme", "theta", "iota"), false)
            .expect("disjoint existing library must load");
        let before_snapshot = function_library_snapshot(&restored);
        let before_dump = restored.function_dump();

        let err = restored
            .function_restore(&payload_dump, "APPEND")
            .expect_err("APPEND with colliding library must fail atomically");
        assert!(
            matches!(err, StoreError::GenericError(ref message) if message.contains("already exists")),
            "unexpected append collision error: {err:?}"
        );
        assert_eq!(function_library_snapshot(&restored), before_snapshot);
        assert_eq!(restored.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_replace_overwrites_collisions_and_keeps_disjoint_existing() {
        let mut payload_store = Store::new();
        payload_store
            .function_load(&sample_function_library("seedlib", "gamma", "delta"), false)
            .expect("replacement seed library must load");
        let payload_dump = payload_store.function_dump();

        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("original seed library must load");
        restored
            .function_load(&sample_function_library("keepme", "theta", "iota"), false)
            .expect("disjoint existing library must load");

        restored
            .function_restore(&payload_dump, "REPLACE")
            .expect("REPLACE must overwrite colliding libraries");

        let restored_snapshot = function_library_snapshot(&restored);
        assert_eq!(restored_snapshot.len(), 2);

        let keepme = restored_snapshot
            .iter()
            .find(|(name, _, _, _)| name == "keepme")
            .expect("disjoint existing library must remain");
        assert_eq!(keepme.1, "LUA");
        let mut keepme_functions = keepme.3.clone();
        keepme_functions.sort();
        assert_eq!(
            keepme_functions,
            vec!["iota".to_string(), "theta".to_string()]
        );

        let seedlib = restored_snapshot
            .iter()
            .find(|(name, _, _, _)| name == "seedlib")
            .expect("colliding library must remain after replacement");
        assert_eq!(seedlib.1, "LUA");
        let mut seedlib_functions = seedlib.3.clone();
        seedlib_functions.sort();
        assert_eq!(
            seedlib_functions,
            vec!["delta".to_string(), "gamma".to_string()]
        );
        assert_eq!(
            seedlib.2,
            sample_function_library("seedlib", "gamma", "delta")
        );
    }

    #[test]
    fn function_restore_append_empty_dump_preserves_existing_libraries() {
        let empty_dump = Store::new().function_dump();
        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        let before_snapshot = function_library_snapshot(&restored);
        let before_dump = restored.function_dump();

        restored
            .function_restore(&empty_dump, "APPEND")
            .expect("APPEND with empty FUNCTION DUMP must be a no-op");

        assert_eq!(function_library_snapshot(&restored), before_snapshot);
        assert_eq!(restored.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_replace_empty_dump_preserves_existing_libraries() {
        let empty_dump = Store::new().function_dump();
        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        restored
            .function_load(&sample_function_library("stale", "gamma", "delta"), false)
            .expect("stale library must load");
        let before_snapshot = function_library_snapshot(&restored);
        let before_dump = restored.function_dump();

        restored
            .function_restore(&empty_dump, "REPLACE")
            .expect("REPLACE with empty FUNCTION DUMP must keep disjoint libraries");

        assert_eq!(function_library_snapshot(&restored), before_snapshot);
        assert_eq!(restored.function_dump(), before_dump);
    }

    #[test]
    fn function_restore_flush_empty_dump_clears_existing_libraries() {
        let empty_dump = Store::new().function_dump();
        let mut restored = Store::new();
        restored
            .function_load(&sample_function_library("seedlib", "alpha", "beta"), false)
            .expect("seed library must load");
        restored
            .function_load(&sample_function_library("stale", "gamma", "delta"), false)
            .expect("stale library must load");

        restored
            .function_restore(&empty_dump, "FLUSH")
            .expect("FLUSH with empty FUNCTION DUMP must clear existing libraries");

        assert!(function_library_snapshot(&restored).is_empty());
        assert_eq!(restored.function_dump(), empty_dump);
    }

    /// Lock the contract for the structured corpus seeds in
    /// `fuzz/corpus/fuzz_function_restore/`. The fuzz harness
    /// dispatches the first byte three ways (`% 3`):
    ///
    ///     0 → fuzz_raw_function_source
    ///     2 → fuzz_raw_function_restore (body[0] = policy_selector)
    ///
    /// The seed generator (`fuzz/scripts/gen_function_restore_seeds.py`)
    /// crafts payloads that exercise each branch of those code paths.
    /// This test verifies that
    ///
    ///   1. The "valid envelope" seeds actually round-trip through
    ///      `function_restore`, so libfuzzer starts from a corpus that
    ///      reaches deep into the parser instead of dying at the
    ///      version/CRC gate.
    ///   2. The "expected-failure" seeds produce the exact upstream
    ///      error wording, so libfuzzer mutations that drift toward
    ///      these payloads don't get the harness stuck on a phantom
    ///      success.
    #[test]
    fn fuzz_function_restore_corpus_matches_documented_contract() {
        use std::path::Path;

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_function_restore");
        if !corpus_root.exists() {
            // Corpus is generated and committed; skip if a checkout
            // strips the fuzz tree (e.g. `cargo package`).
            return;
        }

        // ── Strip the 2-byte mode+policy header that the fuzz harness
        //    consumes before calling function_restore.
        fn restore_payload(seed_bytes: &[u8]) -> &[u8] {
            assert!(seed_bytes.len() >= 2, "restore seed too short");
            assert_eq!(
                seed_bytes[0], 0x02,
                "restore seed mode byte must be 0x02 (mode % 3 == 2)"
            );
            &seed_bytes[2..]
        }

        fn read_seed(corpus_root: &Path, name: &str) -> Vec<u8> {
            std::fs::read(corpus_root.join(name))
                .unwrap_or_else(|err| panic!("read seed {name}: {err}"))
        }

        // Valid single-library payload must restore under every
        // legitimate policy.
        for (seed_name, policy) in [
            ("restore_valid_single_lib_replace.dump", "REPLACE"),
            ("restore_valid_single_lib_flush.dump", "FLUSH"),
            ("restore_valid_two_libs_append.dump", "APPEND"),
            ("restore_empty_libraries_marker_append.dump", "APPEND"),
        ] {
            let seed = read_seed(&corpus_root, seed_name);
            let payload = restore_payload(&seed);
            let mut store = Store::new();
            store
                .function_restore(payload, policy)
                .unwrap_or_else(|err| panic!("seed {seed_name} must restore cleanly: {err:?}"));
        }

        // The BOGUS-policy seed wraps the same valid envelope but
        // selects an unsupported policy string ("BOGUS") via
        // body[0] % 4 == 3. Upstream functions.c rejects this with
        // the exact wording below, regardless of payload validity.
        let bogus = read_seed(&corpus_root, "restore_valid_single_lib_bogus_policy.dump");
        let bogus_payload = restore_payload(&bogus);
        let err = Store::new()
            .function_restore(bogus_payload, "BOGUS")
            .expect_err("BOGUS policy must be rejected");
        assert_eq!(
            err,
            StoreError::GenericError(
                "ERR Wrong restore policy given, value should be either FLUSH, APPEND or REPLACE."
                    .to_string()
            )
        );

        // ── Expected-failure restore seeds ──────────────────────────
        // Each should surface upstream-shaped error text, never panic.
        let cases: &[(&str, &str)] = &[
            (
                "restore_pre_ga_opcode.dump",
                "ERR Pre-GA function format not supported",
            ),
            (
                "restore_unknown_opcode.dump",
                "ERR given type is not a function",
            ),
            (
                "restore_future_rdb_version.dump",
                "ERR DUMP payload version or checksum are wrong",
            ),
            (
                "restore_corrupted_crc.dump",
                "ERR DUMP payload version or checksum are wrong",
            ),
            (
                // (br-frankenredis-ngn0) — upstream verifyDumpPayload
                // emits the same DUMP-version/checksum error for the
                // too-short-envelope branch.
                "restore_truncated_below_footer.dump",
                "ERR DUMP payload version or checksum are wrong",
            ),
            (
                "restore_inner_load_fails_missing_header.dump",
                "ERR Missing library metadata",
            ),
        ];
        for (seed_name, expected_msg) in cases {
            let seed = read_seed(&corpus_root, seed_name);
            let payload = restore_payload(&seed);
            let err = Store::new()
                .function_restore(payload, "APPEND")
                .expect_err(&format!("seed {seed_name} must reject"));
            assert_eq!(
                err,
                StoreError::GenericError(expected_msg.to_string()),
                "seed {seed_name} surfaced unexpected error",
            );
        }

        // ── Mode-0 source seeds: a few sanity checks on the harness
        //    contract. The first byte gates the dispatch; body bytes
        //    are fed straight into function_load. Verify the valid
        //    seed loads and the headerless seed surfaces the right
        //    wording.
        let valid_source = read_seed(&corpus_root, "source_call_form_simple.lua");
        assert_eq!(valid_source[0], 0x00, "source seed mode byte must be 0x00");
        let mut store = Store::new();
        store
            .function_load(&valid_source[1..], false)
            .expect("source_call_form_simple.lua must load");

        let bad_source = read_seed(&corpus_root, "source_no_shebang.lua");
        let err = Store::new()
            .function_load(&bad_source[1..], false)
            .expect_err("source_no_shebang.lua must reject");
        assert_eq!(
            err,
            StoreError::GenericError("ERR Missing library metadata".to_string()),
        );
    }

    /// Lock the contract for the structured corpus seeds in
    /// `fuzz/corpus/fuzz_dump_restore/`. The fuzz harness derives a
    /// `DumpRestoreInput` enum (Valid or Raw) from the input bytes
    /// and exercises both `Store::dump_key` round-trips and
    /// `Store::restore_key` error paths. The seed generator
    /// (`fuzz/scripts/gen_dump_restore_seeds.py`) writes byte-level
    /// DUMP envelopes covering each meaningful branch:
    ///
    ///   - String / List / Set / Hash / ZSET_2 valid round-trips
    ///   - Truncated / corrupted-CRC / future-version envelopes
    ///   - Unknown type bytes
    ///   - Length-prefix overruns the body
    ///
    /// The test verifies:
    ///   1. Every accept seed restores cleanly into a fresh store.
    ///   2. The dump-then-redump round-trip is byte-identical (same
    ///      property `fuzz_valid_roundtrip` asserts).
    ///   3. Every reject seed surfaces an Err without panicking.
    #[test]
    fn fuzz_dump_restore_corpus_matches_documented_contract() {
        use crate::Store;
        use std::path::Path;

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_dump_restore");
        if !corpus_root.exists() {
            return;
        }

        fn read_seed(corpus_root: &Path, name: &str) -> Vec<u8> {
            std::fs::read(corpus_root.join(name))
                .unwrap_or_else(|err| panic!("read seed {name}: {err}"))
        }

        // Accept-class: each must restore cleanly AND the
        // dump-then-redump round-trip must be byte-identical.
        let accepts: &[&str] = &[
            "dump_string_short_ascii",
            "dump_string_empty",
            "dump_string_binary_with_nul",
            "dump_string_64_bytes_just_over_threshold",
            "dump_string_300_bytes",
            "dump_list_single_element",
            "dump_list_three_elements",
            "dump_set_single_member",
            "dump_set_three_members",
            "dump_hash_single_field",
            "dump_hash_three_fields",
            "dump_zset2_single_member",
            "dump_zset2_three_members",
            "dump_zset2_negative_scores",
        ];
        assert!(
            accepts.len() >= 14,
            "fuzz_dump_restore accept seeds must have >= 14 entries"
        );
        for name in accepts {
            let payload = read_seed(&corpus_root, name);
            let mut store = Store::new();
            store
                .restore_key(b"k", 0, &payload, false, 100)
                .unwrap_or_else(|err| panic!("seed {name} must restore: {err:?}"));

            // Dump→redump round-trip identity.
            let redumped = store
                .dump_key(b"k", 100)
                .unwrap_or_else(|| panic!("seed {name} must redump after restore"));
            let mut store2 = Store::new();
            store2
                .restore_key(b"k", 0, &redumped, false, 100)
                .unwrap_or_else(|err| {
                    panic!("seed {name} re-dumped payload must re-restore: {err:?}")
                });
            let redumped2 = store2
                .dump_key(b"k", 100)
                .unwrap_or_else(|| panic!("seed {name} must redump twice"));
            assert_eq!(
                redumped, redumped2,
                "seed {name} dump-then-redump must be byte-identical"
            );
        }

        // Reject-class: each must surface an Err. We don't pin the
        // exact StoreError variant — different shapes hit different
        // branches (InvalidDumpPayload for header / CRC / version
        // failures; the unknown-type-byte and length-overrun paths
        // also surface InvalidDumpPayload via decode_rdb_string).
        let rejects: &[&str] = &[
            "dump_truncated_below_minimum",
            "dump_truncated_zero_bytes",
            "dump_corrupted_crc",
            "dump_future_version",
            "dump_unknown_type_byte",
            "dump_string_length_overruns_body",
            "dump_type_byte_only_no_body",
            "dump_list_count_overruns_body",
        ];
        for name in rejects {
            let payload = read_seed(&corpus_root, name);
            let mut store = Store::new();
            assert!(
                store.restore_key(b"k", 0, &payload, false, 100).is_err(),
                "seed {name} must reject (got Ok)"
            );
        }
    }

    #[test]
    fn fuzz_keyspace_events_corpus_matches_documented_contract() {
        use crate::{
            NOTIFY_ALL, NOTIFY_EVICTED, NOTIFY_EXPIRED, NOTIFY_GENERIC, NOTIFY_HASH,
            NOTIFY_KEY_MISS, NOTIFY_KEYEVENT, NOTIFY_KEYSPACE, NOTIFY_LIST, NOTIFY_NEW, NOTIFY_SET,
            NOTIFY_STREAM, NOTIFY_STRING, NOTIFY_ZSET, keyspace_events_parse,
            keyspace_events_to_string,
        };
        use std::path::Path;

        let corpus_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/fuzz_keyspace_events");
        if !corpus_root.exists() {
            return;
        }

        fn read_seed(corpus_root: &Path, name: &str) -> String {
            let bytes = std::fs::read(corpus_root.join(name))
                .unwrap_or_else(|err| panic!("read seed {name}: {err}"));
            String::from_utf8_lossy(&bytes).into_owned()
        }

        // Accept-class seeds with their expected flag bitmaps.
        // Empty input → 0; per-char accept; AKE → ALL+K+E etc.
        let accepts: &[(&str, u32)] = &[
            ("empty.txt", 0),
            (
                "canonical_AKE.txt",
                NOTIFY_ALL | NOTIFY_KEYSPACE | NOTIFY_KEYEVENT,
            ),
            (
                "KEA_alternate_ordering.txt",
                NOTIFY_ALL | NOTIFY_KEYSPACE | NOTIFY_KEYEVENT,
            ),
            // K alone, E alone, KE without class chars — the parser
            // KEEPS the K/E bits in the bitmap. The "disable all"
            // post-pass only fires when class bits are set but
            // neither K nor E is set; so K-only / E-only / KE pass
            // through unchanged. (No notifications actually fire on
            // these because the dispatcher checks both that K/E is
            // set AND that a class bit is set, but the bitmap shape
            // is what we lock here.)
            ("K_alone_no_class_disables.txt", NOTIFY_KEYSPACE),
            ("E_alone_no_class_disables.txt", NOTIFY_KEYEVENT),
            (
                "KE_no_class_disables.txt",
                NOTIFY_KEYSPACE | NOTIFY_KEYEVENT,
            ),
            ("Kg_generic_only.txt", NOTIFY_KEYSPACE | NOTIFY_GENERIC),
            (
                "Egn_generic_plus_new.txt",
                NOTIFY_KEYEVENT | NOTIFY_GENERIC | NOTIFY_NEW,
            ),
            (
                "KEm_key_miss.txt",
                NOTIFY_KEYSPACE | NOTIFY_KEYEVENT | NOTIFY_KEY_MISS,
            ),
            (
                "KEt_stream.txt",
                NOTIFY_KEYSPACE | NOTIFY_KEYEVENT | NOTIFY_STREAM,
            ),
            (
                "KEn_new_key_redis7.txt",
                NOTIFY_KEYSPACE | NOTIFY_KEYEVENT | NOTIFY_NEW,
            ),
            // KEA + redundant `$` — A already includes string class.
            (
                "KEA_dollar_idempotent.txt",
                NOTIFY_ALL | NOTIFY_KEYSPACE | NOTIFY_KEYEVENT,
            ),
            ("K_string_class.txt", NOTIFY_KEYSPACE | NOTIFY_STRING),
            ("E_list_class.txt", NOTIFY_KEYEVENT | NOTIFY_LIST),
            ("K_set_class.txt", NOTIFY_KEYSPACE | NOTIFY_SET),
            ("K_hash_class.txt", NOTIFY_KEYSPACE | NOTIFY_HASH),
            ("K_zset_class.txt", NOTIFY_KEYSPACE | NOTIFY_ZSET),
            ("K_expired_class.txt", NOTIFY_KEYSPACE | NOTIFY_EXPIRED),
            ("K_evicted_class.txt", NOTIFY_KEYSPACE | NOTIFY_EVICTED),
            ("K_stream_class.txt", NOTIFY_KEYSPACE | NOTIFY_STREAM),
            ("K_keymiss_class.txt", NOTIFY_KEYSPACE | NOTIFY_KEY_MISS),
            ("K_new_class.txt", NOTIFY_KEYSPACE | NOTIFY_NEW),
            // `KEg$lshzxetmn` enumerates every class char alongside
            // K + E. Equals union of all class bits + K + E.
            (
                "KE_all_per_class_chars.txt",
                NOTIFY_KEYSPACE
                    | NOTIFY_KEYEVENT
                    | NOTIFY_GENERIC
                    | NOTIFY_STRING
                    | NOTIFY_LIST
                    | NOTIFY_SET
                    | NOTIFY_HASH
                    | NOTIFY_ZSET
                    | NOTIFY_EXPIRED
                    | NOTIFY_EVICTED
                    | NOTIFY_STREAM
                    | NOTIFY_KEY_MISS
                    | NOTIFY_NEW,
            ),
            (
                "KEA_with_explicit_redundants.txt",
                NOTIFY_ALL | NOTIFY_KEYSPACE | NOTIFY_KEYEVENT | NOTIFY_KEY_MISS | NOTIFY_NEW,
            ),
        ];

        for (name, expected) in accepts {
            let body = read_seed(&corpus_root, name);
            let parsed =
                keyspace_events_parse(&body).unwrap_or_else(|| panic!("seed {name} must parse"));
            assert_eq!(
                parsed, *expected,
                "seed {name} parse flags mismatch (got {parsed:#x}, expected {expected:#x})"
            );
            // Canonical round-trip property the fuzz target asserts,
            // accounting for the documented `n`-when-A-set lossy
            // drop (mirrors upstream notify.c).
            let canonical = keyspace_events_to_string(parsed);
            let expected_after_canonical = if (parsed & NOTIFY_ALL) == NOTIFY_ALL {
                parsed & !NOTIFY_NEW
            } else {
                parsed
            };
            assert_eq!(
                keyspace_events_parse(&canonical),
                Some(expected_after_canonical),
                "seed {name} canonical round-trip broke"
            );
        }

        // Reject-class seeds: parser must return None.
        let rejects: &[&str] = &[
            "lowercase_k_rejected.txt",
            "uppercase_g_rejected.txt",
            "question_mark.txt",
            "digit_7.txt",
            "space_only.txt",
            "tab_only.txt",
            "K_then_invalid_char.txt",
            "KEA_then_invalid.txt",
            "snowman_unicode.txt",
            "crlf_only.txt",
        ];
        for name in rejects {
            let body = read_seed(&corpus_root, name);
            assert_eq!(
                keyspace_events_parse(&body),
                None,
                "seed {name} must reject (got Some(_))"
            );
        }
    }

    // ── Golden artifact tests ──────────────────────────────────────────
    mod golden {
        use crate::{
            DB_NAMESPACE_PREFIX, decode_db_key, encode_db_key, glob_match, keyspace_events_parse,
            keyspace_events_to_string,
        };

        #[test]
        fn golden_encode_db_key_db0_passthrough() {
            let key = b"mykey";
            let encoded = encode_db_key(0, key);
            assert_eq!(encoded, key.to_vec(), "DB 0 keys must be unchanged");
        }

        #[test]
        fn golden_encode_db_key_db1_prefix() {
            let key = b"k";
            let encoded = encode_db_key(1, key);
            let golden = b"\0frdb\0\x00\x00\x00\x00\x00\x00\x00\x01k";
            assert_eq!(encoded, golden.to_vec(), "DB 1 key encoding changed");
        }

        #[test]
        fn golden_encode_db_key_db15_prefix() {
            let key = b"test";
            let encoded = encode_db_key(15, key);
            let golden = b"\0frdb\0\x00\x00\x00\x00\x00\x00\x00\x0ftest";
            assert_eq!(encoded, golden.to_vec(), "DB 15 key encoding changed");
        }

        #[test]
        fn golden_db_namespace_prefix() {
            assert_eq!(
                DB_NAMESPACE_PREFIX, b"\0frdb\0",
                "DB namespace prefix must be \\0frdb\\0"
            );
        }

        #[test]
        fn golden_decode_db_key_roundtrip() {
            for db in [1, 5, 15] {
                let key = b"testkey";
                let encoded = encode_db_key(db, key);
                let (decoded_db, decoded_key) =
                    decode_db_key(&encoded).expect("decode must succeed");
                assert_eq!(decoded_db, db);
                assert_eq!(decoded_key, key);
            }
        }

        #[test]
        fn golden_decode_db_key_rejects_unprefixed() {
            let unprefixed = b"plainkey";
            assert!(
                decode_db_key(unprefixed).is_none(),
                "Unprefixed keys must return None"
            );
        }

        #[test]
        fn golden_decode_db_key_rejects_short() {
            let short = b"\0frdb\0\x00\x00"; // Too short for full db number
            assert!(
                decode_db_key(short).is_none(),
                "Short keys must return None"
            );
        }

        #[test]
        fn golden_glob_match_exact() {
            assert!(glob_match(b"hello", b"hello"));
            assert!(!glob_match(b"hello", b"world"));
        }

        #[test]
        fn golden_glob_match_star_any() {
            assert!(glob_match(b"*", b"anything"));
            assert!(glob_match(b"*", b""));
        }

        #[test]
        fn golden_glob_match_question_single() {
            assert!(glob_match(b"h?llo", b"hello"));
            assert!(glob_match(b"h?llo", b"hallo"));
            assert!(!glob_match(b"h?llo", b"hllo")); // ? must match exactly one char
        }

        #[test]
        fn golden_glob_match_prefix() {
            assert!(glob_match(b"prefix*", b"prefix"));
            assert!(glob_match(b"prefix*", b"prefix_suffix"));
            assert!(!glob_match(b"prefix*", b"other"));
        }

        #[test]
        fn golden_glob_match_suffix() {
            assert!(glob_match(b"*suffix", b"suffix"));
            assert!(glob_match(b"*suffix", b"prefix_suffix"));
            assert!(!glob_match(b"*suffix", b"suffixnot"));
        }

        #[test]
        fn golden_glob_match_escape() {
            assert!(glob_match(b"hello\\*world", b"hello*world"));
            assert!(!glob_match(b"hello\\*world", b"helloXworld"));
        }

        #[test]
        fn golden_glob_match_bracket() {
            assert!(glob_match(b"h[ae]llo", b"hello"));
            assert!(glob_match(b"h[ae]llo", b"hallo"));
            assert!(!glob_match(b"h[ae]llo", b"hillo"));
        }

        #[test]
        fn golden_glob_match_bracket_range() {
            assert!(glob_match(b"key[0-9]", b"key5"));
            assert!(!glob_match(b"key[0-9]", b"keya"));
        }

        #[test]
        fn golden_glob_match_bracket_negation() {
            assert!(glob_match(b"h[^ae]llo", b"hillo"));
            assert!(!glob_match(b"h[^ae]llo", b"hello"));
        }

        #[test]
        fn golden_keyspace_events_canonical_form_matches_upstream() {
            // Upstream notify.c::keyspaceEventsFlagsToString emits the 'n'
            // bit only in the per-class else branch — once 'A' covers
            // every class flag, 'n' is dropped from the string form even
            // when NOTIFY_NEW was set. CONFIG GET reflects this lossy
            // canonicalization: SET KAn → GET AK.
            // (br-frankenredis-xmev)
            let flags = keyspace_events_parse("KAn").expect("valid notify-keyspace-events");
            assert_eq!(keyspace_events_to_string(flags), "AK");

            // n outside the A shorthand still survives in canonical form.
            let flags = keyspace_events_parse("Kgn").expect("valid notify-keyspace-events");
            assert_eq!(keyspace_events_to_string(flags), "gnK");
        }
    }

    // ── Metamorphic property tests ──────────────────────────────────────────
    mod metamorphic {
        use super::{
            function_library_snapshot, group_read_options, sample_function_library,
            sample_function_library_from_seed, sample_replacement_function_library_from_seed,
        };
        use crate::{
            Store, StoreError, StreamClaimOptions, StreamGroupReadCursor, StreamId, StreamRecord,
            decode_db_key, encode_db_key, eq_ascii_ci, glob_match, keyspace_events_parse,
            keyspace_events_to_string,
        };
        use proptest::prelude::*;
        use std::collections::{BTreeMap, BTreeSet};

        const METAMORPHIC_NOW_MS: u64 = 1_000;

        #[derive(Debug, Clone)]
        enum AofSeedValue {
            String(Vec<u8>),
            List(Vec<Vec<u8>>),
            Set(Vec<Vec<u8>>),
            Hash(Vec<(Vec<u8>, Vec<u8>)>),
            SortedSet(Vec<(Vec<u8>, i16)>),
            Stream {
                records: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
                bump_last_id: bool,
                group_seeds: Vec<u8>,
            },
        }

        #[derive(Debug, Clone)]
        struct AofSeedEntry {
            db: u8,
            ttl_ms: Option<u16>,
            value: AofSeedValue,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct AofStreamPendingSnapshot {
            id: StreamId,
            consumer: Vec<u8>,
            deliveries: u64,
            last_delivered_ms: u64,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct AofStreamGroupSnapshot {
            name: Vec<u8>,
            consumers: Vec<Vec<u8>>,
            pending: Vec<AofStreamPendingSnapshot>,
            last_delivered_id: (u64, u64),
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        enum AofValueSnapshot {
            String(Vec<u8>),
            List(Vec<Vec<u8>>),
            Set(Vec<Vec<u8>>),
            Hash(Vec<(Vec<u8>, Vec<u8>)>),
            SortedSet(Vec<(Vec<u8>, u64)>),
            Stream {
                entries: Vec<StreamRecord>,
                last_id: Option<(u64, u64)>,
                groups: Vec<AofStreamGroupSnapshot>,
            },
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct AofKeySnapshot {
            db: usize,
            key: Vec<u8>,
            expires_at_ms: Option<u64>,
            value: AofValueSnapshot,
        }

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct AofReplaySnapshot {
            function_libraries: Vec<(String, String, Vec<u8>, Vec<String>)>,
            keys: Vec<AofKeySnapshot>,
        }

        fn valid_keyspace_char() -> impl Strategy<Value = char> {
            prop_oneof![
                Just('A'),
                Just('g'),
                Just('$'),
                Just('l'),
                Just('s'),
                Just('h'),
                Just('z'),
                Just('x'),
                Just('e'),
                Just('K'),
                Just('E'),
                Just('t'),
                Just('m'),
                Just('n'),
            ]
        }

        fn event_only_keyspace_char() -> impl Strategy<Value = char> {
            prop_oneof![
                Just('A'),
                Just('g'),
                Just('$'),
                Just('l'),
                Just('s'),
                Just('h'),
                Just('z'),
                Just('x'),
                Just('e'),
                Just('t'),
                Just('m'),
                Just('n'),
            ]
        }

        fn small_key() -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 1..16)
        }

        fn small_blob() -> impl Strategy<Value = Vec<u8>> {
            prop::collection::vec(any::<u8>(), 0..32)
        }

        fn optional_ttl_ms() -> impl Strategy<Value = Option<u64>> {
            prop::option::of(1u16..512).prop_map(|ttl| ttl.map(u64::from))
        }

        fn optional_small_ttl_ms() -> impl Strategy<Value = Option<u16>> {
            prop::option::of(1u16..512)
        }

        fn normalized_blob(mut bytes: Vec<u8>, fallback: &[u8]) -> Vec<u8> {
            bytes.truncate(32);
            if bytes.is_empty() {
                fallback.to_vec()
            } else {
                bytes
            }
        }

        fn normalized_list(mut values: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
            values.truncate(8);
            let mut values: Vec<Vec<u8>> = values
                .into_iter()
                .map(|value| normalized_blob(value, b"item"))
                .collect();
            if values.is_empty() {
                values.push(b"item".to_vec());
            }
            values
        }

        fn normalized_set_members(mut members: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
            members.truncate(8);
            let mut members = BTreeSet::from_iter(
                members
                    .into_iter()
                    .map(|member| normalized_blob(member, b"member")),
            );
            if members.is_empty() {
                members.insert(b"member".to_vec());
            }
            members.into_iter().collect()
        }

        fn normalized_hash_entries(
            mut entries: Vec<(Vec<u8>, Vec<u8>)>,
        ) -> Vec<(Vec<u8>, Vec<u8>)> {
            entries.truncate(8);
            let mut map = BTreeMap::new();
            for (field, value) in entries {
                let field = normalized_blob(field, b"field");
                let value = normalized_blob(value, b"value");
                map.insert(field, value);
            }
            if map.is_empty() {
                map.insert(b"field".to_vec(), b"value".to_vec());
            }
            map.into_iter().collect()
        }

        fn normalized_sorted_set_entries(mut entries: Vec<(Vec<u8>, i16)>) -> Vec<(f64, Vec<u8>)> {
            entries.truncate(8);
            let mut map = BTreeMap::new();
            for (member, score) in entries {
                let member = normalized_blob(member, b"member");
                map.insert(member, f64::from(score) / 4.0);
            }
            if map.is_empty() {
                map.insert(b"member".to_vec(), 1.25);
            }
            map.into_iter()
                .map(|(member, score)| (score, member))
                .collect()
        }

        fn normalized_stream_records(
            mut records: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
        ) -> Vec<StreamRecord> {
            records.truncate(6);
            if records.is_empty() {
                records.push(vec![(b"field".to_vec(), b"value".to_vec())]);
            }
            records
                .into_iter()
                .enumerate()
                .map(|(idx, fields)| {
                    let id = (1_000 + (idx as u64 / 2), idx as u64 % 2);
                    let normalized_fields = fields
                        .into_iter()
                        .take(4)
                        .map(|(field, value)| {
                            (
                                normalized_blob(field, b"field"),
                                normalized_blob(value, b"value"),
                            )
                        })
                        .collect();
                    (id, normalized_fields)
                })
                .collect()
        }

        fn normalized_function_library_payloads(mut seeds: Vec<u16>) -> Vec<Vec<u8>> {
            seeds.truncate(4);
            let mut unique_seeds = BTreeSet::from_iter(seeds);
            if unique_seeds.is_empty() {
                unique_seeds.insert(0);
            }
            unique_seeds
                .into_iter()
                .map(sample_function_library_from_seed)
                .collect()
        }

        fn optional_function_library_payloads(mut seeds: Vec<u16>) -> Vec<Vec<u8>> {
            seeds.truncate(4);
            BTreeSet::from_iter(seeds)
                .into_iter()
                .map(sample_function_library_from_seed)
                .collect()
        }

        fn replacement_function_library_payloads(mut seeds: Vec<u16>) -> Vec<Vec<u8>> {
            seeds.truncate(4);
            let mut unique_seeds = BTreeSet::from_iter(seeds);
            if unique_seeds.is_empty() {
                unique_seeds.insert(0);
            }
            unique_seeds
                .into_iter()
                .map(sample_replacement_function_library_from_seed)
                .collect()
        }

        fn install_function_libraries(store: &mut Store, libraries: &[Vec<u8>]) {
            for library in libraries {
                store
                    .function_load(library, false)
                    .expect("generated function library must load");
            }
        }

        fn normalized_stream_group_seeds(mut seeds: Vec<u8>) -> Vec<u8> {
            seeds.truncate(3);
            BTreeSet::from_iter(seeds).into_iter().collect()
        }

        fn aof_seed_entry_strategy() -> impl Strategy<Value = AofSeedEntry> {
            prop_oneof![
                (0u8..3, optional_small_ttl_ms(), small_blob()).prop_map(|(db, ttl_ms, value)| {
                    AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::String(value),
                    }
                }),
                (
                    0u8..3,
                    optional_small_ttl_ms(),
                    prop::collection::vec(small_blob(), 0..6),
                )
                    .prop_map(|(db, ttl_ms, values)| AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::List(values),
                    }),
                (
                    0u8..3,
                    optional_small_ttl_ms(),
                    prop::collection::vec(small_blob(), 0..6),
                )
                    .prop_map(|(db, ttl_ms, members)| AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::Set(members),
                    }),
                (
                    0u8..3,
                    optional_small_ttl_ms(),
                    prop::collection::vec((small_blob(), small_blob()), 0..6),
                )
                    .prop_map(|(db, ttl_ms, entries)| AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::Hash(entries),
                    }),
                (
                    0u8..3,
                    optional_small_ttl_ms(),
                    prop::collection::vec((small_blob(), -256i16..256i16), 0..6),
                )
                    .prop_map(|(db, ttl_ms, entries)| AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::SortedSet(entries),
                    }),
                (
                    0u8..3,
                    optional_small_ttl_ms(),
                    prop::collection::vec(
                        prop::collection::vec((small_blob(), small_blob()), 1..5),
                        1..5,
                    ),
                    any::<bool>(),
                    prop::collection::vec(0u8..8, 0..4),
                )
                    .prop_map(
                        |(db, ttl_ms, records, bump_last_id, group_seeds)| {
                            AofSeedEntry {
                                db,
                                ttl_ms,
                                value: AofSeedValue::Stream {
                                    records,
                                    bump_last_id,
                                    group_seeds,
                                },
                            }
                        }
                    ),
            ]
        }

        fn logical_aof_key(index: usize, db: u8) -> Vec<u8> {
            format!("aof_{index:02x}_{db:02x}").into_bytes()
        }

        fn install_aof_seed_entries(store: &mut Store, entries: &[AofSeedEntry]) {
            for (index, entry) in entries.iter().take(6).enumerate() {
                let logical_key = logical_aof_key(index, entry.db);
                let physical_key = encode_db_key(entry.db as usize, &logical_key);

                match &entry.value {
                    AofSeedValue::String(value) => {
                        store.set(
                            physical_key.clone(),
                            normalized_blob(value.clone(), b"value"),
                            None,
                            METAMORPHIC_NOW_MS,
                        );
                    }
                    AofSeedValue::List(values) => {
                        let values = normalized_list(values.clone());
                        store
                            .rpush(&physical_key, &values, METAMORPHIC_NOW_MS)
                            .expect("generated list seed must install");
                    }
                    AofSeedValue::Set(members) => {
                        let members = normalized_set_members(members.clone());
                        store
                            .sadd(&physical_key, &members, METAMORPHIC_NOW_MS)
                            .expect("generated set seed must install");
                    }
                    AofSeedValue::Hash(entries) => {
                        for (field, value) in normalized_hash_entries(entries.clone()) {
                            store
                                .hset(&physical_key, field, value, METAMORPHIC_NOW_MS)
                                .expect("generated hash seed must install");
                        }
                    }
                    AofSeedValue::SortedSet(entries) => {
                        let entries = normalized_sorted_set_entries(entries.clone());
                        store
                            .zadd(&physical_key, &entries, METAMORPHIC_NOW_MS)
                            .expect("generated sorted-set seed must install");
                    }
                    AofSeedValue::Stream {
                        records,
                        bump_last_id,
                        group_seeds,
                    } => {
                        let records = normalized_stream_records(records.clone());
                        for (id, fields) in &records {
                            store
                                .xadd(&physical_key, *id, fields, METAMORPHIC_NOW_MS)
                                .expect("generated stream seed must install");
                        }

                        let max_entry_id = records.last().map(|(id, _)| *id).unwrap_or((0, 0));
                        let recorded_last_id = if *bump_last_id {
                            let bumped = (
                                max_entry_id.0.saturating_add(1),
                                max_entry_id.1.saturating_add(1),
                            );
                            store
                                .xsetid(&physical_key, bumped, METAMORPHIC_NOW_MS)
                                .expect("generated stream watermark must install");
                            Some(bumped)
                        } else {
                            None
                        };

                        let group_seeds = normalized_stream_group_seeds(group_seeds.clone());
                        for (group_idx, group_seed) in group_seeds.into_iter().enumerate() {
                            let group_name =
                                format!("grp_{index:02x}_{group_seed:02x}").into_bytes();
                            let last_delivered_id = if group_idx % 2 == 0 {
                                (0, 0)
                            } else {
                                recorded_last_id.unwrap_or(max_entry_id)
                            };
                            assert!(
                                store
                                    .xgroup_create(
                                        &physical_key,
                                        &group_name,
                                        last_delivered_id,
                                        false,
                                        METAMORPHIC_NOW_MS,
                                    )
                                    .expect("generated stream group must install"),
                                "generated stream group names must stay unique"
                            );
                        }
                    }
                }

                if let Some(ttl_ms) = entry.ttl_ms {
                    assert!(
                        store.expire_milliseconds(
                            &physical_key,
                            i64::from(ttl_ms),
                            METAMORPHIC_NOW_MS,
                        ),
                        "freshly installed AOF seed must accept ttl"
                    );
                }
            }
        }

        fn snapshot_aof_replay_state(store: &Store) -> AofReplaySnapshot {
            AofReplaySnapshot {
                function_libraries: function_library_snapshot(store),
                keys: store
                    .all_keys()
                    .into_iter()
                    .map(|physical_key| {
                        let (db, logical_key) =
                            decode_db_key(&physical_key).unwrap_or((0, physical_key.as_slice()));
                        let entry = store
                            .entries
                            .get(&physical_key)
                            .expect("all_keys entries must exist");
                        let value = match &entry.value {
                            crate::Value::String(bytes) => AofValueSnapshot::String(bytes.clone()),
                            crate::Value::List(list) => {
                                AofValueSnapshot::List(list.iter().cloned().collect())
                            }
                            crate::Value::Set(set) => {
                                AofValueSnapshot::Set(set.iter().cloned().collect())
                            }
                            crate::Value::Hash(hash) => AofValueSnapshot::Hash(
                                hash.iter()
                                    .map(|(field, value)| (field.clone(), value.clone()))
                                    .collect(),
                            ),
                            crate::Value::SortedSet(zs) => AofValueSnapshot::SortedSet(
                                zs.iter_asc()
                                    .map(|(member, score)| (member.clone(), score.to_bits()))
                                    .collect(),
                            ),
                            crate::Value::Stream(entries) => {
                                let groups = store
                                    .stream_groups
                                    .get(physical_key.as_slice())
                                    .map(|groups| {
                                        groups
                                            .iter()
                                            .map(|(name, group)| AofStreamGroupSnapshot {
                                                name: name.clone(),
                                                consumers: group
                                                    .consumers
                                                    .iter()
                                                    .cloned()
                                                    .collect(),
                                                pending: group
                                                    .pending
                                                    .iter()
                                                    .map(|(id, pending)| AofStreamPendingSnapshot {
                                                        id: *id,
                                                        consumer: pending.consumer.clone(),
                                                        deliveries: pending.deliveries,
                                                        last_delivered_ms: pending
                                                            .last_delivered_ms,
                                                    })
                                                    .collect(),
                                                last_delivered_id: group.last_delivered_id,
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                AofValueSnapshot::Stream {
                                    entries: entries
                                        .iter()
                                        .map(|(id, fields)| (*id, fields.clone()))
                                        .collect(),
                                    last_id: store
                                        .stream_last_ids
                                        .get(physical_key.as_slice())
                                        .copied(),
                                    groups,
                                }
                            }
                        };

                        AofKeySnapshot {
                            db,
                            key: logical_key.to_vec(),
                            expires_at_ms: entry.expires_at_ms,
                            value,
                        }
                    })
                    .collect(),
            }
        }

        fn parse_usize_arg(bytes: &[u8]) -> usize {
            String::from_utf8_lossy(bytes)
                .parse::<usize>()
                .expect("generated SELECT arg must stay numeric")
        }

        fn parse_i64_arg(bytes: &[u8]) -> i64 {
            String::from_utf8_lossy(bytes)
                .parse::<i64>()
                .expect("generated integer arg must stay numeric")
        }

        fn parse_u64_arg(bytes: &[u8]) -> u64 {
            String::from_utf8_lossy(bytes)
                .parse::<u64>()
                .expect("generated unsigned integer arg must stay numeric")
        }

        fn parse_f64_arg(bytes: &[u8]) -> f64 {
            String::from_utf8_lossy(bytes)
                .parse::<f64>()
                .expect("generated float arg must stay numeric")
        }

        fn parse_stream_id_arg(bytes: &[u8]) -> (u64, u64) {
            let id = String::from_utf8_lossy(bytes);
            let (ms, seq) = id
                .split_once('-')
                .expect("generated stream ids must contain a dash");
            (
                ms.parse::<u64>()
                    .expect("generated stream millisecond id must stay numeric"),
                seq.parse::<u64>()
                    .expect("generated stream sequence id must stay numeric"),
            )
        }

        fn replay_aof_commands(commands: &[Vec<Vec<u8>>]) -> Store {
            let mut store = Store::new();
            let mut current_db = 0usize;

            for argv in commands {
                let command = argv
                    .first()
                    .expect("AOF rewrite commands must contain a command name");
                if eq_ascii_ci(command, b"SELECT") {
                    current_db = parse_usize_arg(&argv[1]);
                    continue;
                }
                if eq_ascii_ci(command, b"FUNCTION") {
                    assert!(eq_ascii_ci(&argv[1], b"LOAD"));
                    let (replace, code_idx) =
                        if argv.get(2).is_some_and(|arg| eq_ascii_ci(arg, b"REPLACE")) {
                            (true, 3usize)
                        } else {
                            (false, 2usize)
                        };
                    store
                        .function_load(&argv[code_idx], replace)
                        .expect("FUNCTION LOAD replay must stay valid");
                    continue;
                }

                let key = encode_db_key(current_db, &argv[1]);

                if eq_ascii_ci(command, b"SET") {
                    store.set(key, argv[2].clone(), None, METAMORPHIC_NOW_MS);
                } else if eq_ascii_ci(command, b"PEXPIREAT") {
                    assert!(
                        store.expire_at_milliseconds(
                            &key,
                            parse_i64_arg(&argv[2]),
                            METAMORPHIC_NOW_MS,
                        ),
                        "PEXPIREAT replay must target an existing key"
                    );
                } else if eq_ascii_ci(command, b"HSET") {
                    for pair in argv[2..].chunks_exact(2) {
                        store
                            .hset(&key, pair[0].clone(), pair[1].clone(), METAMORPHIC_NOW_MS)
                            .expect("HSET replay must stay valid");
                    }
                } else if eq_ascii_ci(command, b"RPUSH") {
                    store
                        .rpush(&key, &argv[2..], METAMORPHIC_NOW_MS)
                        .expect("RPUSH replay must stay valid");
                } else if eq_ascii_ci(command, b"SADD") {
                    store
                        .sadd(&key, &argv[2..], METAMORPHIC_NOW_MS)
                        .expect("SADD replay must stay valid");
                } else if eq_ascii_ci(command, b"ZADD") {
                    let entries: Vec<(f64, Vec<u8>)> = argv[2..]
                        .chunks_exact(2)
                        .map(|pair| (parse_f64_arg(&pair[0]), pair[1].clone()))
                        .collect();
                    store
                        .zadd(&key, &entries, METAMORPHIC_NOW_MS)
                        .expect("ZADD replay must stay valid");
                } else if eq_ascii_ci(command, b"XADD") {
                    let id = parse_stream_id_arg(&argv[2]);
                    let fields: Vec<(Vec<u8>, Vec<u8>)> = argv[3..]
                        .chunks_exact(2)
                        .map(|pair| (pair[0].clone(), pair[1].clone()))
                        .collect();
                    store
                        .xadd(&key, id, &fields, METAMORPHIC_NOW_MS)
                        .expect("XADD replay must stay valid");
                } else if eq_ascii_ci(command, b"XSETID") {
                    store
                        .xsetid(&key, parse_stream_id_arg(&argv[2]), METAMORPHIC_NOW_MS)
                        .expect("XSETID replay must stay valid");
                } else if eq_ascii_ci(command, b"XGROUP") {
                    if eq_ascii_ci(&argv[1], b"CREATE") {
                        assert!(
                            store
                                .xgroup_create(
                                    &encode_db_key(current_db, &argv[2]),
                                    &argv[3],
                                    parse_stream_id_arg(&argv[4]),
                                    false,
                                    METAMORPHIC_NOW_MS,
                                )
                                .expect("XGROUP CREATE replay must stay valid"),
                            "AOF rewrite group creation must stay unique during replay"
                        );
                    } else if eq_ascii_ci(&argv[1], b"CREATECONSUMER") {
                        assert!(
                            store
                                .xgroup_createconsumer(
                                    &encode_db_key(current_db, &argv[2]),
                                    &argv[3],
                                    &argv[4],
                                    METAMORPHIC_NOW_MS,
                                )
                                .expect("XGROUP CREATECONSUMER replay must stay valid")
                                .expect("generated stream group must exist for consumer replay"),
                            "AOF rewrite consumer creation must stay unique during replay"
                        );
                    } else {
                        assert!(argv.is_empty(), "unexpected AOF rewrite command: {argv:?}");
                    }
                } else if eq_ascii_ci(command, b"XCLAIM") {
                    let mut ids = Vec::new();
                    let mut idx = 5usize;
                    while idx < argv.len() {
                        if eq_ascii_ci(&argv[idx], b"IDLE")
                            || eq_ascii_ci(&argv[idx], b"TIME")
                            || eq_ascii_ci(&argv[idx], b"RETRYCOUNT")
                            || eq_ascii_ci(&argv[idx], b"FORCE")
                            || eq_ascii_ci(&argv[idx], b"JUSTID")
                            || eq_ascii_ci(&argv[idx], b"LASTID")
                        {
                            break;
                        }
                        ids.push(parse_stream_id_arg(&argv[idx]));
                        idx += 1;
                    }

                    let mut options = StreamClaimOptions {
                        min_idle_time_ms: parse_u64_arg(&argv[4]),
                        idle_ms: None,
                        time_ms: None,
                        retry_count: None,
                        force: false,
                        justid: false,
                        last_id: None,
                    };

                    while idx < argv.len() {
                        if eq_ascii_ci(&argv[idx], b"IDLE") {
                            options.idle_ms = Some(parse_u64_arg(&argv[idx + 1]));
                            idx += 2;
                        } else if eq_ascii_ci(&argv[idx], b"TIME") {
                            options.time_ms = Some(parse_u64_arg(&argv[idx + 1]));
                            idx += 2;
                        } else if eq_ascii_ci(&argv[idx], b"RETRYCOUNT") {
                            options.retry_count = Some(parse_u64_arg(&argv[idx + 1]));
                            idx += 2;
                        } else if eq_ascii_ci(&argv[idx], b"FORCE") {
                            options.force = true;
                            idx += 1;
                        } else if eq_ascii_ci(&argv[idx], b"JUSTID") {
                            options.justid = true;
                            idx += 1;
                        } else if eq_ascii_ci(&argv[idx], b"LASTID") {
                            options.last_id = Some(parse_stream_id_arg(&argv[idx + 1]));
                            idx += 2;
                        } else {
                            assert!(argv.is_empty(), "unexpected AOF rewrite command: {argv:?}");
                        }
                    }

                    store
                        .xclaim(&key, &argv[2], &argv[3], &ids, options, METAMORPHIC_NOW_MS)
                        .expect("XCLAIM replay must stay valid")
                        .expect("generated XCLAIM replay must target an existing stream group");
                } else {
                    assert!(argv.is_empty(), "unexpected AOF rewrite command: {argv:?}");
                }
            }

            store
        }

        #[test]
        fn aof_rewrite_replay_preserves_function_libraries_and_multidb_expiries() {
            let mut original = Store::new();
            install_function_libraries(
                &mut original,
                &[
                    sample_function_library_from_seed(7),
                    sample_function_library_from_seed(3),
                ],
            );
            original.set(
                b"a0".to_vec(),
                b"va0".to_vec(),
                Some(7000),
                METAMORPHIC_NOW_MS,
            );
            original.set(
                encode_db_key(1, b"b1"),
                b"vb1".to_vec(),
                Some(6000),
                METAMORPHIC_NOW_MS,
            );

            let commands = original.to_aof_commands(METAMORPHIC_NOW_MS);
            let expected_snapshot = snapshot_aof_replay_state(&original);

            let mut replayed = replay_aof_commands(&commands);
            assert_eq!(snapshot_aof_replay_state(&replayed), expected_snapshot);
            assert_eq!(replayed.to_aof_commands(METAMORPHIC_NOW_MS), commands);
        }

        #[test]
        fn aof_rewrite_replay_preserves_function_libraries_and_noncontiguous_db_selects() {
            let alpha = sample_function_library("alpha", "afn1", "afn2");
            let beta = sample_function_library("beta", "bfn1", "bfn2");
            let mut original = Store::new();
            original
                .function_load(&beta, false)
                .expect("beta library must load");
            original
                .function_load(&alpha, false)
                .expect("alpha library must load");
            original.set(
                b"a0".to_vec(),
                b"va0".to_vec(),
                Some(7000),
                METAMORPHIC_NOW_MS,
            );
            original.set(
                encode_db_key(2, b"c2"),
                b"vc2".to_vec(),
                None,
                METAMORPHIC_NOW_MS,
            );
            original.set(
                encode_db_key(2, b"b2"),
                b"vb2".to_vec(),
                Some(6500),
                METAMORPHIC_NOW_MS,
            );

            let commands = original.to_aof_commands(METAMORPHIC_NOW_MS);
            assert_eq!(
                commands,
                vec![
                    vec![
                        b"FUNCTION".to_vec(),
                        b"LOAD".to_vec(),
                        b"REPLACE".to_vec(),
                        alpha,
                    ],
                    vec![
                        b"FUNCTION".to_vec(),
                        b"LOAD".to_vec(),
                        b"REPLACE".to_vec(),
                        beta,
                    ],
                    vec![b"SET".to_vec(), b"a0".to_vec(), b"va0".to_vec()],
                    vec![b"PEXPIREAT".to_vec(), b"a0".to_vec(), b"8000".to_vec()],
                    vec![b"SELECT".to_vec(), b"2".to_vec()],
                    vec![b"SET".to_vec(), b"b2".to_vec(), b"vb2".to_vec()],
                    vec![b"PEXPIREAT".to_vec(), b"b2".to_vec(), b"7500".to_vec()],
                    vec![b"SET".to_vec(), b"c2".to_vec(), b"vc2".to_vec()],
                ]
            );
            assert!(
                !commands
                    .iter()
                    .any(|argv| argv.len() == 2 && argv[0] == b"SELECT" && argv[1] == b"0"),
                "DB 0 must remain implicit across noncontiguous DB replay output"
            );
            assert!(
                !commands
                    .iter()
                    .any(|argv| argv.len() == 2 && argv[0] == b"SELECT" && argv[1] == b"1"),
                "noncontiguous DB replay output must not synthesize missing DB 1 selects"
            );

            let expected_snapshot = snapshot_aof_replay_state(&original);
            let mut replayed = replay_aof_commands(&commands);
            assert_eq!(snapshot_aof_replay_state(&replayed), expected_snapshot);
            assert_eq!(replayed.to_aof_commands(METAMORPHIC_NOW_MS), commands);
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            #[test]
            fn mr_encode_decode_roundtrip(db in 1usize..16, key in prop::collection::vec(any::<u8>(), 0..64)) {
                let encoded = encode_db_key(db, &key);
                let (decoded_db, decoded_key) = decode_db_key(&encoded).expect("roundtrip must decode");
                prop_assert_eq!(decoded_db, db);
                prop_assert_eq!(decoded_key, &key[..]);
            }

            #[test]
            fn mr_db0_is_identity(key in prop::collection::vec(any::<u8>(), 0..64)) {
                let encoded = encode_db_key(0, &key);
                prop_assert_eq!(encoded, key, "DB 0 encoding must be identity");
            }

            #[test]
            fn mr_glob_star_matches_all(s in prop::collection::vec(any::<u8>(), 0..32)) {
                prop_assert!(glob_match(b"*", &s), "* must match any string");
            }

            #[test]
            fn mr_glob_empty_pattern_matches_empty_string(s in prop::collection::vec(any::<u8>(), 0..32)) {
                let matches_empty = glob_match(b"", &s);
                prop_assert_eq!(matches_empty, s.is_empty(), "Empty pattern matches only empty string");
            }

            #[test]
            fn mr_glob_exact_match_reflexive(s in prop::collection::vec(any::<u8>().prop_filter("no special chars", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')), 1..16)) {
                prop_assert!(glob_match(&s, &s), "Pattern must match itself when no special chars");
            }

            #[test]
            fn mr_glob_prefix_star_always_matches_prefix(
                prefix in prop::collection::vec(any::<u8>().prop_filter("no special", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')), 1..8),
                suffix in prop::collection::vec(any::<u8>(), 0..16)
            ) {
                let mut pattern = prefix.clone();
                pattern.push(b'*');

                let mut string = prefix.clone();
                string.extend_from_slice(&suffix);

                prop_assert!(glob_match(&pattern, &string), "prefix* must match prefix+anything");
            }

            #[test]
            fn mr_glob_star_suffix_matches_ending(
                prefix in prop::collection::vec(any::<u8>(), 0..16),
                suffix in prop::collection::vec(any::<u8>().prop_filter("no special", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')), 1..8)
            ) {
                let mut pattern = vec![b'*'];
                pattern.extend_from_slice(&suffix);

                let mut string = prefix;
                string.extend_from_slice(&suffix);

                prop_assert!(glob_match(&pattern, &string), "*suffix must match anything+suffix");
            }

            #[test]
            fn mr_glob_question_requires_one_char(
                prefix in prop::collection::vec(any::<u8>().prop_filter("no special", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')), 0..4),
                middle in any::<u8>().prop_filter("no special", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')),
                suffix in prop::collection::vec(any::<u8>().prop_filter("no special", |b| !matches!(*b, b'*' | b'?' | b'[' | b']' | b'\\')), 0..4)
            ) {
                let mut pattern = prefix.clone();
                pattern.push(b'?');
                pattern.extend_from_slice(&suffix);

                let mut string = prefix;
                string.push(middle);
                string.extend_from_slice(&suffix);

                prop_assert!(glob_match(&pattern, &string), "? must match single char");
            }

            #[test]
            fn mr_keyspace_events_order_invariant(classes in prop::collection::vec(valid_keyspace_char(), 0..24)) {
                let original: String = classes.iter().copied().collect();
                let reversed: String = classes.iter().rev().copied().collect();

                prop_assert_eq!(
                    keyspace_events_parse(&original),
                    keyspace_events_parse(&reversed),
                    "notify-keyspace-events parsing should be invariant to token order",
                );
            }

            #[test]
            fn mr_keyspace_events_duplicate_tokens_idempotent(
                prefix in prop::collection::vec(valid_keyspace_char(), 0..16),
                duplicated in valid_keyspace_char(),
                suffix in prop::collection::vec(valid_keyspace_char(), 0..16)
            ) {
                let mut original_chars = prefix.clone();
                original_chars.push(duplicated);
                original_chars.extend_from_slice(&suffix);
                let original: String = original_chars.iter().copied().collect();

                let mut duplicated_chars = prefix;
                duplicated_chars.push(duplicated);
                duplicated_chars.push(duplicated);
                duplicated_chars.extend_from_slice(&suffix);
                let duplicated_string: String = duplicated_chars.iter().copied().collect();

                prop_assert_eq!(
                    keyspace_events_parse(&original),
                    keyspace_events_parse(&duplicated_string),
                    "duplicating a notify-keyspace-events token should not change parsed flags",
                );
            }

            #[test]
            fn mr_keyspace_events_k_or_e_free_classes_collapse_to_zero(
                classes in prop::collection::vec(event_only_keyspace_char(), 1..24)
            ) {
                let input: String = classes.iter().copied().collect();
                let reversed: String = classes.iter().rev().copied().collect();

                prop_assert_eq!(keyspace_events_parse(&input), Some(0));
                prop_assert_eq!(keyspace_events_parse(&reversed), Some(0));
            }

            #[test]
            fn mr_keyspace_events_canonicalization_is_stable(classes in prop::collection::vec(valid_keyspace_char(), 0..24)) {
                let input: String = classes.iter().copied().collect();
                let flags = keyspace_events_parse(&input).expect("generated classes must stay valid");
                let canonical = keyspace_events_to_string(flags);
                let reparsed = keyspace_events_parse(&canonical).expect("canonical form must parse");
                let recanonical = keyspace_events_to_string(reparsed);

                // Upstream canonicalization is intentionally lossy when the
                // 'A' shorthand subsumes per-class flags: e.g. flags
                // NOTIFY_ALL|NOTIFY_NEW|NOTIFY_KEYSPACE round-trip via string
                // to NOTIFY_ALL|NOTIFY_KEYSPACE (the 'n' is dropped because
                // upstream emits it only outside the A-shorthand branch).
                // The stable invariant is that two consecutive canonicalizations
                // converge — not that the first one preserves all bits.
                // (br-frankenredis-xmev)
                prop_assert_eq!(recanonical, canonical);
            }

            #[test]
            fn mr_dump_restore_string_payload_roundtrip_is_stable(
                key in small_key(),
                value in small_blob(),
                ttl_ms in optional_ttl_ms(),
            ) {
                let value = normalized_blob(value, b"value");
                let mut original = Store::new();
                original.set(key.clone(), value.clone(), ttl_ms, METAMORPHIC_NOW_MS);

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed string key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated string dump must restore");
                prop_assert_eq!(
                    restored.get(&key, METAMORPHIC_NOW_MS).unwrap(),
                    Some(value.clone()),
                );

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored string key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_list_payload_roundtrip_is_stable(
                key in small_key(),
                values in prop::collection::vec(small_blob(), 0..8),
                ttl_ms in optional_ttl_ms(),
            ) {
                let values = normalized_list(values);
                let mut original = Store::new();
                original
                    .rpush(&key, &values, METAMORPHIC_NOW_MS)
                    .expect("valid fuzz list setup must succeed");
                if let Some(ttl_ms) = ttl_ms {
                    prop_assert!(
                        original.expire_milliseconds(&key, ttl_ms as i64, METAMORPHIC_NOW_MS),
                        "freshly installed list must accept ttl"
                    );
                }

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed list key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated list dump must restore");
                prop_assert_eq!(restored.lrange(&key, 0, -1, METAMORPHIC_NOW_MS).unwrap(), values);

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored list key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_set_payload_roundtrip_is_stable(
                key in small_key(),
                members in prop::collection::vec(small_blob(), 0..8),
                ttl_ms in optional_ttl_ms(),
            ) {
                let members = normalized_set_members(members);
                let mut original = Store::new();
                original
                    .sadd(&key, &members, METAMORPHIC_NOW_MS)
                    .expect("valid fuzz set setup must succeed");
                if let Some(ttl_ms) = ttl_ms {
                    prop_assert!(
                        original.expire_milliseconds(&key, ttl_ms as i64, METAMORPHIC_NOW_MS),
                        "freshly installed set must accept ttl"
                    );
                }

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed set key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated set dump must restore");
                let restored_members =
                    BTreeSet::from_iter(restored.smembers(&key, METAMORPHIC_NOW_MS).unwrap());
                let expected_members = BTreeSet::from_iter(members.iter().cloned());
                prop_assert_eq!(restored_members, expected_members);

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored set key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_hash_payload_roundtrip_is_stable(
                key in small_key(),
                entries in prop::collection::vec((small_blob(), small_blob()), 0..8),
                ttl_ms in optional_ttl_ms(),
            ) {
                let entries = normalized_hash_entries(entries);
                let mut original = Store::new();
                for (field, value) in &entries {
                    original
                        .hset(&key, field.clone(), value.clone(), METAMORPHIC_NOW_MS)
                        .expect("valid fuzz hash setup must succeed");
                }
                if let Some(ttl_ms) = ttl_ms {
                    prop_assert!(
                        original.expire_milliseconds(&key, ttl_ms as i64, METAMORPHIC_NOW_MS),
                        "freshly installed hash must accept ttl"
                    );
                }

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed hash key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated hash dump must restore");
                prop_assert_eq!(restored.hgetall(&key, METAMORPHIC_NOW_MS).unwrap(), entries);

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored hash key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_sorted_set_payload_roundtrip_is_stable(
                key in small_key(),
                entries in prop::collection::vec((small_blob(), -256i16..256i16), 0..8),
                ttl_ms in optional_ttl_ms(),
            ) {
                let entries = normalized_sorted_set_entries(entries);
                let mut original = Store::new();
                original
                    .zadd(&key, &entries, METAMORPHIC_NOW_MS)
                    .expect("valid fuzz sorted-set setup must succeed");
                if let Some(ttl_ms) = ttl_ms {
                    prop_assert!(
                        original.expire_milliseconds(&key, ttl_ms as i64, METAMORPHIC_NOW_MS),
                        "freshly installed sorted set must accept ttl"
                    );
                }
                let expected = original
                    .zget_members_with_scores(&key, METAMORPHIC_NOW_MS)
                    .expect("installed sorted set must enumerate");

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed sorted-set key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated sorted-set dump must restore");
                prop_assert_eq!(
                    restored
                        .zget_members_with_scores(&key, METAMORPHIC_NOW_MS)
                        .expect("restored sorted set must enumerate"),
                    expected
                );

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored sorted-set key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_stream_payload_roundtrip_is_stable(
                key in small_key(),
                records in prop::collection::vec(
                    prop::collection::vec((small_blob(), small_blob()), 1..5),
                    1..6
                ),
                ttl_ms in optional_ttl_ms(),
            ) {
                let records = normalized_stream_records(records);
                let mut original = Store::new();
                for (id, fields) in &records {
                    original
                        .xadd(&key, *id, fields, METAMORPHIC_NOW_MS)
                        .expect("valid fuzz stream setup must succeed");
                }
                if let Some(ttl_ms) = ttl_ms {
                    prop_assert!(
                        original.expire_milliseconds(&key, ttl_ms as i64, METAMORPHIC_NOW_MS),
                        "freshly installed stream must accept ttl"
                    );
                }
                let expected = original
                    .xrange(&key, (0, 0), (u64::MAX, u64::MAX), None, METAMORPHIC_NOW_MS)
                    .expect("installed stream must enumerate");

                let payload = original
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("installed stream key must dump");
                let restore_ttl_ms = ttl_ms.unwrap_or(0);

                let mut restored = Store::new();
                restored
                    .restore_key(&key, restore_ttl_ms, &payload, false, METAMORPHIC_NOW_MS)
                    .expect("self-generated stream dump must restore");
                prop_assert_eq!(
                    restored
                        .xrange(&key, (0, 0), (u64::MAX, u64::MAX), None, METAMORPHIC_NOW_MS)
                        .expect("restored stream must enumerate"),
                    expected
                );

                let reencoded = restored
                    .dump_key(&key, METAMORPHIC_NOW_MS)
                    .expect("restored stream key must dump");
                prop_assert_eq!(payload, reencoded);
            }

            #[test]
            fn mr_dump_restore_busykey_without_replace_is_atomic(
                source_key in small_key(),
                target_key in small_key(),
                value in small_blob(),
                sentinel in small_blob(),
            ) {
                let value = normalized_blob(value, b"value");
                let sentinel = normalized_blob(sentinel, b"sentinel");

                let mut source = Store::new();
                source.set(source_key.clone(), value, None, METAMORPHIC_NOW_MS);
                let payload = source
                    .dump_key(&source_key, METAMORPHIC_NOW_MS)
                    .expect("installed source key must dump");

                let mut target = Store::new();
                target.set(target_key.clone(), sentinel, None, METAMORPHIC_NOW_MS);
                let before = target
                    .dump_key(&target_key, METAMORPHIC_NOW_MS)
                    .expect("occupied key must dump");

                let result =
                    target.restore_key(&target_key, 0, &payload, false, METAMORPHIC_NOW_MS);
                prop_assert_eq!(result, Err(StoreError::BusyKey));

                let after = target
                    .dump_key(&target_key, METAMORPHIC_NOW_MS)
                    .expect("busy-key failure must preserve original value");
                prop_assert_eq!(before, after);
            }

            #[test]
            fn mr_function_dump_restore_snapshot_roundtrip_is_stable(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let mut original = Store::new();
                install_function_libraries(&mut original, &libraries);

                let expected_snapshot = function_library_snapshot(&original);
                let dumped = original.function_dump();

                let mut restored = Store::new();
                restored
                    .function_restore(&dumped, "REPLACE")
                    .expect("self-generated FUNCTION DUMP payload must restore");

                prop_assert_eq!(function_library_snapshot(&restored), expected_snapshot);
                prop_assert_eq!(restored.function_dump(), dumped);
            }

            #[test]
            fn mr_function_restore_invalid_dump_is_atomic_against_generated_snapshot(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let mut store = Store::new();
                install_function_libraries(&mut store, &libraries);

                let before_snapshot = function_library_snapshot(&store);
                let before_dump = store.function_dump();
                let invalid_dump = before_dump[..before_dump.len() - 1].to_vec();

                let err = store
                    .function_restore(&invalid_dump, "FLUSH")
                    .expect_err("truncated FUNCTION DUMP payload must fail");
                // Truncating either (a) takes the buffer below the
                // 10-byte footer minimum and trips the length check
                // ("Invalid dump data"), or (b) corrupts the trailing
                // CRC64 footer ("DUMP payload version or checksum are
                // wrong"). Both come from the new envelope gate
                // landed under br-frankenredis-r83v and are valid
                // upstream behaviours.
                let msg = match err {
                    StoreError::GenericError(ref s) => s.clone(),
                    other => panic!("unexpected error: {other:?}"),
                };
                prop_assert!(
                    msg == "ERR Invalid dump data"
                        || msg == "ERR DUMP payload version or checksum are wrong",
                    "unexpected error wording: {msg}"
                );
                prop_assert_eq!(function_library_snapshot(&store), before_snapshot);
                prop_assert_eq!(store.function_dump(), before_dump);
            }

            #[test]
            fn mr_function_restore_append_of_disjoint_payload_is_union(
                base_seeds in prop::collection::vec(0u16..2048, 0..6),
                append_seeds in prop::collection::vec(2048u16..4096, 0..6),
            ) {
                let base_libraries = normalized_function_library_payloads(base_seeds);
                let append_libraries = optional_function_library_payloads(append_seeds);

                let mut payload_store = Store::new();
                install_function_libraries(&mut payload_store, &append_libraries);
                let payload_dump = payload_store.function_dump();

                let mut restored = Store::new();
                install_function_libraries(&mut restored, &base_libraries);
                restored
                    .function_restore(&payload_dump, "APPEND")
                    .expect("APPEND with disjoint libraries must succeed");

                let mut expected = Store::new();
                install_function_libraries(&mut expected, &base_libraries);
                install_function_libraries(&mut expected, &append_libraries);

                prop_assert_eq!(
                    function_library_snapshot(&restored),
                    function_library_snapshot(&expected)
                );
                prop_assert_eq!(restored.function_dump(), expected.function_dump());
            }

            #[test]
            fn mr_function_restore_append_collision_is_atomic(
                base_seeds in prop::collection::vec(0u16..2048, 0..6),
                append_extra_seeds in prop::collection::vec(2048u16..4096, 0..6),
            ) {
                let base_libraries = normalized_function_library_payloads(base_seeds.clone());
                let colliding_libraries = replacement_function_library_payloads(base_seeds);
                let append_extras = optional_function_library_payloads(append_extra_seeds);

                let mut payload_store = Store::new();
                install_function_libraries(&mut payload_store, &colliding_libraries);
                install_function_libraries(&mut payload_store, &append_extras);
                let payload_dump = payload_store.function_dump();

                let mut restored = Store::new();
                install_function_libraries(&mut restored, &base_libraries);
                let before_snapshot = function_library_snapshot(&restored);
                let before_dump = restored.function_dump();

                let err = restored
                    .function_restore(&payload_dump, "APPEND")
                    .expect_err("APPEND with colliding libraries must fail atomically");
                prop_assert!(
                    matches!(err, StoreError::GenericError(message) if message.contains("already exists"))
                );
                prop_assert_eq!(function_library_snapshot(&restored), before_snapshot);
                prop_assert_eq!(restored.function_dump(), before_dump);
            }

            #[test]
            fn mr_function_restore_replace_overwrites_collisions_and_keeps_disjoint_existing(
                base_seeds in prop::collection::vec(0u16..2048, 0..6),
                stale_seeds in prop::collection::vec(2048u16..4096, 0..6),
            ) {
                let original_libraries = normalized_function_library_payloads(base_seeds.clone());
                let replacement_libraries = replacement_function_library_payloads(base_seeds);
                let stale_libraries = optional_function_library_payloads(stale_seeds);

                let mut payload_store = Store::new();
                install_function_libraries(&mut payload_store, &replacement_libraries);
                let payload_dump = payload_store.function_dump();

                let mut restored = Store::new();
                install_function_libraries(&mut restored, &original_libraries);
                install_function_libraries(&mut restored, &stale_libraries);
                restored
                    .function_restore(&payload_dump, "REPLACE")
                    .expect("REPLACE must overwrite colliding libraries");

                let mut expected = Store::new();
                install_function_libraries(&mut expected, &replacement_libraries);
                install_function_libraries(&mut expected, &stale_libraries);

                prop_assert_eq!(
                    function_library_snapshot(&restored),
                    function_library_snapshot(&expected)
                );
                prop_assert_eq!(restored.function_dump(), expected.function_dump());
            }

            #[test]
            fn mr_function_restore_empty_dump_append_is_identity(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let empty_dump = Store::new().function_dump();
                let mut restored = Store::new();
                install_function_libraries(&mut restored, &libraries);
                let before_snapshot = function_library_snapshot(&restored);
                let before_dump = restored.function_dump();

                restored
                    .function_restore(&empty_dump, "APPEND")
                    .expect("APPEND with empty FUNCTION DUMP must be a no-op");

                prop_assert_eq!(function_library_snapshot(&restored), before_snapshot);
                prop_assert_eq!(restored.function_dump(), before_dump);
            }

            #[test]
            fn mr_function_restore_empty_policy_empty_dump_is_identity(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let empty_dump = Store::new().function_dump();
                let mut restored = Store::new();
                install_function_libraries(&mut restored, &libraries);
                let before_snapshot = function_library_snapshot(&restored);
                let before_dump = restored.function_dump();

                restored
                    .function_restore(&empty_dump, "")
                    .expect("empty policy with empty FUNCTION DUMP must be a no-op");

                prop_assert_eq!(function_library_snapshot(&restored), before_snapshot);
                prop_assert_eq!(restored.function_dump(), before_dump);
            }

            #[test]
            fn mr_function_restore_empty_dump_replace_is_identity(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let empty_dump = Store::new().function_dump();
                let mut restored = Store::new();
                install_function_libraries(&mut restored, &libraries);
                let before_snapshot = function_library_snapshot(&restored);
                let before_dump = restored.function_dump();

                restored
                    .function_restore(&empty_dump, "REPLACE")
                    .expect("REPLACE with empty FUNCTION DUMP must preserve libraries");

                prop_assert_eq!(function_library_snapshot(&restored), before_snapshot);
                prop_assert_eq!(restored.function_dump(), before_dump);
            }

            #[test]
            fn mr_function_restore_empty_dump_flush_clears_snapshot(
                seeds in prop::collection::vec(0u16..4096, 0..6),
            ) {
                let libraries = normalized_function_library_payloads(seeds);
                let empty_dump = Store::new().function_dump();
                let mut restored = Store::new();
                install_function_libraries(&mut restored, &libraries);

                restored
                    .function_restore(&empty_dump, "FLUSH")
                    .expect("FLUSH with empty FUNCTION DUMP must clear libraries");

                prop_assert!(function_library_snapshot(&restored).is_empty());
                prop_assert_eq!(restored.function_dump(), empty_dump);
            }

            #[test]
            fn mr_aof_rewrite_replay_roundtrip_is_stable(
                entries in prop::collection::vec(aof_seed_entry_strategy(), 0..6),
                function_seeds in prop::collection::vec(any::<u16>(), 0..4),
            ) {
                let mut original = Store::new();
                install_aof_seed_entries(&mut original, &entries);
                let function_libraries = optional_function_library_payloads(function_seeds);
                install_function_libraries(&mut original, &function_libraries);

                let commands = original.to_aof_commands(METAMORPHIC_NOW_MS);
                let expected_snapshot = snapshot_aof_replay_state(&original);

                let mut replayed = replay_aof_commands(&commands);
                prop_assert_eq!(snapshot_aof_replay_state(&replayed), expected_snapshot);
                prop_assert_eq!(replayed.to_aof_commands(METAMORPHIC_NOW_MS), commands);
            }

            #[test]
            fn mr_aof_rewrite_stream_metadata_roundtrip_is_stable(
                db in 0u8..3,
                ttl_ms in optional_small_ttl_ms(),
                records in prop::collection::vec(
                    prop::collection::vec((small_blob(), small_blob()), 1..5),
                    1..5,
                ),
                bump_last_id in any::<bool>(),
                group_seeds in prop::collection::vec(0u8..8, 1..4),
                idle_consumer_seed in 0u8..8,
                claim_consumer_seed in 0u8..8,
                retry_count in 1u8..8,
                delivery_time_ms in 1u16..512,
            ) {
                let mut original = Store::new();
                let original_group_seeds = group_seeds.clone();
                install_aof_seed_entries(
                    &mut original,
                    &[AofSeedEntry {
                        db,
                        ttl_ms,
                        value: AofSeedValue::Stream {
                            records,
                            bump_last_id,
                            group_seeds,
                        },
                    }],
                );

                let normalized_group_seeds = normalized_stream_group_seeds(original_group_seeds);
                let logical_key = logical_aof_key(0, db);
                let physical_key = encode_db_key(db as usize, &logical_key);
                let first_group_name =
                    format!("grp_{:02x}_{:02x}", 0, normalized_group_seeds[0]).into_bytes();
                let idle_consumer = format!("idle_{idle_consumer_seed:02x}").into_bytes();
                let claim_consumer = format!("claim_{claim_consumer_seed:02x}").into_bytes();
                let pending_seed_consumer = b"reader".to_vec();
                let pending_entry_id = original
                    .xrange(
                        &physical_key,
                        (0, 0),
                        (u64::MAX, u64::MAX),
                        Some(1),
                        METAMORPHIC_NOW_MS,
                    )
                    .expect("generated stream must support XRANGE")
                    .first()
                    .map(|(id, _)| *id)
                    .expect("generated stream must contain at least one record");
                let _ = original
                    .xgroup_createconsumer(
                        &physical_key,
                        &first_group_name,
                        &idle_consumer,
                        METAMORPHIC_NOW_MS,
                    )
                    .expect("generated stream group must accept idle consumers");
                let _ = original
                    .xreadgroup(
                        &physical_key,
                        &first_group_name,
                        &pending_seed_consumer,
                        group_read_options(StreamGroupReadCursor::NewEntries, false, Some(1)),
                        METAMORPHIC_NOW_MS,
                    )
                    .expect("generated stream group must allow pending seeds")
                    .expect("generated stream group must exist");
                let _ = original
                    .xclaim(
                        &physical_key,
                        &first_group_name,
                        &claim_consumer,
                        &[pending_entry_id],
                        StreamClaimOptions {
                            min_idle_time_ms: 0,
                            idle_ms: None,
                            time_ms: Some(u64::from(delivery_time_ms)),
                            retry_count: Some(u64::from(retry_count)),
                            force: false,
                            justid: false,
                            last_id: None,
                        },
                        METAMORPHIC_NOW_MS,
                    )
                    .expect("generated stream group must allow claim seeds");

                let commands = original.to_aof_commands(METAMORPHIC_NOW_MS);
                let expected_snapshot = snapshot_aof_replay_state(&original);

                let mut replayed = replay_aof_commands(&commands);
                prop_assert_eq!(snapshot_aof_replay_state(&replayed), expected_snapshot);
                prop_assert_eq!(replayed.to_aof_commands(METAMORPHIC_NOW_MS), commands);
            }
        }
    }
}
