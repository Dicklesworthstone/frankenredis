#![forbid(unsafe_code)]

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    ValueNotInteger,
    IntegerOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    value: Vec<u8>,
    expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttlValue {
    KeyMissing,
    NoExpiry,
    Remaining(i64),
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

    pub fn get(&mut self, key: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        self.drop_if_expired(key, now_ms);
        self.entries.get(key).map(|entry| entry.value.clone())
    }

    pub fn set(&mut self, key: Vec<u8>, value: Vec<u8>, px_ttl_ms: Option<u64>, now_ms: u64) {
        let expires_at_ms = px_ttl_ms.map(|ttl| now_ms.saturating_add(ttl));
        self.entries.insert(
            key,
            Entry {
                value,
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
            Some(entry) => (parse_i64(&entry.value)?, entry.expires_at_ms),
            None => (0_i64, None),
        };
        let next = current.checked_add(1).ok_or(StoreError::IntegerOverflow)?;
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: next.to_string().into_bytes(),
                expires_at_ms,
            },
        );
        Ok(next)
    }

    pub fn expire_seconds(&mut self, key: &[u8], seconds: i64, now_ms: u64) -> bool {
        self.drop_if_expired(key, now_ms);
        if !self.entries.contains_key(key) {
            return false;
        }
        if seconds <= 0 {
            self.entries.remove(key);
            return true;
        }
        let ttl_ms = seconds
            .checked_mul(1000)
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(u64::MAX);
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
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
            hash = fnv1a_update(hash, &entry.value);
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

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{PttlValue, Store};

    #[test]
    fn set_get_and_del() {
        let mut store = Store::new();
        store.set(b"k".to_vec(), b"v".to_vec(), None, 100);
        assert_eq!(store.get(b"k", 100), Some(b"v".to_vec()));
        assert_eq!(store.del(&[b"k".to_vec()], 100), 1);
        assert_eq!(store.get(b"k", 100), None);
    }

    #[test]
    fn incr_missing_then_existing() {
        let mut store = Store::new();
        assert_eq!(store.incr(b"n", 0).expect("incr"), 1);
        assert_eq!(store.incr(b"n", 0).expect("incr"), 2);
        assert_eq!(store.get(b"n", 0), Some(b"2".to_vec()));
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
}
