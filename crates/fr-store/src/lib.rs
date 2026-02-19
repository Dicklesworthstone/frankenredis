#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};

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
        }
    }
}

#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<Vec<u8>, Entry>,
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
        match entry.expires_at_ms {
            None => PttlValue::NoExpiry,
            Some(expires_at_ms) => {
                if expires_at_ms <= now_ms {
                    self.entries.remove(key);
                    PttlValue::KeyMissing
                } else {
                    let remain = expires_at_ms.saturating_sub(now_ms);
                    let remain = i64::try_from(remain).unwrap_or(i64::MAX);
                    PttlValue::Remaining(remain)
                }
            }
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
        let entry = self.entries.remove(key).unwrap();
        match entry.value {
            Value::String(v) => Ok(Some(v)),
            _ => unreachable!(),
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
        })
    }

    #[must_use]
    pub fn key_type(&mut self, key: &[u8], now_ms: u64) -> Option<&'static str> {
        self.value_type(key, now_ms).map(ValueType::as_str)
    }

    pub fn rename(&mut self, key: &[u8], newkey: &[u8], now_ms: u64) -> Result<(), StoreError> {
        self.drop_if_expired(key, now_ms);
        let entry = self.entries.remove(key).ok_or(StoreError::KeyNotFound)?;
        self.entries.remove(newkey);
        self.entries.insert(newkey.to_vec(), entry);
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
        self.entries.insert(newkey.to_vec(), entry);
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

    pub fn flushdb(&mut self) {
        self.entries.clear();
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

    // ── Sorted Set (ZSet) operations ─────────────────────────────

    /// Add members with scores. Returns the number of *new* members added.
    pub fn zadd(
        &mut self,
        key: &[u8],
        members: &[(f64, Vec<u8>)],
        now_ms: u64,
    ) -> Result<usize, StoreError> {
        self.drop_if_expired(key, now_ms);
        let zs = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => zs,
                _ => return Err(StoreError::WrongType),
            },
            None => {
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::SortedSet(HashMap::new()),
                        expires_at_ms: None,
                    },
                );
                match &mut self.entries.get_mut(key).unwrap().value {
                    Value::SortedSet(zs) => zs,
                    _ => unreachable!(),
                }
            }
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
        let zs = match self.entries.get_mut(key) {
            Some(entry) => match &mut entry.value {
                Value::SortedSet(zs) => zs,
                _ => return Err(StoreError::WrongType),
            },
            None => {
                self.entries.insert(
                    key.to_vec(),
                    Entry {
                        value: Value::SortedSet(HashMap::new()),
                        expires_at_ms: None,
                    },
                );
                match &mut self.entries.get_mut(key).unwrap().value {
                    Value::SortedSet(zs) => zs,
                    _ => unreachable!(),
                }
            }
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
            .map(|(m, s)| (m.clone(), *s))
            .unwrap();
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
            .map(|(m, s)| (m.clone(), *s))
            .unwrap();
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
        let expired = self
            .entries
            .get(key)
            .and_then(|entry| entry.expires_at_ms)
            .is_some_and(|expires_at_ms| expires_at_ms <= now_ms);
        if expired {
            self.entries.remove(key);
        }
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
            }
            let expiry_bytes = entry.expires_at_ms.unwrap_or(0).to_le_bytes();
            hash = fnv1a_update(hash, &expiry_bytes);
        }
        format!("{hash:016x}")
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
    use super::{PttlValue, Store, StoreError, ValueType};

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
