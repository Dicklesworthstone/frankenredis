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
//! the side indices. Grows at load factor 1 and shrinks under ~10% fill
//! ([`maybe_shrink`](KeyDict::maybe_shrink), Redis's HASHTABLE_MIN_FILL policy) so a
//! keyspace that spikes large then sheds its keys returns the bucket memory — the
//! reverse-binary cursor keeps its no-missed-key guarantee across both grow and
//! shrink (verified by the scan-across-growth and scan-across-shrink tests).
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
        Self::with_capacity(0)
    }

    /// Create a dict sized for `capacity` entries at load factor <= 1.
    ///
    /// This is the bulk-load path needed by the structural `Store.entries`
    /// replacement: it avoids repeated bucket doublings and reserves node arena
    /// slots up front while preserving the same hash, chain, SCAN, and
    /// RANDOMKEY semantics as incremental growth.
    pub fn with_capacity(capacity: usize) -> Self {
        let n = Self::bucket_count_for_capacity(capacity);
        let mut buckets = Vec::with_capacity(n);
        buckets.resize_with(n, || None);
        Self {
            buckets,
            nodes: Vec::with_capacity(capacity),
            free: Vec::new(),
            mask: (n as u64) - 1,
            count: 0,
            hasher: foldhash::quality::RandomState::default(),
        }
    }

    /// Reserve room for at least `additional` more inserts without resizing the
    /// bucket table or growing the live-node arena.
    pub fn reserve(&mut self, additional: usize) {
        let needed = self.count.saturating_add(additional);
        if needed > self.buckets.len() {
            self.resize_buckets(Self::bucket_count_for_capacity(needed));
        }
        self.nodes
            .reserve(additional.saturating_sub(self.free.len()));
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
            let node = self.nodes[idx]
                .as_ref()
                .expect("bucket chain points at live node");
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
            let node = self.nodes[idx]
                .as_ref()
                .expect("bucket chain points at live node");
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
            let node = self.nodes[idx]
                .as_mut()
                .expect("bucket chain points at live node");
            if node.hash == h && *node.key == *key {
                return Some(std::mem::replace(&mut node.value, value));
            }
            cur = node.next;
        }
        // Grow before linking the new node when the insert would exceed load
        // factor 1. That avoids writing a node into the old table only to
        // immediately rebuild its chain in `grow`.
        let b = if self.count == self.buckets.len() {
            self.grow();
            self.bucket_of(h)
        } else {
            b
        };
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
            let node = self.nodes[idx]
                .as_ref()
                .expect("bucket chain points at live node");
            let next = node.next;
            if node.hash == h && *node.key == *key {
                let removed = self.nodes[idx]
                    .take()
                    .expect("bucket chain points at live node");
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
                self.maybe_shrink();
                return Some(removed.value);
            }
            prev = cur;
            cur = next;
        }
        None
    }

    /// Halve the bucket table (repeatedly, to the smallest power-of-two that keeps
    /// the load factor >= ~0.1) once removals leave it under ~10% full — the mirror
    /// of the load-factor-1 doubling in [`insert`], and the same HASHTABLE_MIN_FILL
    /// policy Redis's `dictShrinkIfNeeded` uses. Without this a keyspace that spiked
    /// large and then shed most of its keys would keep the whole grown bucket array
    /// forever (the "grow-only" gap called out in the module header). The 10%-shrink
    /// / 100%-grow watermarks leave a wide stable band [0.1, 1.0], so alternating
    /// insert/remove at a boundary cannot thrash. Shrinking is a plain rehash into a
    /// smaller power-of-two table, so the reverse-binary [`scan`](Self::scan) cursor
    /// keeps its no-missed-key guarantee across the size change exactly as it does
    /// across growth (a stale larger cursor masked by the new smaller mask re-visits
    /// the merged bucket — a permitted duplicate — and never skips).
    fn maybe_shrink(&mut self) {
        if self.buckets.len() <= Self::INITIAL_BUCKETS {
            return;
        }
        // fill < 10% (count*10 < buckets); target = smallest pow2 that fits `count`.
        if self.count.saturating_mul(10) >= self.buckets.len() {
            return;
        }
        let target = Self::bucket_count_for_capacity(self.count);
        if target < self.buckets.len() {
            self.resize_buckets(target);
        }
    }

    /// Double the bucket array and rehash every node into its new home. Power-of-
    /// two growth keeps `hash & mask` stable modulo the new high bit, which is
    /// exactly what the reverse-binary [`scan`](Self::scan) cursor relies on.
    fn grow(&mut self) {
        self.resize_buckets(self.buckets.len() * 2);
    }

    fn bucket_count_for_capacity(capacity: usize) -> usize {
        capacity
            .max(Self::INITIAL_BUCKETS)
            .checked_next_power_of_two()
            .expect("KeyDict capacity is too large")
    }

    fn resize_buckets(&mut self, new_len: usize) {
        debug_assert!(new_len.is_power_of_two());
        // Rehashes every live node into a fresh power-of-two table; works for both
        // growth (grow / reserve) and shrink (maybe_shrink) — `hash & new_mask` is
        // correct for a larger or smaller mask alike. Only a true no-op is skipped.
        if new_len == self.buckets.len() {
            return;
        }
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
                let n = self.nodes[idx]
                    .as_ref()
                    .expect("bucket chain points at live node");
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
                let chain_len = std::iter::successors(Some(head), |&idx| {
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

    /// (uhthd) Quantify the RAM payoff of wiring `KeyDict` as the live keyspace vs
    /// today's three side indices (`hashbrown` entries + `BTreeSet` ordered_keys +
    /// `Vec` random slots), each holding an `Arc<[u8]>` per key. Measured as the
    /// resident-set delta while building N keys, in ONE process, without dropping
    /// either structure (RSS only grows, so the two deltas are clean and additive —
    /// no allocator-retention confound). Run:
    ///   cargo test -p fr-store keydict_vs_side_index_ram_uhthd -- --ignored --nocapture
    #[test]
    #[ignore = "RSS benchmark; run explicitly with --ignored --nocapture"]
    fn keydict_vs_side_index_ram_uhthd() {
        use std::collections::BTreeSet;
        use std::sync::Arc;

        fn rss_bytes() -> usize {
            // /proc/self/statm field 2 = resident pages.
            let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
            let pages: usize = s
                .split_whitespace()
                .nth(1)
                .and_then(|f| f.parse().ok())
                .unwrap_or(0);
            pages * 4096
        }

        let n = 2_000_000usize;
        let mkkey = |i: usize| format!("key:{i:010}").into_bytes().into_boxed_slice();

        let r0 = rss_bytes();
        // Baseline: the three Arc-keyed side indices the live Store keeps today.
        let mut entries: std::collections::HashMap<Arc<[u8]>, ()> =
            std::collections::HashMap::with_capacity(n);
        let mut ordered: BTreeSet<Arc<[u8]>> = BTreeSet::new();
        let mut slots: Vec<Arc<[u8]>> = Vec::with_capacity(n);
        for i in 0..n {
            let key: Arc<[u8]> = Arc::from(mkkey(i));
            entries.insert(Arc::clone(&key), ());
            ordered.insert(Arc::clone(&key));
            slots.push(key);
        }
        let r1 = rss_bytes();
        let base = r1 - r0;

        // Candidate: one KeyDict owning each key once as Box<[u8]> (no Arc header,
        // no side indices — it serves SCAN + RANDOMKEY itself).
        let mut kd: KeyDict<()> = KeyDict::with_capacity(n);
        for i in 0..n {
            kd.insert(mkkey(i), ());
        }
        let r2 = rss_bytes();
        let cand = r2 - r1;

        std::hint::black_box((&entries, &ordered, &slots, &kd));
        let bpp_base = base as f64 / n as f64;
        let bpp_cand = cand as f64 / n as f64;
        println!(
            "KeyDict RAM (N={n}): 3-side-index baseline={:.1}MB ({:.1} B/key) | KeyDict={:.1}MB ({:.1} B/key) | ratio={:.3} (KeyDict uses {:.0}% of baseline, saves {:.1} B/key)",
            base as f64 / 1e6,
            bpp_base,
            cand as f64 / 1e6,
            bpp_cand,
            cand as f64 / base as f64,
            100.0 * cand as f64 / base as f64,
            bpp_base - bpp_cand,
        );
        assert!(
            cand < base,
            "KeyDict must use less RAM than the 3 side indices"
        );
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
    fn presized_bulk_build_avoids_resize_and_preserves_semantics_uhthd() {
        let n = 4096usize;
        let mut d: KeyDict<usize> = KeyDict::with_capacity(n);
        let initial_buckets = d.bucket_count();
        assert!(initial_buckets >= n);
        assert_eq!(d.storage_slots(), 0);

        for i in 0..n {
            assert_eq!(
                d.insert(format!("bulk:{i:04}").into_bytes().into_boxed_slice(), i),
                None
            );
        }
        assert_eq!(d.len(), n);
        assert_eq!(
            d.bucket_count(),
            initial_buckets,
            "presized bulk build should not resize"
        );
        assert_eq!(d.storage_slots(), n);

        for i in 0..n {
            assert_eq!(d.get(format!("bulk:{i:04}").as_bytes()), Some(&i));
        }

        let mut seen = std::collections::HashSet::new();
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 64, |key, value| {
                assert_eq!(d.get(key), Some(value));
                seen.insert(key.to_vec());
            });
            if cursor == 0 {
                break;
            }
        }
        assert_eq!(seen.len(), n);

        let mut rng = Lcg(0x0123_4567_89ab_cdef);
        for _ in 0..10_000 {
            let (key, value) = d.random_sample(|| rng.next()).expect("non-empty");
            assert_eq!(d.get(key), Some(value));
        }

        let mut reserved: KeyDict<usize> = KeyDict::new();
        for i in 0..8usize {
            reserved.insert(format!("warm:{i}").into_bytes().into_boxed_slice(), i);
        }
        reserved.reserve(n);
        let reserved_buckets = reserved.bucket_count();
        for i in 8..(n + 8) {
            reserved.insert(format!("warm:{i}").into_bytes().into_boxed_slice(), i);
        }
        assert_eq!(reserved.bucket_count(), reserved_buckets);
        assert_eq!(reserved.len(), n + 8);
    }

    // (frankenredis-uhthd) Concrete bulk-build timing hook for the KeyDict
    // presize lever. `cargo test -p fr-store keydict_presized_build_bench_uhthd
    // -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn keydict_presized_build_bench_uhthd() {
        use std::time::Instant;

        const N: usize = 200_000;

        let t = Instant::now();
        let mut incremental: KeyDict<usize> = KeyDict::new();
        for i in 0..N {
            incremental.insert(format!("key:{i:08}").into_bytes().into_boxed_slice(), i);
        }
        let incremental_us = t.elapsed().as_secs_f64() * 1e6;

        let t = Instant::now();
        let mut presized: KeyDict<usize> = KeyDict::with_capacity(N);
        for i in 0..N {
            presized.insert(format!("key:{i:08}").into_bytes().into_boxed_slice(), i);
        }
        let presized_us = t.elapsed().as_secs_f64() * 1e6;

        assert_eq!(incremental.len(), N);
        assert_eq!(presized.len(), N);
        assert_eq!(
            incremental.get(b"key:00012345"),
            presized.get(b"key:00012345")
        );
        eprintln!(
            "KeyDict build {N} keys: incremental={incremental_us:.0}us presized={presized_us:.0}us speedup={:.2}x buckets={} storage_slots={}",
            incremental_us / presized_us,
            presized.bucket_count(),
            presized.storage_slots()
        );
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
    fn scan_never_misses_a_present_throughout_key_across_shrink() {
        // The dictScan guarantee must also hold when the table SHRINKS mid-scan:
        // start large, delete most keys during the scan (forcing repeated halvings
        // via maybe_shrink), and assert every key present for the WHOLE scan is
        // still returned at least once. A "keep" set is never deleted; everything
        // else is shed as the scan proceeds.
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..4000u32 {
            d.insert(format!("base{i}").into_bytes().into_boxed_slice(), i);
        }
        let start_buckets = d.bucket_count();
        // keep = present-throughout; never deleted.
        let keep: std::collections::HashSet<Vec<u8>> = (0..200u32)
            .map(|i| format!("base{i}").into_bytes())
            .collect();
        // deletable pool (base200..base3999), removed a chunk per step.
        let mut deletable: Vec<u32> = (200..4000u32).collect();
        let mut rng = Lcg(0x51ed_2718_dead_c0de);
        // shuffle the deletable order (Fisher-Yates via the Lcg).
        for i in (1..deletable.len()).rev() {
            let j = (rng.next() % (i as u64 + 1)) as usize;
            deletable.swap(i, j);
        }
        let mut di = 0usize;
        let mut returned: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut cursor = 0u64;
        let mut step = 0u32;
        loop {
            cursor = d.scan(cursor, 7, |key, _| {
                returned.insert(key.to_vec());
            });
            // Shed ~120 deletable keys per step so the table drops from 4000 to 200
            // over the scan, tripping several shrinks.
            for _ in 0..120 {
                if di < deletable.len() {
                    let victim = format!("base{}", deletable[di]).into_bytes();
                    d.remove(&victim);
                    di += 1;
                }
            }
            step += 1;
            if cursor == 0 {
                break;
            }
            assert!(step < 1_000_000, "scan did not terminate");
        }
        assert!(
            d.bucket_count() < start_buckets,
            "test must actually trigger a shrink (buckets {} -> {})",
            start_buckets,
            d.bucket_count()
        );
        for key in &keep {
            assert!(
                returned.contains(key),
                "present-throughout key {:?} was MISSED by scan across shrink",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn remove_shrinks_table_and_preserves_entries_and_scan() {
        // maybe_shrink: filling then emptying returns bucket memory (buckets shrink
        // back toward INITIAL), all survivors stay reachable, and a full static scan
        // still returns exactly the survivor set.
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..2000u32 {
            d.insert(format!("k{i}").into_bytes().into_boxed_slice(), i);
        }
        let grown = d.bucket_count();
        assert!(grown >= 2000, "should have grown to hold 2000");
        // Delete all but 5 -> fill collapses well under 10% -> shrinks fire.
        for i in 5..2000u32 {
            assert_eq!(d.remove(format!("k{i}").into_bytes().as_slice()), Some(i));
        }
        assert_eq!(d.len(), 5);
        assert!(
            d.bucket_count() < grown,
            "table should have shrunk ({grown} -> {})",
            d.bucket_count()
        );
        // Survivors intact + reachable.
        for i in 0..5u32 {
            assert_eq!(d.get(format!("k{i}").into_bytes().as_slice()), Some(&i));
        }
        // Full scan returns exactly the 5 survivors.
        let mut seen = std::collections::HashSet::new();
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 4, |key, _| {
                seen.insert(key.to_vec());
            });
            if cursor == 0 {
                break;
            }
        }
        let want: std::collections::HashSet<Vec<u8>> =
            (0..5u32).map(|i| format!("k{i}").into_bytes()).collect();
        assert_eq!(
            seen, want,
            "post-shrink scan must return exactly the survivors"
        );
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
