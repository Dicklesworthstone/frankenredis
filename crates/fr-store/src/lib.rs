#![forbid(unsafe_code)]

use fr_expire::evaluate_expiry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Bound::{Excluded, Unbounded};

pub type StreamId = (u64, u64);
pub type StreamField = (Vec<u8>, Vec<u8>);
pub type StreamEntries = BTreeMap<StreamId, Vec<StreamField>>;
pub type StreamRecord = (StreamId, Vec<StreamField>);
pub type StreamInfoBounds = (usize, Option<StreamRecord>, Option<StreamRecord>);
pub type StreamConsumerInfo = Vec<u8>;
pub type StreamPendingEntries = BTreeMap<StreamId, Vec<u8>>;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    ValueNotInteger,
    ValueNotFloat,
    IntegerOverflow,
    KeyNotFound,
    WrongType,
    InvalidHllValue,
}

/// The inner value held by a key in the store.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(Vec<u8>),
    Hash(HashMap<Vec<u8>, Vec<u8>>),
    List(VecDeque<Vec<u8>>),
    Set(HashSet<Vec<u8>>),
    /// Sorted set: member -> score mapping. Ordered iteration is done on demand.
    SortedSet(HashMap<Vec<u8>, f64>),
    /// Stream entries keyed by `(milliseconds, sequence)` stream IDs.
    Stream(StreamEntries),
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    value: Value,
    expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttlValue {
    KeyMissing,
    NoExpiry,
    Remaining(i64),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveExpireCycleResult {
    pub sampled_keys: usize,
    pub evicted_keys: usize,
    pub next_cursor: usize,
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

#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<Vec<u8>, Entry>,
    stream_groups: HashMap<Vec<u8>, StreamGroupState>,
}

impl Store {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get a string value. Returns `None` if the key doesn't exist.
    /// Returns `Err(WrongType)` if the key holds a non-string value.
    pub fn get(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => Ok(Some(v.clone())),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn set(&mut self, key: Vec<u8>, value: Vec<u8>, px_ttl_ms: Option<u64>, now_ms: u64) {
        let expires_at_ms = px_ttl_ms.map(|ttl| now_ms.saturating_add(ttl));
        self.stream_groups.remove(key.as_slice());
        self.entries.insert(
            key,
            Entry {
                value: Value::String(value),
                expires_at_ms,
            },
        );
    }

    pub fn del(&mut self, keys: &[Vec<u8>], now_ms: u64) -> u64 {
        let mut removed = 0_u64;
        for key in keys {
            self.drop_if_expired(key, now_ms);
            if self.entries.remove(key.as_slice()).is_some() {
                self.stream_groups.remove(key.as_slice());
                removed = removed.saturating_add(1);
            }
        }
        removed
    }

    pub fn exists(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        self.entries.contains_key(key)
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
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: Value::String(next.to_string().into_bytes()),
                expires_at_ms,
            },
        );
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
        if milliseconds <= 0 {
            self.entries.remove(key);
            return true;
        }

        let ttl_ms = u64::try_from(milliseconds).unwrap_or(u64::MAX);
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        if let Some(entry) = self.entries.get_mut(key) {
            entry.expires_at_ms = Some(expires_at_ms);
        }
        true
    }

    pub fn expire_at_milliseconds(&mut self, key: &[u8], when_ms: i64, now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        if !self.entries.contains_key(key) {
            return false;
        }

        if i128::from(when_ms) <= i128::from(now_ms) {
            self.entries.remove(key);
            return true;
        }

        let expires_at_ms = u64::try_from(when_ms).unwrap_or(u64::MAX);
        if let Some(entry) = self.entries.get_mut(key) {
            entry.expires_at_ms = Some(expires_at_ms);
        }
        true
    }

    #[must_use]
    pub fn pttl(&mut self, key: &[u8], now_ms: u64) -> PttlValue {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get(key) else {
            return PttlValue::KeyMissing;
        };
        let decision = evaluate_expiry(now_ms, entry.expires_at_ms);
        if decision.should_evict {
            self.entries.remove(key);
            return PttlValue::KeyMissing;
        }
        if decision.remaining_ms == -1 {
            PttlValue::NoExpiry
        } else {
            PttlValue::Remaining(decision.remaining_ms)
        }
    }

    pub fn append(&mut self, key: &[u8], value: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        if let Some(entry) = self.entries.get_mut(key) {
            match &mut entry.value {
                Value::String(v) => {
                    v.extend_from_slice(value);
                    Ok(v.len())
                }
                _ => Err(StoreError::WrongType),
            }
        } else {
            let len = value.len();
            self.entries.insert(
                key.to_vec(),
                Entry {
                    value: Value::String(value.to_vec()),
                    expires_at_ms: None,
                },
            );
            Ok(len)
        }
    }

    pub fn strlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => Ok(v.len()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    /// MGET returns values for each key; non-string keys return None (like Redis).
    #[must_use]
    pub fn mget(&mut self, keys: &[&[u8]], now_ms: u64) -> Vec<Option<Vec<u8>>> {
        keys.iter()
            .map(|key| {
                self.drop_if_expired(key, now_ms);
                self.entries.get(*key).and_then(|entry| match &entry.value {
                    Value::String(v) => Some(v.clone()),
                    _ => None,
                })
            })
            .collect()
    }

    pub fn setnx(&mut self, key: Vec<u8>, value: Vec<u8>, now_ms: u64) -> bool {
        self.drop_if_expired(&key, now_ms);
        if self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(
            key,
            Entry {
                value: Value::String(value),
                expires_at_ms: None,
            },
        );
        true
    }

    pub fn getset(
        &mut self,
        key: Vec<u8>,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(&key, now_ms);
        let (old, expires_at_ms) = match self.entries.get(&key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (Some(v.clone()), entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (None, None),
        };
        self.entries.insert(
            key,
            Entry {
                value: Value::String(value),
                expires_at_ms,
            },
        );
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
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: Value::String(next.to_string().into_bytes()),
                expires_at_ms,
            },
        );
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
        if next.is_infinite() || next.is_nan() {
            return Err(StoreError::ValueNotFloat);
        }
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: Value::String(next.to_string().into_bytes()),
                expires_at_ms,
            },
        );
        Ok(next)
    }

    pub fn getdel(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(_) => {}
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        }
        let Some(entry) = self.entries.remove(key) else {
            return Ok(None);
        };
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let len = v.len() as i64;
                    let s = if start < 0 {
                        (len + start).max(0) as usize
                    } else {
                        start as usize
                    };
                    let e = if end < 0 {
                        (len + end).max(0) as usize
                    } else {
                        end as usize
                    };
                    if s > e || s >= v.len() {
                        Ok(Vec::new())
                    } else {
                        let end_idx = (e + 1).min(v.len());
                        Ok(v[s..end_idx].to_vec())
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
        let (mut current, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (v.clone(), entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (Vec::new(), None),
        };
        let needed = offset + value.len();
        if current.len() < needed {
            current.resize(needed, 0);
        }
        current[offset..offset + value.len()].copy_from_slice(value);
        let new_len = current.len();
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: Value::String(current),
                expires_at_ms,
            },
        );
        Ok(new_len)
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
        let (mut bytes, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => (v.clone(), entry.expires_at_ms),
                _ => return Err(StoreError::WrongType),
            },
            None => (Vec::new(), None),
        };
        if bytes.len() <= byte_idx {
            bytes.resize(byte_idx + 1, 0);
        }
        let old_bit = (bytes[byte_idx] >> bit_idx) & 1 == 1;
        if value {
            bytes[byte_idx] |= 1 << bit_idx;
        } else {
            bytes[byte_idx] &= !(1 << bit_idx);
        }
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: Value::String(bytes),
                expires_at_ms,
            },
        );
        Ok(old_bit)
    }

    pub fn getbit(&mut self, key: &[u8], offset: usize, now_ms: u64) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
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
            },
            None => Ok(false),
        }
    }

    pub fn bitcount(
        &mut self,
        key: &[u8],
        start: Option<i64>,
        end: Option<i64>,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => {
                    let len = v.len() as i64;
                    let s = match start {
                        Some(s) if s < 0 => (len + s).max(0) as usize,
                        Some(s) => s as usize,
                        None => 0,
                    };
                    let e = match end {
                        Some(e) if e < 0 => (len + e).max(0) as usize,
                        Some(e) => e as usize,
                        None => v.len().saturating_sub(1),
                    };
                    if s > e || s >= v.len() {
                        return Ok(0);
                    }
                    let end_idx = (e + 1).min(v.len());
                    let count = v[s..end_idx].iter().map(|b| b.count_ones() as usize).sum();
                    Ok(count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn bitpos(
        &mut self,
        key: &[u8],
        bit: bool,
        start: Option<i64>,
        end: Option<i64>,
        now_ms: u64,
    ) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let bytes = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(v) => v.as_slice(),
                _ => return Err(StoreError::WrongType),
            },
            None => {
                // Missing key: BITPOS 0 returns 0, BITPOS 1 returns -1
                return if bit { Ok(-1) } else { Ok(0) };
            }
        };
        if bytes.is_empty() {
            return if bit { Ok(-1) } else { Ok(0) };
        }
        let len = bytes.len() as i64;
        let has_end = end.is_some();
        let s = match start {
            Some(s) if s < 0 => (len + s).max(0) as usize,
            Some(s) => s as usize,
            None => 0,
        };
        let e = match end {
            Some(e) if e < 0 => (len + e).max(0) as usize,
            Some(e) => e as usize,
            None => bytes.len().saturating_sub(1),
        };
        if s > e || s >= bytes.len() {
            return Ok(-1);
        }
        let end_idx = (e + 1).min(bytes.len());
        let slice = &bytes[s..end_idx];
        for (byte_offset, &byte) in slice.iter().enumerate() {
            for bit_offset in 0..8u32 {
                let b = (byte >> (7 - bit_offset)) & 1 == 1;
                if b == bit {
                    return Ok(((s + byte_offset) * 8 + bit_offset as usize) as i64);
                }
            }
        }
        // If searching for 0 with no explicit end, and all bits in range are 1,
        // return the position just past the last byte of the string
        if !bit && !has_end {
            return Ok((end_idx * 8) as i64);
        }
        Ok(-1)
    }

    pub fn persist(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        if let Some(entry) = self.entries.get_mut(key)
            && entry.expires_at_ms.is_some()
        {
            entry.expires_at_ms = None;
            return true;
        }
        false
    }

    #[must_use]
    pub fn value_type(&mut self, key: &[u8], now_ms: u64) -> Option<ValueType> {
        self.drop_if_expired(key, now_ms);
        self.entries.get(key).map(|entry| match &entry.value {
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

    pub fn rename(&mut self, key: &[u8], newkey: &[u8], now_ms: u64) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        let entry = self.entries.remove(key).ok_or(StoreError::KeyNotFound)?;
        let moved_groups = self.stream_groups.remove(key);
        self.entries.remove(newkey);
        self.stream_groups.remove(newkey);
        self.entries.insert(newkey.to_vec(), entry);
        if let Some(groups) = moved_groups {
            self.stream_groups.insert(newkey.to_vec(), groups);
        }
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
        let Some(entry) = self.entries.remove(key) else {
            return Err(StoreError::KeyNotFound);
        };
        let moved_groups = self.stream_groups.remove(key);
        self.entries.insert(newkey.to_vec(), entry);
        if let Some(groups) = moved_groups {
            self.stream_groups.insert(newkey.to_vec(), groups);
        }
        Ok(true)
    }

    #[must_use]
    pub fn keys_matching(&mut self, pattern: &[u8], now_ms: u64) -> Vec<Vec<u8>> {
        // Expire all keys first so we don't return expired ones.
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }
        let mut result: Vec<Vec<u8>> = self
            .entries
            .keys()
            .filter(|key| glob_match(pattern, key))
            .cloned()
            .collect();
        result.sort();
        result
    }

    #[must_use]
    pub fn dbsize(&mut self, now_ms: u64) -> usize {
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }
        self.entries.len()
    }

    #[must_use]
    pub fn count_expiring_keys(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.expires_at_ms.is_some())
            .count()
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
        let mut cursor = 0usize;
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

            let cycle = self.run_active_expire_cycle(now_ms, cursor, sample_limit);
            sampled_keys = sampled_keys.saturating_add(cycle.sampled_keys);
            evicted_keys = evicted_keys.saturating_add(cycle.evicted_keys);
            cursor = cycle.next_cursor;

            if cycle.evicted_keys == 0 {
                let Some(candidate) = self.select_eviction_candidate(now_ms) else {
                    break;
                };
                if self.entries.remove(candidate.as_slice()).is_some() {
                    evicted_keys = evicted_keys.saturating_add(1);
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
        start_cursor: usize,
        sample_limit: usize,
    ) -> ActiveExpireCycleResult {
        if sample_limit == 0 || self.entries.is_empty() {
            return ActiveExpireCycleResult {
                sampled_keys: 0,
                evicted_keys: 0,
                next_cursor: if self.entries.is_empty() {
                    0
                } else {
                    start_cursor % self.entries.len()
                },
            };
        }

        let mut keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        keys.sort();
        let key_count = keys.len();
        let normalized_start = start_cursor % key_count;
        let sampled_keys = sample_limit.min(key_count);
        let next_key_anchor = keys[(normalized_start + sampled_keys) % key_count].clone();
        let mut evicted_keys = 0usize;

        for offset in 0..sampled_keys {
            let key_index = (normalized_start + offset) % key_count;
            let key = &keys[key_index];
            let should_evict = evaluate_expiry(
                now_ms,
                self.entries
                    .get(key.as_slice())
                    .and_then(|entry| entry.expires_at_ms),
            )
            .should_evict;
            if should_evict {
                self.entries.remove(key.as_slice());
                evicted_keys = evicted_keys.saturating_add(1);
            }
        }

        ActiveExpireCycleResult {
            sampled_keys,
            evicted_keys,
            next_cursor: if self.entries.is_empty() {
                0
            } else {
                let mut remaining_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
                remaining_keys.sort();
                remaining_keys
                    .iter()
                    .position(|key| *key == next_key_anchor)
                    .unwrap_or(0)
            },
        }
    }

    pub fn flushdb(&mut self) {
        self.entries.clear();
        self.stream_groups.clear();
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
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Hash(m) => {
                    let is_new = !m.contains_key(&field);
                    m.insert(field, value);
                    Ok(is_new)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut m = HashMap::new();
                m.insert(field, value);
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Hash(m),
                        expires_at_ms: None,
                    },
                );
                Ok(true)
            }
        }
    }

    pub fn hget(
        &mut self,
        key: &[u8],
        field: &[u8],
        now_ms: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(m.get(field).cloned()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Hash(m) => {
                    let mut removed = 0_u64;
                    for field in fields {
                        if m.remove(*field).is_some() {
                            removed += 1;
                        }
                    }
                    if m.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn hexists(&mut self, key: &[u8], field: &[u8], now_ms: u64) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(m.contains_key(field)),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    pub fn hlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(m.len()),
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> =
                        m.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    pairs.sort_by(|a, b| a.0.cmp(&b.0));
                    Ok(pairs)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn hkeys(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let mut keys: Vec<Vec<u8>> = m.keys().cloned().collect();
                    keys.sort();
                    Ok(keys)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn hvals(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => {
                    let mut pairs: Vec<(&Vec<u8>, &Vec<u8>)> = m.iter().collect();
                    pairs.sort_by_key(|(k, _)| *k);
                    Ok(pairs.into_iter().map(|(_, v)| v.clone()).collect())
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(fields.iter().map(|f| m.get(*f).cloned()).collect()),
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
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Hash(m) => {
                    let current = match m.get(field) {
                        Some(v) => parse_i64(v)?,
                        None => 0,
                    };
                    let next = current
                        .checked_add(delta)
                        .ok_or(StoreError::IntegerOverflow)?;
                    m.insert(field.to_vec(), next.to_string().into_bytes());
                    Ok(next)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut m = HashMap::new();
                m.insert(field.to_vec(), delta.to_string().into_bytes());
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Hash(m),
                        expires_at_ms: None,
                    },
                );
                Ok(delta)
            }
        }
    }

    pub fn hsetnx(
        &mut self,
        key: &[u8],
        field: Vec<u8>,
        value: Vec<u8>,
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Hash(m) => {
                    use std::collections::hash_map::Entry as HEntry;
                    if let HEntry::Vacant(e) = m.entry(field) {
                        e.insert(value);
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut m = HashMap::new();
                m.insert(field, value);
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Hash(m),
                        expires_at_ms: None,
                    },
                );
                Ok(true)
            }
        }
    }

    pub fn hstrlen(&mut self, key: &[u8], field: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(m.get(field).map_or(0, Vec::len)),
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
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Hash(m) => {
                    let current = match m.get(field) {
                        Some(v) => parse_f64(v)?,
                        None => 0.0,
                    };
                    let next = current + delta;
                    if next.is_infinite() || next.is_nan() {
                        return Err(StoreError::ValueNotFloat);
                    }
                    m.insert(field.to_vec(), next.to_string().into_bytes());
                    Ok(next)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                if delta.is_infinite() || delta.is_nan() {
                    return Err(StoreError::ValueNotFloat);
                }
                let mut m = HashMap::new();
                m.insert(field.to_vec(), delta.to_string().into_bytes());
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Hash(m),
                        expires_at_ms: None,
                    },
                );
                Ok(delta)
            }
        }
    }

    pub fn hrandfield(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(m) => Ok(m.keys().next().cloned()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
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
                    Ok(l.len())
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                for v in values {
                    l.push_front(v.clone());
                }
                let len = l.len();
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::List(l),
                        expires_at_ms: None,
                    },
                );
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
                    Ok(l.len())
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                for v in values {
                    l.push_back(v.clone());
                }
                let len = l.len();
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::List(l),
                        expires_at_ms: None,
                    },
                );
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
                    if l.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(val)
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
                    if l.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(val)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn llen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => Ok(l.len()),
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len as usize {
                        return Ok(Vec::new());
                    }
                    let e = e.min(len as usize - 1);
                    Ok(l.iter().skip(s).take(e - s + 1).cloned().collect())
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => {
                    let len = l.len() as i64;
                    let idx = normalize_index(index, len);
                    Ok(l.get(idx).cloned())
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
                    let idx = normalize_index(index, len);
                    if idx >= l.len() {
                        return Err(StoreError::KeyNotFound);
                    }
                    l[idx] = value;
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::List(l) => Ok(l.iter().position(|v| v.as_slice() == element)),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
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
                        Ok(l.len() as i64)
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
                        Ok(l.len() as i64)
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
                        let mut i = 0;
                        while i < l.len() && removed < limit {
                            if l[i].as_slice() == value {
                                l.remove(i);
                                removed += 1;
                            } else {
                                i += 1;
                            }
                        }
                    } else if count < 0 {
                        let limit = (-count) as u64;
                        let mut i = l.len();
                        while i > 0 && removed < limit {
                            i -= 1;
                            if l[i].as_slice() == value {
                                l.remove(i);
                                removed += 1;
                            }
                        }
                    } else {
                        let old_len = l.len();
                        l.retain(|v| v.as_slice() != value);
                        removed = (old_len - l.len()) as u64;
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
        // Pop from source
        let popped = match self.entries.get_mut(source) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => l.pop_back(),
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };
        let Some(val) = popped else {
            return Ok(None);
        };
        // Push to destination
        match self.entries.get_mut(destination) {
            Some(entry) => match &mut entry.value {
                Value::List(l) => l.push_front(val.clone()),
                _ => return Err(StoreError::WrongType),
            },
            None => {
                let mut l = VecDeque::new();
                l.push_front(val.clone());
                self.entries.insert(
                    destination.to_vec(),
                    Entry {
                        value: Value::List(l),
                        expires_at_ms: None,
                    },
                );
            }
        }
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
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= len as usize {
                        l.clear();
                    } else {
                        let end = (e + 1).min(len as usize);
                        let trimmed: VecDeque<Vec<u8>> =
                            l.iter().skip(s).take(end - s).cloned().collect();
                        *l = trimmed;
                    }
                    if l.is_empty() {
                        self.entries.remove(key);
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
                    if eq_ascii_ci(wherefrom, b"LEFT") {
                        l.pop_front()
                    } else {
                        l.pop_back()
                    }
                }
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(None),
        };
        let Some(val) = popped else {
            return Ok(None);
        };
        // Clean up empty source.
        if let Some(entry) = self.entries.get(source)
            && let Value::List(l) = &entry.value
            && l.is_empty()
        {
            self.entries.remove(source);
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
                self.entries.insert(
                    destination.to_vec(),
                    Entry {
                        value: Value::List(l),
                        expires_at_ms: None,
                    },
                );
            }
        }
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
                    Ok(added)
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut s = HashSet::new();
                let mut added = 0_u64;
                for m in members {
                    if s.insert(m.clone()) {
                        added += 1;
                    }
                }
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Set(s),
                        expires_at_ms: None,
                    },
                );
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
                        self.entries.remove(key);
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn smembers(&mut self, key: &[u8], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let mut members: Vec<Vec<u8>> = s.iter().cloned().collect();
                    members.sort();
                    Ok(members)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn scard(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => Ok(s.len()),
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => Ok(s.contains(member)),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(false),
        }
    }

    /// Helper: get the set for a key, or an empty set if key doesn't exist.
    fn get_set_or_empty(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<HashSet<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => Ok(s.clone()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(HashSet::new()),
        }
    }

    pub fn sinter(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = self.get_set_or_empty(keys[0], now_ms)?;
        for key in &keys[1..] {
            let other = self.get_set_or_empty(key, now_ms)?;
            result.retain(|m| other.contains(m));
        }
        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn sunion(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        let mut result = HashSet::new();
        for key in keys {
            let s = self.get_set_or_empty(key, now_ms)?;
            result.extend(s);
        }
        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn sdiff(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<Vec<Vec<u8>>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = self.get_set_or_empty(keys[0], now_ms)?;
        for key in &keys[1..] {
            let other = self.get_set_or_empty(key, now_ms)?;
            result.retain(|m| !other.contains(m));
        }
        let mut v: Vec<Vec<u8>> = result.into_iter().collect();
        v.sort();
        Ok(v)
    }

    pub fn spop(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        let mut should_remove_key = false;
        let member = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Set(s) => {
                    // HashSet iteration order is arbitrary, which provides pseudo-random behavior
                    let member = s.iter().next().cloned();
                    if let Some(ref m) = member {
                        s.remove(m);
                    }
                    if s.is_empty() {
                        should_remove_key = true;
                    }
                    Ok(member)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }?;
        if should_remove_key {
            self.entries.remove(key);
        }
        Ok(member)
    }

    pub fn srandmember(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => Ok(s.iter().next().cloned()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
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
        // Remove from source
        let removed = match self.entries.get_mut(source) {
            Some(entry) => match &mut entry.value {
                Value::Set(s) => s.remove(member),
                _ => return Err(StoreError::WrongType),
            },
            None => return Ok(false),
        };
        if !removed {
            return Ok(false);
        }
        // Clean up empty source
        if let Some(entry) = self.entries.get(source)
            && let Value::Set(s) = &entry.value
            && s.is_empty()
        {
            self.entries.remove(source);
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
        self.entries.remove(destination);
        if !result.is_empty() {
            let set: HashSet<Vec<u8>> = result.into_iter().collect();
            self.entries.insert(
                destination.to_vec(),
                Entry {
                    value: Value::Set(set),
                    expires_at_ms: None,
                },
            );
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
        self.entries.remove(destination);
        if !result.is_empty() {
            let set: HashSet<Vec<u8>> = result.into_iter().collect();
            self.entries.insert(
                destination.to_vec(),
                Entry {
                    value: Value::Set(set),
                    expires_at_ms: None,
                },
            );
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
        self.entries.remove(destination);
        if !result.is_empty() {
            let set: HashSet<Vec<u8>> = result.into_iter().collect();
            self.entries.insert(
                destination.to_vec(),
                Entry {
                    value: Value::Set(set),
                    expires_at_ms: None,
                },
            );
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
        self.drop_if_expired(key, now_ms);
        let entry = self.entries.entry(key.to_vec()).or_insert_with(|| Entry {
            value: Value::SortedSet(HashMap::new()),
            expires_at_ms: None,
        });
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let mut added = 0;
        for (score, member) in members {
            if zs.insert(member.clone(), *score).is_none() {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Remove members. Returns count of members actually removed.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let Some(entry) = self.entries.get_mut(key) else {
            return Ok(0);
        };
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let mut removed = 0_u64;
        for member in members {
            if zs.remove(*member).is_some() {
                removed += 1;
            }
        }
        if zs.is_empty() {
            self.entries.remove(key);
        }
        Ok(removed)
    }

    /// Get the score of a member. Returns None if member or key doesn't exist.
    pub fn zscore(
        &mut self,
        key: &[u8],
        member: &[u8],
        now_ms: u64,
    ) -> Result<Option<f64>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => Ok(zs.get(member).copied()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    /// Return cardinality of sorted set.
    pub fn zcard(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => Ok(zs.len()),
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let Some(score) = zs.get(member).copied() else {
                        return Ok(None);
                    };
                    let rank = zs
                        .iter()
                        .filter(|(m, s)| score_member_lt(**s, m, score, member))
                        .count();
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let Some(score) = zs.get(member).copied() else {
                        return Ok(None);
                    };
                    let rank = zs
                        .iter()
                        .filter(|(m, s)| score_member_lt(score, member, **s, m))
                        .count();
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let sorted = sorted_members_asc(zs);
                    let len = sorted.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= sorted.len() {
                        return Ok(Vec::new());
                    }
                    let end = (e + 1).min(sorted.len());
                    Ok(sorted[s..end].iter().map(|(_, m)| m.clone()).collect())
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let mut sorted = sorted_members_asc(zs);
                    sorted.reverse();
                    let len = sorted.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= sorted.len() {
                        return Ok(Vec::new());
                    }
                    let end = (e + 1).min(sorted.len());
                    Ok(sorted[s..end].iter().map(|(_, m)| m.clone()).collect())
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
        min: f64,
        max: f64,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let sorted = sorted_members_asc(zs);
                    Ok(sorted
                        .into_iter()
                        .filter(|(s, _)| *s >= min && *s <= max)
                        .map(|(_, m)| m)
                        .collect())
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    /// Count members with scores within [min, max] range.
    pub fn zcount(
        &mut self,
        key: &[u8],
        min: f64,
        max: f64,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    Ok(zs.values().filter(|s| **s >= min && **s <= max).count())
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
        self.drop_if_expired(key, now_ms);
        let entry = self.entries.entry(key.to_vec()).or_insert_with(|| Entry {
            value: Value::SortedSet(HashMap::new()),
            expires_at_ms: None,
        });
        let Value::SortedSet(zs) = &mut entry.value else {
            return Err(StoreError::WrongType);
        };
        let new_score = zs.get(&member).unwrap_or(&0.0) + delta;
        zs.insert(member, new_score);
        Ok(new_score)
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
        if zs.is_empty() {
            return Ok(None);
        }
        let min_member = zs
            .iter()
            .min_by(|(m1, s1), (m2, s2)| cmp_score_member(**s1, m1, **s2, m2))
            .map(|(m, s)| (m.clone(), *s));
        let Some(min_member) = min_member else {
            return Ok(None);
        };
        zs.remove(&min_member.0);
        if zs.is_empty() {
            self.entries.remove(key);
        }
        Ok(Some(min_member))
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
        if zs.is_empty() {
            return Ok(None);
        }
        let max_member = zs
            .iter()
            .max_by(|(m1, s1), (m2, s2)| cmp_score_member(**s1, m1, **s2, m2))
            .map(|(m, s)| (m.clone(), *s));
        let Some(max_member) = max_member else {
            return Ok(None);
        };
        zs.remove(&max_member.0);
        if zs.is_empty() {
            self.entries.remove(key);
        }
        Ok(Some(max_member))
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let sorted = sorted_members_asc(zs);
                    let len = sorted.len() as i64;
                    let s = normalize_index(start, len);
                    let e = normalize_index(stop, len);
                    if s > e || s >= sorted.len() {
                        return Ok(Vec::new());
                    }
                    let end = (e + 1).min(sorted.len());
                    Ok(sorted[s..end]
                        .iter()
                        .map(|(score, m)| (m.clone(), *score))
                        .collect())
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let mut sorted = sorted_members_asc(zs);
                    sorted.reverse();
                    let result = sorted
                        .into_iter()
                        .filter(|(score, _)| *score >= min && *score <= max)
                        .map(|(_, m)| m)
                        .collect();
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let sorted = sorted_members_asc(zs);
                    let result = sorted
                        .into_iter()
                        .filter(|(_, m)| lex_in_range(m, min, max))
                        .map(|(_, m)| m)
                        .collect();
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
        let mut members = self.zrangebylex(key, min, max, now_ms)?;
        members.reverse();
        Ok(members)
    }

    pub fn zlexcount(
        &mut self,
        key: &[u8],
        min: &[u8],
        max: &[u8],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        let members = self.zrangebylex(key, min, max, now_ms)?;
        Ok(members.len())
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
                    let sorted = sorted_members_asc(zs);
                    let len = sorted.len();
                    let s = normalize_index(start, len as i64);
                    let e = normalize_index(stop, len as i64);
                    if s > e || s >= len {
                        return Ok(0);
                    }
                    let end = (e + 1).min(len);
                    let to_remove: Vec<Vec<u8>> =
                        sorted[s..end].iter().map(|(_, m)| m.clone()).collect();
                    let count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    if zs.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zremrangebyscore(
        &mut self,
        key: &[u8],
        min: f64,
        max: f64,
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => {
                    let to_remove: Vec<Vec<u8>> = zs
                        .iter()
                        .filter(|(_, score)| **score >= min && **score <= max)
                        .map(|(m, _)| m.clone())
                        .collect();
                    let count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    if zs.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(count)
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
                    let sorted = sorted_members_asc(zs);
                    let to_remove: Vec<Vec<u8>> = sorted
                        .into_iter()
                        .filter(|(_, m)| lex_in_range(m, min, max))
                        .map(|(_, m)| m)
                        .collect();
                    let count = to_remove.len();
                    for m in &to_remove {
                        zs.remove(m);
                    }
                    if zs.is_empty() {
                        self.entries.remove(key);
                    }
                    Ok(count)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn zrandmember(&mut self, key: &[u8], now_ms: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => Ok(zs.keys().next().cloned()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
    }

    pub fn zmscore(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
        now_ms: u64,
    ) -> Result<Vec<Option<f64>>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => Ok(members.iter().map(|m| zs.get(*m).copied()).collect()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(members.iter().map(|_| None).collect()),
        }
    }

    pub fn xlast_id(&mut self, key: &[u8], now_ms: u64) -> Result<Option<StreamId>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => Ok(entries.last_key_value().map(|(id, _)| *id)),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
        }
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
                    Ok(())
                }
                _ => Err(StoreError::WrongType),
            },
            None => {
                let mut entries = BTreeMap::new();
                entries.insert(id, fields.to_vec());
                self.stream_groups.remove(key);
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::Stream(entries),
                        expires_at_ms: None,
                    },
                );
                Ok(())
            }
        }
    }

    pub fn xlen(&mut self, key: &[u8], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(entries) => Ok(entries.len()),
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
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
                    Ok(out)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(Vec::new()),
        }
    }

    pub fn xdel(&mut self, key: &[u8], ids: &[StreamId], now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    let mut removed = 0usize;
                    for id in ids {
                        if entries.remove(id).is_some() {
                            removed = removed.saturating_add(1);
                        }
                    }
                    if let Some(groups) = self.stream_groups.get_mut(key) {
                        for group_state in groups.values_mut() {
                            for id in ids {
                                group_state.pending.remove(id);
                            }
                        }
                    }
                    Ok(removed)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn xtrim(&mut self, key: &[u8], max_len: usize, now_ms: u64) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::Stream(entries) => {
                    if entries.len() <= max_len {
                        return Ok(0);
                    }
                    let to_remove = entries.len() - max_len;
                    let remove_ids: Vec<StreamId> =
                        entries.keys().copied().take(to_remove).collect();
                    for id in &remove_ids {
                        entries.remove(id);
                    }
                    if let Some(groups) = self.stream_groups.get_mut(key) {
                        for group_state in groups.values_mut() {
                            for id in &remove_ids {
                                group_state.pending.remove(id);
                            }
                        }
                    }
                    Ok(to_remove)
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(0),
        }
    }

    pub fn xread(
        &mut self,
        key: &[u8],
        start_exclusive: StreamId,
        count: Option<usize>,
        now_ms: u64,
    ) -> Result<Vec<StreamRecord>, StoreError> {
        self.drop_if_expired(key, now_ms);
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
        self.drop_if_expired(key, now_ms);
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
                                for (id, owner) in
                                    group_state.pending.range((Excluded(start_id), Unbounded))
                                {
                                    if owner.as_slice() != consumer {
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
                    group_state.pending.insert(*id, consumer.clone());
                }
            }
        }

        Ok(Some(records))
    }

    pub fn xinfo_stream(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<StreamInfoBounds>, StoreError> {
        self.drop_if_expired(key, now_ms);
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
            self.entries.insert(
                key.to_vec(),
                Entry {
                    value: Value::Stream(BTreeMap::new()),
                    expires_at_ms: None,
                },
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
        self.drop_if_expired(key, now_ms);
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
                    Ok(Some(group_state.consumers.insert(consumer.to_vec())))
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
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Stream(_) => {
                    let Some(groups) = self.stream_groups.get(key) else {
                        return Ok(None);
                    };
                    let Some(group_state) = groups.get(group) else {
                        return Ok(None);
                    };
                    Ok(Some(group_state.consumers.iter().cloned().collect()))
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
                    group_state.pending.retain(|_, owner| {
                        let keep = owner.as_slice() != consumer;
                        if !keep {
                            removed_pending = removed_pending.saturating_add(1);
                        }
                        keep
                    });
                    Ok(Some(removed_pending))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok(None),
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
        let (mut registers, existed) = match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::String(data) => {
                    if data.starts_with(HLL_MAGIC) {
                        (hll_parse_registers(data)?, true)
                    } else {
                        return Err(StoreError::InvalidHllValue);
                    }
                }
                _ => return Err(StoreError::WrongType),
            },
            None => (vec![0u8; HLL_REGISTERS], false),
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
            let data = hll_encode(&registers);
            self.entries.insert(
                key.to_vec(),
                Entry {
                    value: Value::String(data),
                    expires_at_ms: expires_at,
                },
            );
        }
        Ok(created || modified)
    }

    /// PFCOUNT: return the approximate cardinality for one or more HLL keys.
    /// Multiple keys are merged into a temporary union before estimating.
    pub fn pfcount(&mut self, keys: &[&[u8]], now_ms: u64) -> Result<u64, StoreError> {
        let mut merged = vec![0u8; HLL_REGISTERS];
        for &key in keys {
            self.drop_if_expired(key, now_ms);
            if let Some(entry) = self.entries.get(key) {
                match &entry.value {
                    Value::String(data) => {
                        if data.starts_with(HLL_MAGIC) {
                            let regs = hll_parse_registers(data)?;
                            for i in 0..HLL_REGISTERS {
                                merged[i] = merged[i].max(regs[i]);
                            }
                        } else {
                            return Err(StoreError::InvalidHllValue);
                        }
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

        // Include dest if it already holds an HLL
        self.drop_if_expired(dest, now_ms);
        if let Some(entry) = self.entries.get(dest) {
            match &entry.value {
                Value::String(data) => {
                    if data.starts_with(HLL_MAGIC) {
                        let regs = hll_parse_registers(data)?;
                        for i in 0..HLL_REGISTERS {
                            merged[i] = merged[i].max(regs[i]);
                        }
                    } else {
                        return Err(StoreError::InvalidHllValue);
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
                        if data.starts_with(HLL_MAGIC) {
                            let regs = hll_parse_registers(data)?;
                            for i in 0..HLL_REGISTERS {
                                merged[i] = merged[i].max(regs[i]);
                            }
                        } else {
                            return Err(StoreError::InvalidHllValue);
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }

        let data = hll_encode(&merged);
        self.entries.insert(
            dest.to_vec(),
            Entry {
                value: Value::String(data),
                expires_at_ms: None,
            },
        );
        Ok(())
    }

    fn drop_if_expired(&mut self, key: &[u8], now_ms: u64) {
        let should_evict = evaluate_expiry(
            now_ms,
            self.entries.get(key).and_then(|entry| entry.expires_at_ms),
        )
        .should_evict;
        if should_evict {
            self.entries.remove(key);
            self.stream_groups.remove(key);
        }
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
                        entry.expires_at_ms = exp;
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
        // Collect values, treating missing keys as empty strings
        let mut values: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
        for &key in keys {
            self.drop_if_expired(key, now_ms);
            match self.entries.get(key) {
                Some(entry) => match &entry.value {
                    Value::String(v) => values.push(v.clone()),
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
            for val in values.iter().skip(1) {
                for (i, byte) in result.iter_mut().enumerate() {
                    let b = val.get(i).copied().unwrap_or(0);
                    if eq_ascii_ci(op, b"AND") {
                        *byte &= b;
                    } else if eq_ascii_ci(op, b"OR") {
                        *byte |= b;
                    } else if eq_ascii_ci(op, b"XOR") {
                        *byte ^= b;
                    }
                }
            }
        }

        let len = result.len();
        self.entries.insert(
            dest.to_vec(),
            Entry {
                value: Value::String(result),
                expires_at_ms: None,
            },
        );
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
        let mut combined: std::collections::HashMap<Vec<u8>, f64> =
            std::collections::HashMap::new();

        for (i, &key) in keys.iter().enumerate() {
            self.drop_if_expired(key, now_ms);
            let weight = weights.get(i).copied().unwrap_or(1.0);
            if let Some(entry) = self.entries.get(key) {
                match &entry.value {
                    Value::SortedSet(zs) => {
                        for (member, &score) in zs {
                            let weighted = score * weight;
                            let current = combined.entry(member.clone()).or_insert(0.0);
                            *current = aggregate_scores(*current, weighted, aggregate);
                        }
                    }
                    _ => return Err(StoreError::WrongType),
                }
            }
        }

        let count = combined.len();
        self.entries.insert(
            dest.to_vec(),
            Entry {
                value: Value::SortedSet(combined),
                expires_at_ms: None,
            },
        );
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
            self.entries.insert(
                dest.to_vec(),
                Entry {
                    value: Value::SortedSet(std::collections::HashMap::new()),
                    expires_at_ms: None,
                },
            );
            return Ok(0);
        }

        // Start with members from the first key
        self.drop_if_expired(keys[0], now_ms);
        let mut result: std::collections::HashMap<Vec<u8>, f64> = match self.entries.get(keys[0]) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let w = weights.first().copied().unwrap_or(1.0);
                    zs.iter().map(|(m, &s)| (m.clone(), s * w)).collect()
                }
                _ => return Err(StoreError::WrongType),
            },
            None => std::collections::HashMap::new(),
        };

        // Intersect with remaining keys
        for (i, &key) in keys.iter().enumerate().skip(1) {
            self.drop_if_expired(key, now_ms);
            let weight = weights.get(i).copied().unwrap_or(1.0);
            match self.entries.get(key) {
                Some(entry) => match &entry.value {
                    Value::SortedSet(zs) => {
                        result.retain(|member, score| {
                            if let Some(&other_score) = zs.get(member) {
                                *score = aggregate_scores(*score, other_score * weight, aggregate);
                                true
                            } else {
                                false
                            }
                        });
                    }
                    _ => return Err(StoreError::WrongType),
                },
                None => {
                    result.clear();
                }
            }
        }

        let count = result.len();
        self.entries.insert(
            dest.to_vec(),
            Entry {
                value: Value::SortedSet(result),
                expires_at_ms: None,
            },
        );
        Ok(count)
    }

    /// SMISMEMBER: check membership for multiple members.
    pub fn smismember(
        &mut self,
        key: &[u8],
        members: &[&[u8]],
        now_ms: u64,
    ) -> Result<Vec<bool>, StoreError> {
        self.drop_if_expired(key, now_ms);
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => Ok(members.iter().map(|m| s.contains(*m)).collect()),
                _ => Err(StoreError::WrongType),
            },
            None => Ok(vec![false; members.len()]),
        }
    }

    // ── Server / utility operations ────────────────────────────────

    /// Return a random live key, or None if the keyspace is empty.
    #[must_use]
    pub fn randomkey(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        // Expire all keys first so we don't return expired ones.
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }
        self.entries.keys().next().cloned()
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
        // Expire stale keys
        let all_keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        for key in &all_keys {
            self.drop_if_expired(key, now_ms);
        }

        let mut keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        keys.sort();

        let start = cursor as usize;
        if start >= keys.len() {
            return (0, Vec::new());
        }

        let batch_size = count.max(1);
        let mut result = Vec::new();
        let mut pos = start;

        while pos < keys.len() && result.len() < batch_size {
            if let Some(pat) = pattern {
                if glob_match(pat, &keys[pos]) {
                    result.push(keys[pos].clone());
                }
            } else {
                result.push(keys[pos].clone());
            }
            pos += 1;
        }

        let next_cursor = if pos >= keys.len() { 0 } else { pos as u64 };
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Hash(h) => {
                    let mut fields: Vec<(Vec<u8>, Vec<u8>)> =
                        h.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    fields.sort_by(|a, b| a.0.cmp(&b.0));

                    let start = cursor as usize;
                    if start >= fields.len() {
                        return Ok((0, Vec::new()));
                    }

                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;
                    while pos < fields.len() && result.len() < batch_size {
                        if let Some(pat) = pattern {
                            if glob_match(pat, &fields[pos].0) {
                                result.push(fields[pos].clone());
                            }
                        } else {
                            result.push(fields[pos].clone());
                        }
                        pos += 1;
                    }

                    let next = if pos >= fields.len() { 0 } else { pos as u64 };
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::Set(s) => {
                    let mut members: Vec<Vec<u8>> = s.iter().cloned().collect();
                    members.sort();

                    let start = cursor as usize;
                    if start >= members.len() {
                        return Ok((0, Vec::new()));
                    }

                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;
                    while pos < members.len() && result.len() < batch_size {
                        if let Some(pat) = pattern {
                            if glob_match(pat, &members[pos]) {
                                result.push(members[pos].clone());
                            }
                        } else {
                            result.push(members[pos].clone());
                        }
                        pos += 1;
                    }

                    let next = if pos >= members.len() { 0 } else { pos as u64 };
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
        match self.entries.get(key) {
            Some(entry) => match &entry.value {
                Value::SortedSet(zs) => {
                    let mut pairs: Vec<(Vec<u8>, f64)> =
                        zs.iter().map(|(m, &s)| (m.clone(), s)).collect();
                    pairs.sort_by(|a, b| {
                        a.1.partial_cmp(&b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a.0.cmp(&b.0))
                    });

                    let start = cursor as usize;
                    if start >= pairs.len() {
                        return Ok((0, Vec::new()));
                    }

                    let batch_size = count.max(1);
                    let mut result = Vec::new();
                    let mut pos = start;
                    while pos < pairs.len() && result.len() < batch_size {
                        if let Some(pat) = pattern {
                            if glob_match(pat, &pairs[pos].0) {
                                result.push(pairs[pos].clone());
                            }
                        } else {
                            result.push(pairs[pos].clone());
                        }
                        pos += 1;
                    }

                    let next = if pos >= pairs.len() { 0 } else { pos as u64 };
                    Ok((next, result))
                }
                _ => Err(StoreError::WrongType),
            },
            None => Ok((0, Vec::new())),
        }
    }

    /// TOUCH: returns count of keys that exist (and updates last access time in Redis, here just checks existence).
    pub fn touch(&mut self, keys: &[&[u8]], now_ms: u64) -> i64 {
        let mut count = 0i64;
        for &key in keys {
            self.drop_if_expired(key, now_ms);
            if self.entries.contains_key(key) {
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

        self.entries.insert(destination.to_vec(), entry);
        Ok(true)
    }

    #[must_use]
    pub fn state_digest(&self) -> String {
        let mut rows = self.entries.iter().collect::<Vec<_>>();
        rows.sort_by_key(|(key, _)| *key);
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for (key, entry) in rows {
            hash = fnv1a_update(hash, key);
            match &entry.value {
                Value::String(v) => {
                    hash = fnv1a_update(hash, b"S");
                    hash = fnv1a_update(hash, v);
                }
                Value::Hash(m) => {
                    hash = fnv1a_update(hash, b"H");
                    let mut fields: Vec<_> = m.iter().collect();
                    fields.sort_by_key(|(k, _)| *k);
                    for (k, v) in fields {
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
                    let mut pairs: Vec<_> = zs.iter().collect();
                    pairs.sort_by(|a, b| a.0.cmp(b.0));
                    for (member, score) in pairs {
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
        }
        format!("{hash:016x}")
    }

    fn estimate_memory_usage_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(key, entry)| estimate_entry_memory_usage_bytes(key, entry))
            .sum()
    }

    fn select_eviction_candidate(&mut self, now_ms: u64) -> Option<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = self.entries.keys().cloned().collect();
        keys.sort();
        for key in keys {
            self.drop_if_expired(&key, now_ms);
            if self.entries.contains_key(key.as_slice()) {
                return Some(key);
            }
        }
        None
    }
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
    let text = std::str::from_utf8(bytes).map_err(|_| StoreError::ValueNotInteger)?;
    text.parse::<i64>().map_err(|_| StoreError::ValueNotInteger)
}

fn parse_f64(bytes: &[u8]) -> Result<f64, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|_| StoreError::ValueNotFloat)?;
    text.parse::<f64>().map_err(|_| StoreError::ValueNotFloat)
}

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Convert a Redis-style index (negative = from end) to a `usize`.
fn normalize_index(index: i64, len: i64) -> usize {
    if index < 0 {
        let adjusted = len.saturating_add(index);
        if adjusted < 0 { 0 } else { adjusted as usize }
    } else {
        index as usize
    }
}

/// Compare (score, member) pairs for sorted set ordering.
/// Redis sorts by score first, then by member lexicographically for ties.
fn cmp_score_member(s1: f64, m1: &[u8], s2: f64, m2: &[u8]) -> std::cmp::Ordering {
    s1.total_cmp(&s2).then_with(|| m1.cmp(m2))
}

/// Returns true if (s1, m1) < (s2, m2) in Redis sorted set ordering.
fn score_member_lt(s1: f64, m1: &[u8], s2: f64, m2: &[u8]) -> bool {
    cmp_score_member(s1, m1, s2, m2) == std::cmp::Ordering::Less
}

/// Return sorted members as (score, member) pairs in ascending order.
fn sorted_members_asc(zs: &HashMap<Vec<u8>, f64>) -> Vec<(f64, Vec<u8>)> {
    let mut pairs: Vec<(f64, Vec<u8>)> = zs.iter().map(|(m, &s)| (s, m.clone())).collect();
    pairs.sort_by(|(s1, m1), (s2, m2)| cmp_score_member(*s1, m1, *s2, m2));
    pairs
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
const HLL_MAGIC: &[u8] = b"HYLL";
const HLL_DATA_SIZE: usize = HLL_MAGIC.len() + HLL_REGISTERS; // 16388

/// FNV-1a 64-bit hash for HyperLogLog element hashing.
fn hll_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Position of the leftmost 1-bit in a `(64 - HLL_P)`-bit value, counting from 1.
/// Returns `64 - HLL_P + 1` when `w == 0` (all zeros).
fn hll_rho(w: u64) -> u8 {
    let width = 64 - HLL_P; // 50
    if w == 0 {
        return (width + 1) as u8;
    }
    let lz = w.leading_zeros() - HLL_P; // subtract the high bits that don't belong to w
    (lz + 1) as u8
}

fn hll_parse_registers(data: &[u8]) -> Result<Vec<u8>, StoreError> {
    if data.len() != HLL_DATA_SIZE || !data.starts_with(HLL_MAGIC) {
        return Err(StoreError::InvalidHllValue);
    }
    Ok(data[HLL_MAGIC.len()..].to_vec())
}

fn hll_encode(registers: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(HLL_DATA_SIZE);
    data.extend_from_slice(HLL_MAGIC);
    data.extend_from_slice(registers);
    data
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

/// Redis-compatible glob pattern matching.
///
/// Supports `*` (match any sequence), `?` (match one byte),
/// `[abc]` (character class), `[^abc]` (negated class),
/// and `\x` (escape).
fn glob_match(pattern: &[u8], string: &[u8]) -> bool {
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
        EvictionLoopFailure, EvictionLoopStatus, EvictionSafetyGateState, MaxmemoryPressureLevel,
        PttlValue, Store, StoreError, StreamGroupReadCursor, StreamGroupReadOptions, ValueType,
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

    #[test]
    fn set_get_and_del() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        assert_eq!(store.get(b"k", 100).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.del(&[b"k".to_vec()], 100), 1);
        assert_eq!(store.get(b"k", 100).unwrap(), None);
    }

    #[test]
    fn incr_missing_then_existing() {
        let mut store = Store::new();
        assert_eq!(store.incr(b"n", 0).expect("incr"), 1);
        assert_eq!(store.incr(b"n", 0).expect("incr"), 2);
        assert_eq!(store.get(b"n", 0).unwrap(), Some(b"2".to_vec()));
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
    fn expire_at_milliseconds_deletes_when_deadline_not_in_future() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 1_000);
        assert!(store.expire_at_milliseconds(b"k", 1_000, 1_000));
        assert_eq!(store.get(b"k", 1_000).unwrap(), None);
    }

    #[test]
    fn expire_missing_key_returns_false() {
        let mut store = Store::new();
        assert!(!store.expire_seconds(b"missing", 5, 0));
        assert!(!store.expire_milliseconds(b"missing", 5, 0));
        assert!(!store.expire_at_milliseconds(b"missing", 5_000, 0));
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
    }

    #[test]
    fn fr_p2c_008_u001_active_expire_cycle_evicts_expired_keys() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), Some(1), 0);
        store.set(b"b".to_vec(), b"2".to_vec(), Some(1), 0);
        store.set(b"c".to_vec(), b"3".to_vec(), None, 0);

        let result = store.run_active_expire_cycle(10, 0, 10);
        assert_eq!(result.sampled_keys, 3);
        assert_eq!(result.evicted_keys, 2);
        assert_eq!(store.dbsize(10), 1);
        assert_eq!(store.get(b"c", 10).unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn fr_p2c_008_u002_active_expire_cycle_cursor_is_deterministic() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), Some(1), 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), Some(1), 0);
        store.set(b"d".to_vec(), b"4".to_vec(), None, 0);

        let first = store.run_active_expire_cycle(10, 0, 2);
        assert_eq!(first.sampled_keys, 2);
        assert_eq!(first.evicted_keys, 1);
        assert_eq!(first.next_cursor, 1);

        let second = store.run_active_expire_cycle(10, first.next_cursor, 2);
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
        for idx in 0..8 {
            let key = format!("fr:p2c:008:evict:{idx}");
            store.set(key.into_bytes(), vec![b'v'; 32], None, 0);
        }

        let result =
            store.run_bounded_eviction_loop(0, 64, 0, 1, 1, EvictionSafetyGateState::default());
        assert_eq!(result.status, EvictionLoopStatus::Running);
        assert!(result.evicted_keys >= 1);
        assert!(result.bytes_to_free_after > 0);
    }

    #[test]
    fn fr_p2c_008_u012_bounded_eviction_loop_reports_ok_when_pressure_cleared() {
        let mut store = Store::new();
        for idx in 0..6 {
            let key = format!("fr:p2c:008:evict:ok:{idx}");
            store.set(key.into_bytes(), vec![b'v'; 24], None, 0);
        }

        let result =
            store.run_bounded_eviction_loop(0, 64, 0, 2, 16, EvictionSafetyGateState::default());
        assert_eq!(result.status, EvictionLoopStatus::Ok);
        assert!(result.evicted_keys >= 1);
        assert_eq!(result.bytes_to_free_after, 0);
    }

    #[test]
    fn fr_p2c_008_u013_safety_gate_suppresses_eviction() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), vec![b'x'; 96], None, 0);
        store.set(b"b".to_vec(), vec![b'y'; 96], None, 0);
        let before_dbsize = store.dbsize(0);

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
        assert_eq!(store.dbsize(0), before_dbsize);
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
    fn getset_preserves_existing_ttl() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v1".to_vec(), Some(5_000), 1_000);
        assert_eq!(store.pttl(b"k", 1_000), PttlValue::Remaining(5_000));

        assert_eq!(
            store.getset(b"k".to_vec(), b"v2".to_vec(), 2_000).unwrap(),
            Some(b"v1".to_vec())
        );
        assert_eq!(store.get(b"k", 2_000).unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.pttl(b"k", 2_000), PttlValue::Remaining(4_000));
        assert_eq!(store.get(b"k", 6_001).unwrap(), None);
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
        store.set(b"hello".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"hallo".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"world".to_vec(), b"3".to_vec(), None, 0);
        let result = store.keys_matching(b"h?llo", 0);
        assert_eq!(result, vec![b"hallo".to_vec(), b"hello".to_vec()]);
        let result = store.keys_matching(b"*", 0);
        assert_eq!(result.len(), 3);
        let result = store.keys_matching(b"h*", 0);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn keys_matching_malformed_class_contract_matches_redis() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"c".to_vec(), b"3".to_vec(), None, 0);
        store.set(b"[abc".to_vec(), b"1".to_vec(), None, 0);
        // Redis treats malformed "[abc" as a class of bytes {'a','b','c'}.
        assert_eq!(
            store.keys_matching(b"[abc", 0),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        // The malformed class does not match literal '[' prefixed keys.
        assert!(!store.keys_matching(b"[abc", 0).iter().any(|k| k == b"[abc"));
        // "[a-" is malformed too; with this key set Redis matches only 'a'.
        assert_eq!(store.keys_matching(b"[a-", 0), vec![b"a".to_vec()]);
    }

    #[test]
    fn keys_matching_range_and_escape_contract_matches_redis() {
        let mut store = Store::new();
        store.set(b"!".to_vec(), b"0".to_vec(), None, 0);
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"6".to_vec(), None, 0);
        store.set(b"m".to_vec(), b"2".to_vec(), None, 0);
        store.set(b"z".to_vec(), b"3".to_vec(), None, 0);
        store.set(b"-".to_vec(), b"4".to_vec(), None, 0);
        store.set(b"]".to_vec(), b"5".to_vec(), None, 0);

        assert_eq!(
            store.keys_matching(b"[z-a]", 0),
            vec![b"a".to_vec(), b"b".to_vec(), b"m".to_vec(), b"z".to_vec()]
        );
        assert_eq!(store.keys_matching(b"[\\-]", 0), vec![b"-".to_vec()]);
        assert_eq!(
            store.keys_matching(b"[a-]", 0),
            vec![b"]".to_vec(), b"a".to_vec()]
        );
        assert_eq!(
            store.keys_matching(b"[!a]", 0),
            vec![b"!".to_vec(), b"a".to_vec()]
        );
    }

    #[test]
    fn keys_matching_skips_expired_entries() {
        let mut store = Store::new();
        store.set(b"live".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"soon".to_vec(), b"2".to_vec(), Some(50), 0);
        store.set(b"later".to_vec(), b"3".to_vec(), Some(500), 0);

        let result = store.keys_matching(b"*", 100);
        assert_eq!(result, vec![b"later".to_vec(), b"live".to_vec()]);
    }

    #[test]
    fn dbsize_counts_live_keys() {
        let mut store = Store::new();
        store.set(b"a".to_vec(), b"1".to_vec(), None, 0);
        store.set(b"b".to_vec(), b"2".to_vec(), Some(100), 0);
        assert_eq!(store.dbsize(0), 2);
        assert_eq!(store.dbsize(200), 1); // b expired
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

        store.ltrim(b"l", 9, 12, 0).unwrap();
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
        let range = store.zrangebyscore(b"z", 2.0, 3.0, 0).unwrap();
        assert_eq!(range, vec![b"b".to_vec(), b"c".to_vec()]);
        let count = store.zcount(b"z", 2.0, 3.0, 0).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn zincrby_creates_and_increments() {
        let mut store = Store::new();
        let score = store.zincrby(b"z", b"m".to_vec(), 5.0, 0).unwrap();
        assert_eq!(score, 5.0);
        let score = store.zincrby(b"z", b"m".to_vec(), 2.5, 0).unwrap();
        assert_eq!(score, 7.5);
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

        let removed = store.xtrim(b"s", 2, 0).unwrap();
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

        assert_eq!(store.xtrim(b"s", 0, 0).unwrap(), 2);
        assert_eq!(store.xlen(b"s", 0).unwrap(), 0);
        assert_eq!(store.key_type(b"s", 0), Some("stream"));

        assert_eq!(store.xtrim(b"missing", 1, 0).unwrap(), 0);

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(store.xtrim(b"str", 1, 0), Err(StoreError::WrongType));
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
    }

    #[test]
    fn stream_xinfo_missing_and_wrongtype() {
        let mut store = Store::new();
        assert_eq!(store.xinfo_stream(b"missing", 0).unwrap(), None);

        store.set(b"str".to_vec(), b"value".to_vec(), None, 0);
        assert_eq!(store.xinfo_stream(b"str", 0), Err(StoreError::WrongType));
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
        assert_eq!(groups, vec![(b"g1".to_vec(), 1, 0, (0, 0))]);

        assert_eq!(
            store
                .xgroup_delconsumer(b"s", b"g1", b"missing_consumer", 0)
                .unwrap(),
            Some(0)
        );
        let groups = store.xinfo_groups(b"s", 0).unwrap().expect("groups");
        assert_eq!(groups, vec![(b"g1".to_vec(), 1, 0, (0, 0))]);

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
        assert_eq!(consumers, vec![b"c1".to_vec(), b"c2".to_vec()]);

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
        assert_eq!(store.bitcount(b"k", None, None, 0).unwrap(), 8);
    }

    #[test]
    fn bitpos_finds_first_set_bit() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0x00, 0x80], None, 0); // bit 8 set (MSB of byte 1)
        assert_eq!(store.bitpos(b"k", true, None, None, 0).unwrap(), 8);
    }

    #[test]
    fn bitpos_finds_first_clear_bit() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), vec![0xff, 0xff], None, 0); // all bits set
        // Without explicit end, returns position past end
        assert_eq!(store.bitpos(b"k", false, None, None, 0).unwrap(), 16);
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
}
