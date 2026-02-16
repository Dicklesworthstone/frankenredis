#![forbid(unsafe_code)]

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    ValueNotInteger,
    IntegerOverflow,
    KeyNotFound,
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

    pub fn append(&mut self, key: &[u8], value: &[u8], now_ms: u64) -> usize {
        self.drop_if_expired(key, now_ms);
        if let Some(entry) = self.entries.get_mut(key) {
            entry.value.extend_from_slice(value);
            entry.value.len()
        } else {
            let len = value.len();
            self.entries.insert(
                key.to_vec(),
                Entry {
                    value: value.to_vec(),
                    expires_at_ms: None,
                },
            );
            len
        }
    }

    #[must_use]
    pub fn strlen(&mut self, key: &[u8], now_ms: u64) -> usize {
        self.drop_if_expired(key, now_ms);
        self.entries.get(key).map_or(0, |entry| entry.value.len())
    }

    #[must_use]
    pub fn mget(&mut self, keys: &[&[u8]], now_ms: u64) -> Vec<Option<Vec<u8>>> {
        keys.iter()
            .map(|key| {
                self.drop_if_expired(key, now_ms);
                self.entries.get(*key).map(|entry| entry.value.clone())
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
                value,
                expires_at_ms: None,
            },
        );
        true
    }

    pub fn getset(&mut self, key: Vec<u8>, value: Vec<u8>, now_ms: u64) -> Option<Vec<u8>> {
        self.drop_if_expired(&key, now_ms);
        let old = self.entries.get(&key).map(|entry| entry.value.clone());
        self.entries.insert(
            key,
            Entry {
                value,
                expires_at_ms: None,
            },
        );
        old
    }

    pub fn incrby(&mut self, key: &[u8], delta: i64, now_ms: u64) -> Result<i64, StoreError> {
        self.drop_if_expired(key, now_ms);
        let (current, expires_at_ms) = match self.entries.get(key) {
            Some(entry) => (parse_i64(&entry.value)?, entry.expires_at_ms),
            None => (0_i64, None),
        };
        let next = current
            .checked_add(delta)
            .ok_or(StoreError::IntegerOverflow)?;
        self.entries.insert(
            key.to_vec(),
            Entry {
                value: next.to_string().into_bytes(),
                expires_at_ms,
            },
        );
        Ok(next)
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
    pub fn key_type(&mut self, key: &[u8], now_ms: u64) -> Option<&'static str> {
        self.drop_if_expired(key, now_ms);
        if self.entries.contains_key(key) {
            Some("string")
        } else {
            None
        }
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
        let entry = self.entries.remove(key).expect("checked above");
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

/// Redis-compatible glob pattern matching.
///
/// Supports `*` (match any sequence), `?` (match one byte),
/// `[abc]` (character class), `[^abc]` / `[!abc]` (negated class),
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
    if i >= pattern.len() {
        return None;
    }

    let negate = pattern[i] == b'^' || pattern[i] == b'!';
    if negate {
        i += 1;
    }

    let mut matched = false;
    let start = i;

    while i < pattern.len() && (pattern[i] != b']' || i == start) {
        if i + 2 < pattern.len() && pattern[i + 1] == b'-' {
            // Range: a-z.
            let lo = pattern[i];
            let hi = pattern[i + 2];
            if ch >= lo && ch <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if pattern[i] == ch {
                matched = true;
            }
            i += 1;
        }
    }

    if i >= pattern.len() {
        return None; // Malformed: no closing bracket.
    }

    // i is now at the closing ']'.
    let result = if negate { !matched } else { matched };
    Some((result, i + 1))
}

#[cfg(test)]
mod tests {
    use super::{PttlValue, Store, StoreError};

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
        assert_eq!(store.get(b"k", 1_000), None);
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
        assert_eq!(store.append(b"k", b"hello", 0), 5);
        assert_eq!(store.append(b"k", b" world", 0), 11);
        assert_eq!(store.get(b"k", 0), Some(b"hello world".to_vec()));
    }

    #[test]
    fn strlen_returns_length_or_zero() {
        let mut store = Store::new();
        assert_eq!(store.strlen(b"missing", 0), 0);
        store.set(b"k".to_vec(), b"hello".to_vec(), None, 0);
        assert_eq!(store.strlen(b"k", 0), 5);
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
        assert_eq!(store.get(b"k", 0), Some(b"v1".to_vec()));
    }

    #[test]
    fn getset_returns_old_and_sets_new() {
        let mut store = Store::new();
        assert_eq!(store.getset(b"k".to_vec(), b"v1".to_vec(), 0), None);
        assert_eq!(
            store.getset(b"k".to_vec(), b"v2".to_vec(), 0),
            Some(b"v1".to_vec())
        );
        assert_eq!(store.get(b"k", 0), Some(b"v2".to_vec()));
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
    fn rename_moves_key() {
        let mut store = Store::new();
        store.set(b"old".to_vec(), b"v".to_vec(), None, 0);
        store.rename(b"old", b"new", 0).expect("rename");
        assert_eq!(store.get(b"old", 0), None);
        assert_eq!(store.get(b"new", 0), Some(b"v".to_vec()));
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
        assert_eq!(store.get(b"a", 0), Some(b"1".to_vec()));
        assert!(store.renamenx(b"a", b"c", 0).expect("renamenx"));
        assert_eq!(store.get(b"a", 0), None);
        assert_eq!(store.get(b"c", 0), Some(b"1".to_vec()));
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
    }
}
