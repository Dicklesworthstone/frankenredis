//! `KeyDict` — a redis-dict-class chaining hash table in 100% safe Rust.
//! (frankenredis-uhthd, step 1)
//!
//! ## Why this exists
//!
//! fr's keyspace is a `hashbrown::HashMap<Arc<[u8]>, Entry>`. hashbrown is
//! open-addressing, which gives compact storage but provides **neither** of the
//! two capabilities a Redis keyspace needs natively:
//!
//!   1. a **resumable cursor SCAN** that tolerates concurrent rehash, and
//!   2. **O(1) uniform-ish random sampling** (RANDOMKEY / eviction).
//!
//! So fr bolts on side indices — `ordered_keys: BTreeSet<Arc<[u8]>>` (SCAN/KEYS)
//! and `random_key_slots: Vec<Vec<Arc<[u8]>>>` (RANDOMKEY) — each of which keeps
//! a *second* `Arc<[u8]>` copy of every key plus its own structure overhead.
//! That redundancy is the bulk of the keyspace's ~4.5x RAM vs Redis.
//!
//! Redis avoids it with a **chaining** dict: every key lives in exactly one
//! bucket (`hash & mask`), so (a) a deletion never moves another key — slot
//! positions are stable — which lets the **reverse-binary cursor** of
//! `dictScan` walk buckets without missing any key that is present for the whole
//! scan, even across a table doubling; and (b) RANDOMKEY just picks a random
//! bucket. This module reimplements that, owning each key once as a `Box<[u8]>`
//! (no refcount header, no `Arc` sharing), so that when it replaces `entries` it
//! also deletes `ordered_keys` + `random_key_slots` wholesale.
//!
//! ## Status
//!
//! Step 1 = this self-contained, exhaustively-tested primitive (NOT yet wired
//! into `Store`). Step 2 = swap it in for `entries`, route SCAN through
//! [`KeyDict::scan`] and RANDOMKEY through [`KeyDict::random_sample`], and delete
//! the side indices. Grow-only for now (Redis also shrinks; a later step can add
//! it — the reverse-binary cursor handles size *changes*, and grow-only is the
//! conservative subset that never drops a present-throughout key).
//!
//! `#![forbid(unsafe_code)]` holds: chaining uses arena indices, not raw links.

use std::hash::BuildHasher;

/// One key/value cell; `next` chains collisions in the same bucket.
struct Node<V> {
    hash: u64,
    key: Box<[u8]>,
    value: V,
    next: Option<usize>,
}

/// A chaining hash table keyed by raw bytes, sized to a power of two so the
/// bucket index is `hash & mask` and the [`reverse-binary cursor`](KeyDict::scan)
/// is well-defined.
pub struct KeyDict<V> {
    buckets: Vec<Option<usize>>,
    /// Arena of key/value cells. Removed cells become `None` and their slot is
    /// pushed into `free`, so high-churn workloads do not allocate a fresh node
    /// per insert. This removes the pass226 `Box<Node>` allocation penalty while
    /// keeping key ownership and chain order semantics unchanged.
    nodes: Vec<Option<Node<V>>>,
    free: Vec<usize>,
    /// `buckets.len() - 1`; bucket index = `hash & mask`.
    mask: u64,
    count: usize,
    hasher: foldhash::quality::RandomState,
}

impl<V> Default for KeyDict<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// Reverse all 64 bits of `v` (MSB<->LSB). The cursor advance below works in the
/// reversed-bit domain so that the walk is robust to the table doubling.
#[inline]
fn reverse_bits_u64(v: u64) -> u64 {
    v.reverse_bits()
}

impl<V> KeyDict<V> {
    /// Smallest table: 4 buckets (mask 0b11). Grows by doubling.
    const INITIAL_BUCKETS: usize = 4;

    pub fn new() -> Self {
        let n = Self::INITIAL_BUCKETS;
        let mut buckets = Vec::with_capacity(n);
        buckets.resize_with(n, || None);
        Self {
            buckets,
            nodes: Vec::new(),
            free: Vec::new(),
            mask: (n as u64) - 1,
            count: 0,
            hasher: foldhash::quality::RandomState::default(),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Number of buckets (power of two). Exposed for tests / sizing.
    #[inline]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Number of arena slots allocated for nodes, including free slots retained
    /// for reuse. Exposed for the churn guard; not part of Redis-visible state.
    #[inline]
    pub fn storage_slots(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    fn hash_key(&self, key: &[u8]) -> u64 {
        self.hasher.hash_one(key)
    }

    #[inline]
    fn bucket_of(&self, hash: u64) -> usize {
        (hash & self.mask) as usize
    }

    fn alloc_node(&mut self, node: Node<V>) -> usize {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = Some(node);
            idx
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        }
    }

    /// Borrow the value for `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        let h = self.hash_key(key);
        let mut cur = self.buckets[self.bucket_of(h)];
        while let Some(idx) = cur {
            let node = self.nodes[idx].as_ref().expect("bucket chain points at live node");
            if node.hash == h && *node.key == *key {
                return Some(&node.value);
            }
            cur = node.next;
        }
        None
    }

    /// Mutably borrow the value for `key`, or `None`.
    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        let h = self.hash_key(key);
        let b = self.bucket_of(h);
        let mut cur = self.buckets[b];
        while let Some(idx) = cur {
            let node = self.nodes[idx].as_ref().expect("bucket chain points at live node");
            if node.hash == h && *node.key == *key {
                return Some(
                    &mut self.nodes[idx]
                        .as_mut()
                        .expect("bucket chain points at live node")
                        .value,
                );
            }
            cur = node.next;
        }
        None
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Insert `key`/`value`, returning the previous value if the key existed.
    /// The key bytes are owned once (`Box<[u8]>`), with no `Arc` header.
    pub fn insert(&mut self, key: Box<[u8]>, value: V) -> Option<V> {
        let h = self.hash_key(&key);
        let b = self.bucket_of(h);
        // Overwrite in place if present.
        let mut cur = self.buckets[b];
        while let Some(idx) = cur {
            let node = self.nodes[idx].as_mut().expect("bucket chain points at live node");
            if node.hash == h && *node.key == *key {
                return Some(std::mem::replace(&mut node.value, value));
            }
            cur = node.next;
        }
        // Prepend a fresh node (head insertion; order within a bucket is not
        // observable — SCAN emits whole buckets).
        let head = self.buckets[b];
        let idx = self.alloc_node(Node {
            hash: h,
            key,
            value,
            next: head,
        });
        self.buckets[b] = Some(idx);
        self.count += 1;
        if self.count > self.buckets.len() {
            self.grow();
        }
        None
    }

    /// Remove `key`, returning its value if present.
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let h = self.hash_key(key);
        let b = self.bucket_of(h);
        let mut prev: Option<usize> = None;
        let mut cur = self.buckets[b];
        while let Some(idx) = cur {
            let node = self.nodes[idx].as_ref().expect("bucket chain points at live node");
            let next = node.next;
            if node.hash == h && *node.key == *key {
                let removed = self.nodes[idx].take().expect("bucket chain points at live node");
                if let Some(prev_idx) = prev {
                    self.nodes[prev_idx]
                        .as_mut()
                        .expect("bucket chain points at live node")
                        .next = removed.next;
                } else {
                    self.buckets[b] = removed.next;
                }
                self.free.push(idx);
                self.count -= 1;
                return Some(removed.value);
            }
            prev = cur;
            cur = next;
        }
        None
    }

    /// Double the bucket array and rehash every node into its new home. Power-of-
    /// two growth keeps `hash & mask` stable modulo the new high bit, which is
    /// exactly what the reverse-binary [`scan`](Self::scan) cursor relies on.
    fn grow(&mut self) {
        let new_len = self.buckets.len() * 2;
        let new_mask = (new_len as u64) - 1;
        let mut buckets: Vec<Option<usize>> = Vec::with_capacity(new_len);
        buckets.resize_with(new_len, || None);
        for (idx, node) in self.nodes.iter_mut().enumerate() {
            if let Some(node) = node {
                let b = (node.hash & new_mask) as usize;
                node.next = buckets[b];
                buckets[b] = Some(idx);
            }
        }
        self.buckets = buckets;
        self.mask = new_mask;
    }

    /// Remove all entries (keeps the allocated bucket array, like `HashMap::clear`).
    pub fn clear(&mut self) {
        for b in &mut self.buckets {
            *b = None;
        }
        self.nodes.clear();
        self.free.clear();
        self.count = 0;
    }

    /// Iterate all (key, value) pairs in unspecified order.
    pub fn iter(&self) -> KeyDictIter<'_, V> {
        KeyDictIter {
            dict: self,
            bucket: 0,
            current: None,
        }
    }

    /// Iterate keys in unspecified order.
    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(|(k, _)| k)
    }

    /// One step of a Redis-style `SCAN`. Starting from `cursor` (0 begins a
    /// fresh scan), emit whole buckets via `emit` until at least `count`
    /// elements have been produced (or the table is exhausted), and return the
    /// next cursor — `0` means the scan is complete.
    ///
    /// Guarantee: any key that is present for the entire duration of a full scan
    /// (cursor 0 → returned 0) is emitted at least once, even if the table grows
    /// (doubles) between steps. Keys inserted or deleted mid-scan may or may not
    /// appear. This is the `dictScan` reverse-binary-cursor contract.
    pub fn scan<F: FnMut(&[u8], &V)>(&self, cursor: u64, count: usize, mut emit: F) -> u64 {
        let mut v = cursor;
        let mut emitted = 0usize;
        loop {
            let b = (v & self.mask) as usize;
            let mut node = self.buckets[b];
            while let Some(idx) = node {
                let n = self.nodes[idx].as_ref().expect("bucket chain points at live node");
                emit(&n.key, &n.value);
                emitted += 1;
                node = n.next;
            }
            // Reverse-binary increment within the current mask.
            v |= !self.mask;
            v = reverse_bits_u64(v);
            v = v.wrapping_add(1);
            v = reverse_bits_u64(v);
            if v == 0 || emitted >= count.max(1) {
                return v;
            }
        }
    }

    /// Sample a roughly-uniform random key/value. `next_rand` supplies raw u64
    /// entropy (the caller threads its own PRNG, keeping this borrow-free).
    /// Picks a random bucket and, if non-empty, a random element of its chain —
    /// the same mild short-chain bias as Redis `dictGetRandomKey`, which is fine
    /// for RANDOMKEY/eviction sampling. Returns `None` only when empty.
    pub fn random_sample<R: FnMut() -> u64>(&self, mut next_rand: R) -> Option<(&[u8], &V)> {
        if self.count == 0 {
            return None;
        }
        let nb = self.buckets.len();
        // Map raw entropy to a bucket via Lemire's multiply-reduce — `(rand * nb)
        // >> 64` — which keys off the HIGH bits. A plain `rand % nb` with a
        // power-of-two `nb` would use only the low bits, which are weak in the
        // LCG-style PRNGs both the tests and the Store thread in; that biases
        // coverage badly. Multiply-reduce is uniform and low-bit-agnostic.
        let reduce = |r: u64, n: usize| -> usize { ((r as u128 * n as u128) >> 64) as usize };
        // Bounded retries: with load factor <= 1 a random bucket is non-empty
        // with decent probability; cap attempts then fall back to a linear scan
        // from a random origin so we always return in O(buckets) worst case.
        for _ in 0..64 {
            let b = reduce(next_rand(), nb);
            if let Some(head) = self.buckets[b] {
                let chain_len =
                    std::iter::successors(Some(head), |&idx| {
                        self.nodes[idx]
                            .as_ref()
                            .expect("bucket chain points at live node")
                            .next
                    })
                    .count();
                let pick = reduce(next_rand(), chain_len);
                let chosen = std::iter::successors(Some(head), |&idx| {
                    self.nodes[idx]
                        .as_ref()
                        .expect("bucket chain points at live node")
                        .next
                })
                    .nth(pick)
                    .unwrap();
                let chosen = self.nodes[chosen]
                    .as_ref()
                    .expect("bucket chain points at live node");
                return Some((&chosen.key, &chosen.value));
            }
        }
        // Fallback: first non-empty bucket from a random origin.
        let start = reduce(next_rand(), nb);
        for i in 0..self.buckets.len() {
            let b = (start + i) % self.buckets.len();
            if let Some(head) = self.buckets[b] {
                let head = self.nodes[head]
                    .as_ref()
                    .expect("bucket chain points at live node");
                return Some((&head.key, &head.value));
            }
        }
        None
    }
}

/// Iterator over live `KeyDict` entries in bucket/chain order.
pub struct KeyDictIter<'a, V> {
    dict: &'a KeyDict<V>,
    bucket: usize,
    current: Option<usize>,
}

impl<'a, V> Iterator for KeyDictIter<'a, V> {
    type Item = (&'a [u8], &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(idx) = self.current {
                let node = self.dict.nodes[idx]
                    .as_ref()
                    .expect("bucket chain points at live node");
                self.current = node.next;
                return Some((&node.key, &node.value));
            }
            if self.bucket >= self.dict.buckets.len() {
                return None;
            }
            self.current = self.dict.buckets[self.bucket];
            self.bucket += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Box<[u8]> {
        s.as_bytes().to_vec().into_boxed_slice()
    }

    // Small deterministic LCG so tests are reproducible without rand crates and
    // without the harness-forbidden Math.random equivalent.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    #[test]
    fn basic_insert_get_remove_overwrite() {
        let mut d: KeyDict<i32> = KeyDict::new();
        assert!(d.is_empty());
        assert_eq!(d.insert(k("a"), 1), None);
        assert_eq!(d.insert(k("b"), 2), None);
        assert_eq!(d.len(), 2);
        assert_eq!(d.get(b"a"), Some(&1));
        assert_eq!(d.get(b"b"), Some(&2));
        assert_eq!(d.get(b"missing"), None);
        assert!(d.contains_key(b"a"));
        // Overwrite returns the old value, count unchanged.
        assert_eq!(d.insert(k("a"), 10), Some(1));
        assert_eq!(d.len(), 2);
        assert_eq!(d.get(b"a"), Some(&10));
        *d.get_mut(b"b").unwrap() += 100;
        assert_eq!(d.get(b"b"), Some(&102));
        // Remove returns value; missing remove is None.
        assert_eq!(d.remove(b"a"), Some(10));
        assert_eq!(d.remove(b"a"), None);
        assert_eq!(d.len(), 1);
        assert!(!d.contains_key(b"a"));
    }

    #[test]
    fn grow_preserves_all_entries() {
        let mut d: KeyDict<u64> = KeyDict::new();
        let n = 20_000u64;
        for i in 0..n {
            d.insert(format!("key:{i:08}").into_bytes().into_boxed_slice(), i);
        }
        assert_eq!(d.len(), n as usize);
        assert!(d.bucket_count() >= n as usize); // grew past load factor 1
        for i in 0..n {
            assert_eq!(
                d.get(format!("key:{i:08}").as_bytes()),
                Some(&i),
                "lost key {i}"
            );
        }
        // Remove a scattered third; the rest must survive.
        for i in (0..n).step_by(3) {
            assert_eq!(d.remove(format!("key:{i:08}").as_bytes()), Some(i));
        }
        for i in 0..n {
            let want = if i % 3 == 0 { None } else { Some(&i) };
            assert_eq!(
                d.get(format!("key:{i:08}").as_bytes()),
                want,
                "key {i} after churn"
            );
        }
    }

    #[test]
    fn arena_slots_are_reused_after_removal_uhthd() {
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..1024u32 {
            d.insert(format!("hot:{i:04}").into_bytes().into_boxed_slice(), i);
        }
        let high_water = d.storage_slots();
        assert_eq!(high_water, 1024);

        for i in 0..1024u32 {
            assert_eq!(d.remove(format!("hot:{i:04}").as_bytes()), Some(i));
        }
        assert_eq!(d.len(), 0);
        assert_eq!(
            d.storage_slots(),
            high_water,
            "removing nodes should retain slots for reuse instead of freeing the arena"
        );

        for i in 0..1024u32 {
            d.insert(format!("new:{i:04}").into_bytes().into_boxed_slice(), i + 1);
        }
        assert_eq!(d.len(), 1024);
        assert_eq!(
            d.storage_slots(),
            high_water,
            "re-inserting after churn should recycle free node slots"
        );
        for i in 0..1024u32 {
            assert_eq!(d.get(format!("new:{i:04}").as_bytes()), Some(&(i + 1)));
        }
    }

    #[test]
    fn full_scan_returns_exact_keyset() {
        let mut d: KeyDict<u32> = KeyDict::new();
        let mut expect = std::collections::HashSet::new();
        for i in 0..5000u32 {
            d.insert(format!("k{i}").into_bytes().into_boxed_slice(), i);
            expect.insert(format!("k{i}").into_bytes());
        }
        // A full scan with no mutation returns every key exactly once.
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 10, |key, _| seen.push(key.to_vec()));
            if cursor == 0 {
                break;
            }
        }
        let seen_set: std::collections::HashSet<Vec<u8>> = seen.iter().cloned().collect();
        assert_eq!(seen_set, expect, "scan keyset mismatch");
        assert_eq!(seen.len(), expect.len(), "stable scan must not duplicate");
    }

    #[test]
    fn scan_never_misses_a_present_throughout_key_across_growth() {
        // The dictScan guarantee: a key present for the WHOLE scan is returned at
        // least once even if the table doubles mid-scan. Drive growth by inserting
        // during the scan, and delete some keys; assert every key that started in
        // the dict and was never deleted is in the returned set.
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..2000u32 {
            d.insert(format!("base{i}").into_bytes().into_boxed_slice(), i);
        }
        // "stable" = keys present at scan start; we remove from it on deletion.
        let mut stable: std::collections::HashSet<Vec<u8>> = (0..2000u32)
            .map(|i| format!("base{i}").into_bytes())
            .collect();
        let mut returned: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut rng = Lcg(0x1234_5678_9abc_def0);
        let mut next_new = 0u32;
        let mut step = 0u32;
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 7, |key, _| {
                returned.insert(key.to_vec());
            });
            // Mutate between steps: insert a couple new keys (forces growth) and
            // delete a base key (must be honoured by `stable`).
            for _ in 0..3 {
                let nk = format!("new{next_new}").into_bytes().into_boxed_slice();
                d.insert(nk, next_new);
                next_new += 1;
            }
            let victim_i = (rng.next() % 2000) as u32;
            let victim = format!("base{victim_i}").into_bytes();
            if d.remove(&victim).is_some() {
                stable.remove(&victim);
            }
            step += 1;
            if cursor == 0 {
                break;
            }
            assert!(step < 100_000, "scan did not terminate");
        }
        for key in &stable {
            assert!(
                returned.contains(key),
                "present-throughout key {:?} was MISSED by scan across growth",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn random_sample_is_valid_and_reaches_every_key() {
        let mut d: KeyDict<u32> = KeyDict::new();
        let n = 500u32;
        for i in 0..n {
            d.insert(format!("m{i}").into_bytes().into_boxed_slice(), i);
        }
        let mut rng = Lcg(0xdead_beef_cafe_babe);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200_000 {
            let (key, val) = d.random_sample(|| rng.next()).expect("non-empty");
            // Every sample is a live key with its correct value.
            assert_eq!(d.get(key), Some(val));
            seen.insert(key.to_vec());
        }
        assert_eq!(seen.len(), n as usize, "random_sample must reach every key");
        // Empty dict samples to None.
        let mut empty: KeyDict<u32> = KeyDict::new();
        assert!(empty.random_sample(|| 0).is_none());
        empty.insert(k("only"), 1);
        assert_eq!(
            empty.random_sample(|| 0).map(|(key, _)| key.to_vec()),
            Some(b"only".to_vec())
        );
    }

    #[test]
    fn clear_and_iter() {
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..100u32 {
            d.insert(format!("i{i}").into_bytes().into_boxed_slice(), i);
        }
        let collected: std::collections::HashSet<u32> = d.iter().map(|(_, v)| *v).collect();
        assert_eq!(collected.len(), 100);
        assert_eq!(d.keys().count(), 100);
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.iter().count(), 0);
        assert_eq!(d.get(b"i0"), None);
    }
}
