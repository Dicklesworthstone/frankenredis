//! `KeyDict` — a redis-dict-class chaining hash table in 100% safe Rust.
//! (frankenredis-uhthd) **This IS `Store::entries`.**
//!
//! ## Why this exists
//!
//! fr's keyspace was a `hashbrown::HashMap<Box<[u8]>, Entry>`. hashbrown is
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
//! ## Status: WIRED
//!
//! `Store::entries` is a `KeyDict`. Keyspace SCAN runs through [`KeyDict::scan`]
//! and RANDOMKEY through [`KeyDict::random_sample`], and `ordered_keys`,
//! `random_key_slots` and the two SCAN resume caches are deleted. Measured against
//! a live Redis 7.2.4 in one invocation, 1M keys, A/A null 0.9995:
//! **248.1 -> 145.4 bytes/key, 3.0005x -> 1.7612x.**
//!
//! Grows at load factor 1 and shrinks under ~10% fill
//! ([`maybe_shrink`](KeyDict::maybe_shrink), Redis's HASHTABLE_MIN_FILL policy) so a
//! keyspace that spikes large then sheds its keys returns the bucket memory — the
//! reverse-binary cursor keeps its no-missed-key guarantee across both grow and
//! shrink (verified by the scan-across-growth and scan-across-shrink tests).
//!
//! `#![forbid(unsafe_code)]` holds: chaining uses arena indices, not raw links.

use std::hash::BuildHasher;

/// "No node here" for a bucket head or a chain link.
///
/// (uhthd) Arena links are `u32` + sentinel rather than `Option<usize>` because this
/// table's whole purpose is bytes per key. `Option<usize>` is **16 bytes**: `usize` has
/// no spare bit pattern, so the discriminant cannot be packed into it and the compiler
/// adds a whole word. That 16 is paid twice per key — once in `buckets` (load factor 1,
/// so one bucket slot per key) and once in `Node::next` — for a value whose real range
/// is bounded by the node arena.
///
/// The arena is bounded by `u32::MAX - 1` nodes, checked in [`KeyDict::alloc_node`].
/// That is not a practical limit: a node is ~72 bytes plus its key allocation, so
/// 4.29e9 of them is over 300 GB of keyspace index before any values.
const NIL: u32 = u32::MAX;

/// Nodes per [`NodeArena`] chunk. 1024 cells, i.e. a ~72 KB chunk at the live
/// `Node<Entry>` width -- CHOSEN BY MEASUREMENT, not by taste.
///
/// (frankenredis-uhthd) This was 4096 because it looked like a sensible round number
/// when the chunked arena was written. It is not a free choice: RSS on this server is
/// mimalloc's `committed`, and committed is QUANTIZED at the size of the allocation
/// the chunk asks for, so the chunk size is worth several bytes per key by itself.
/// Swept against the live three-server harness at 1M keys, vs Redis 7.2.4:
///
///     shift  cells  chunk    B/key   vs redis   mimalloc committed
///        8     256   18 KB    95.0    1.1500x       94.8 MiB
///        9     512   36 KB    95.4    1.1552x       95.6 MiB
///       10    1024   72 KB    95.2    1.1533x       94.5 MiB   <- here
///       11    2048  144 KB    99.4    1.2043x       97.9 MiB
///       12    4096  288 KB   103.7    1.2559x      102.0 MiB   <- was here
///       13    8192  576 KB   101.0    1.2227x      100.1 MiB
///
/// `committed` tracks B/key row for row, which is what identifies the allocator as the
/// thing being measured rather than anything in this file.
///
/// THE CLIFF IS BETWEEN 72 KB AND 144 KB and the curve is FLAT below it -- 8, 9 and 10
/// are indistinguishable. That places the boundary where mimalloc stops serving an
/// allocation from its normal page path and starts treating it as a large object,
/// which is documented at 128 KB; the cliff's LOCATION is measured, the attribution to
/// that specific threshold is inference from where it sits, and no per-bin allocator
/// stats were read to confirm it (this mimalloc is built without them).
///
/// 10 rather than 8: everything below 11 buys the same RAM, so take the LARGEST chunk
/// in the flat region. That is the fewest chunks, the smallest outer `Vec`, and the
/// fewest allocations, for identical bytes per key. At 1M keys it is 977 chunks, an
/// outer `Vec` of ~16 KB, and the tail wastes at most 1023 cells (0.07 B/key).
///
/// CPU is unchanged, which had to be checked because a smaller chunk means a bigger
/// outer vector on every node access. Callgrind, fixed 20k populate + 20,000 GETs, two
/// runs per arm: shift 12 reads 106,424,590 / 106,390,779 Ir and shift 10 reads
/// 106,369,087 / 106,251,338 -- a 0.09 pct difference against a 0.11 pct run-to-run
/// spread, i.e. inside the noise.
const ARENA_CHUNK_SHIFT: u32 = 10;
const ARENA_CHUNK_LEN: usize = 1 << ARENA_CHUNK_SHIFT;
const ARENA_CHUNK_MASK: usize = ARENA_CHUNK_LEN - 1;

/// The node arena, as FIXED-SIZE CHUNKS that are allocated once and never reallocated.
///
/// (frankenredis-uhthd) This was a single `Vec<Option<Node<V>>>`, which grows by
/// DOUBLING, so its capacity is a power of two while the key count is not. The
/// stranded remainder is charged at the full node width: at 1M keys the arena holds
/// 1,048,576 slots for 1,000,000 live nodes, and 48,576 x 72 B is **3.5 B/key** of
/// reserved-but-never-used arena. That is a best case, not a typical one -- just past
/// a doubling the same policy strands close to HALF the arena, which at this node
/// width is tens of bytes per key.
///
/// THE STRANDED CAPACITY IS THE SMALL HALF. The large one is the DOUBLING GARBAGE:
/// reaching 2^20 slots frees a 37.7 MB region, the growth before it frees 18.9 MB,
/// and so on. Under the server's allocator those freed regions are not returned --
/// they are retained in mimalloc's arena, and the instrument samples RSS immediately
/// after the load, which is when they are all still resident. Chunking never creates
/// them. Measured on a live server, 1M keys, against Redis 7.2.4 in the same
/// invocation: **145.2 -> 115.9 B/key, 1.7580x -> 1.4005x, -29.3 B/key**, three
/// interleaved pairs, every A/A null within 0.9971-1.0009.
///
/// DO NOT TRY TO SEE THIS IN `keydict_byte_attribution_uhthd`. That instrument reads
/// 110.4 B/key resident against 91.7 accounted and shows NO retained block, which is
/// how this lever was very nearly sized at -3.2 B/key and dropped as marginal. The
/// reason is that `#[global_allocator]` lives in `fr-server/src/main.rs` alone, so
/// the fr-store TEST binary runs on system malloc, which munmaps a large free
/// immediately, while the shipping server runs mimalloc, which does not. The
/// instrument is measuring a different allocator from the one that ships, and is
/// blind to every retention effect by construction.
///
/// Chunking removes the growth copy rather than hiding it: a slot index is
/// `(chunk, offset)` by construction, so growth never moves a live node and never
/// touches a byte of one. That also makes the arena index STABLE, which the bucket
/// heads and `next` links already depend on, and it takes an O(n) memcpy off the
/// insert path that crosses a power of two.
///
/// The trade is one extra load per node access -- `chunks[i >> SHIFT][i & MASK]`
/// instead of `nodes[i]` -- and it is NOT free. Measured under callgrind on a fixed
/// 20k-key populate plus 20,000 GETs, two runs per arm, the same job on both
/// (`dbsize` 20000 on every run):
///
///     pre-arena  (37daa064f)  107,480,532 / 107,580,605 Ir
///     chunked    (169d32e68)  108,149,944 / 108,201,231 Ir   +0.60 pct
///
/// The two bases differ in exactly one `.rs` file, this one, so that number is this
/// change and nothing else. 0.60 pct of retired instructions buys -29.2 B/key of
/// keyspace RSS; the trade is worth making and is recorded rather than waved at. An
/// earlier draft of this comment argued the added load would hide in the shadow of
/// the cache miss it precedes -- that was a guess, and the measurement says it costs
/// something real, if small.
///
/// `#![forbid(unsafe_code)]` holds: chunks are ordinary boxed slices.
struct NodeArena<V> {
    chunks: Vec<Box<[Option<Node<V>>]>>,
    /// High-water mark of allocated slots, the analogue of `Vec::len`. Slots at or
    /// above this are `None` and have never been handed out.
    len: usize,
}

impl<V> NodeArena<V> {
    fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// One chunk of `None`s. `repeat_with` reports an exact size hint, so the `Vec`
    /// is allocated at exactly `ARENA_CHUNK_LEN` and `into_boxed_slice` never
    /// reallocates -- a chunk carries no spare capacity.
    fn new_chunk() -> Box<[Option<Node<V>>]> {
        std::iter::repeat_with(|| None)
            .take(ARENA_CHUNK_LEN)
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }

    /// Pre-allocate whole chunks for `capacity` cells. Only ever called before the
    /// first insert (see [`KeyDict::with_capacity`]) or from `reserve`.
    fn reserve_slots(&mut self, capacity: usize) {
        let needed = capacity.div_ceil(ARENA_CHUNK_LEN);
        while self.chunks.len() < needed {
            self.chunks.push(Self::new_chunk());
        }
    }

    #[inline]
    fn slot(&self, idx: u32) -> &Option<Node<V>> {
        let i = idx as usize;
        &self.chunks[i >> ARENA_CHUNK_SHIFT][i & ARENA_CHUNK_MASK]
    }

    #[inline]
    fn slot_mut(&mut self, idx: u32) -> &mut Option<Node<V>> {
        let i = idx as usize;
        &mut self.chunks[i >> ARENA_CHUNK_SHIFT][i & ARENA_CHUNK_MASK]
    }

    #[inline]
    fn get(&self, idx: u32) -> &Node<V> {
        self.slot(idx)
            .as_ref()
            .expect("bucket chain points at live node")
    }

    #[inline]
    fn get_mut(&mut self, idx: u32) -> &mut Node<V> {
        self.slot_mut(idx)
            .as_mut()
            .expect("bucket chain points at live node")
    }

    /// Append a node at the high-water mark, allocating one chunk if it lands past
    /// the last. Returns its stable slot index.
    fn push(&mut self, node: Node<V>) -> u32 {
        let i = self.len;
        if (i >> ARENA_CHUNK_SHIFT) == self.chunks.len() {
            self.chunks.push(Self::new_chunk());
        }
        self.chunks[i >> ARENA_CHUNK_SHIFT][i & ARENA_CHUNK_MASK] = Some(node);
        self.len += 1;
        i as u32
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    /// Slots reserved across all chunks, the analogue of `Vec::capacity`.
    #[inline]
    fn capacity(&self) -> usize {
        self.chunks.len() * ARENA_CHUNK_LEN
    }

    /// Drop every node, keeping the chunks -- `Vec::clear` semantics, which
    /// `KeyDict::clear` relies on to retain its allocation like `HashMap::clear`.
    ///
    /// Only the live prefix is walked, not the whole reserved capacity: slots at or
    /// above the high-water mark are already `None`, and touching them would make
    /// FLUSHALL cost the arena's capacity rather than its length.
    fn clear(&mut self) {
        for (_, slot) in self.slots_mut() {
            *slot = None;
        }
        self.len = 0;
    }

    /// Every live slot below the high-water mark, with its index. Slots at or above
    /// `len` were never handed out and are skipped, so a caller enumerating this sees
    /// exactly what a `Vec<Option<_>>::iter_mut().enumerate()` used to show it.
    fn slots_mut(&mut self) -> impl Iterator<Item = (usize, &mut Option<Node<V>>)> {
        let len = self.len;
        self.chunks
            .iter_mut()
            .flat_map(|chunk| chunk.iter_mut())
            .take(len)
            .enumerate()
    }

    /// Squeeze the `None` holes out, preserving relative order and RENUMBERING every
    /// surviving node to a dense prefix. The caller must rebuild the bucket heads and
    /// `next` links afterwards, because every index it held is now stale.
    fn compact(&mut self) {
        let mut write = 0usize;
        for read in 0..self.len {
            if self.slot(read as u32).is_some() {
                if write != read {
                    let node = self.slot_mut(read as u32).take();
                    *self.slot_mut(write as u32) = node;
                }
                write += 1;
            }
        }
        // Every `Some` now lives below `write`: a slot at or above it either held
        // `None` already or was moved down by the `take` above.
        self.len = write;
    }

    /// Hand back whole chunks that hold nothing. Only chunks entirely above the
    /// high-water mark can go, so this is meaningful after [`Self::compact`].
    fn shrink_to_fit(&mut self) {
        self.chunks.truncate(self.len.div_ceil(ARENA_CHUNK_LEN));
        self.chunks.shrink_to_fit();
    }
}

/// Longest key held inside the node instead of in its own heap block.
///
/// 15 and not 16: [`NodeKey`] is laid out with `Heap`'s pointer as the niche that
/// carries the discriminant, so the `Inline` payload has to fit in the 16 bytes that
/// follow it -- one length byte plus fifteen of key.
const NODE_KEY_INLINE_CAP: usize = 15;

/// The key bytes of one node, inline when short enough.
///
/// (frankenredis-uhthd) A `Box<[u8]>` key costs the node a 16-byte fat pointer AND a
/// separate heap block. Measured on the live 1M-key probe, that block is 9.9 B/key of
/// payload carried in 28.6 B/key of footprint -- the allocator rounds a ~10-byte
/// request up and charges its own per-block overhead -- so the key name costs ~44.6
/// B/key all in, against the ~24 Redis pays for a pointer plus one sds. It is the
/// largest single line left in this table.
///
/// Inlining trades node WIDTH for that block: the node grows 8 bytes (the enum is 24
/// where the fat pointer was 16) and the block disappears entirely for keys of 15
/// bytes or fewer. Keys longer than that keep their block and pay the 8 bytes for
/// nothing, which is the honest cost of this shape and is why a long-key arm is
/// measured alongside the short-key one rather than assumed away.
///
/// MEASURED, and the long-key half did not cost what the arithmetic said. Short keys
/// 115.8 -> 103.2 B/key (1.4032x -> 1.2542x vs live Redis 7.2.4); long keys 130.7 ->
/// 130.6, i.e. NEUTRAL, where node width alone predicted +8 B/key. A three-binary
/// isolation (this shape with inlining forced off) puts node 64 -> 72 at +0.0 B/key
/// across four key counts and node 64 -> 112 at +39.9, so arena residency is simply
/// not linear near 64 bytes. Do not re-derive a width cost by multiplying
/// `size_of::<Node<_>>()` by the key count; it has been wrong every time.
///
/// It is also CHEAPER on CPU, which was not the goal: callgrind on a fixed 20k
/// populate plus 20,000 GETs reads 108,010,679 / 108,088,523 Ir without inlining
/// against 106,530,902 / 106,583,482 with it -- **-1.38 pct**, roughly 19x the
/// instrument's own 0.07 pct run-to-run spread. Reading the key out of the node the
/// hash already pulled in beats chasing a pointer to it.
///
/// Redis keys reach this table DB-ENCODED, but `encode_db_key` returns db 0 keys
/// VERBATIM and only prefixes db != 0, so the common single-database server inlines on
/// the real key length, not on a padded one.
///
/// WHY THE PRIOR REJECT DOES NOT TRANSFER: an inline-small key enum was measured and
/// rejected on 2026-06-20 (keyspace 1.169x -> 1.465x, 6 of 7 RSS cells worse), with the
/// retry gate "not without table-entry-size proof". That was the PRE-KeyDict keyspace,
/// where the key was an `Arc<[u8]>` SHARED by the entries map and three side indices --
/// inlining copied the bytes into every one of them, which is exactly why it lost.
/// Those indices were deleted with the KeyDict wiring and this table owns the key ONCE.
/// The table-entry-size proof the gate asks for is the `==` assertion below.
enum NodeKey {
    Inline { len: u8, bytes: [u8; NODE_KEY_INLINE_CAP] },
    Heap(Box<[u8]>),
}

/// The layout this lever is built on, asserted rather than assumed -- a `<=` budget on
/// `Entry` is what sent the previous pass after a shrink the struct had already made.
const _: () = assert!(std::mem::size_of::<NodeKey>() == 24);

impl NodeKey {
    #[inline]
    fn from_slice(key: &[u8]) -> Self {
        if key.len() <= NODE_KEY_INLINE_CAP {
            let mut bytes = [0u8; NODE_KEY_INLINE_CAP];
            bytes[..key.len()].copy_from_slice(key);
            Self::Inline {
                len: key.len() as u8,
                bytes,
            }
        } else {
            Self::Heap(Box::from(key))
        }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..usize::from(*len)],
            Self::Heap(bytes) => bytes,
        }
    }
}

/// One key/value cell; `next` chains collisions in the same bucket.
struct Node<V> {
    /// Low 32 bits of the key's hash, kept as a cheap pre-filter before the byte
    /// compare and as the rehash input.
    ///
    /// (uhthd) 32 bits suffice for BOTH uses. As a rehash input it is exact: the
    /// bucket index is `hash & mask`, and `mask` can never exceed `u32::MAX`, because
    /// a bucket count above 2^32 would require more than `u32::MAX` nodes, which
    /// [`KeyDict::alloc_node`] refuses. As a pre-filter it is a probabilistic
    /// accelerator, never a decision: a collision here still falls through to the
    /// full `node.key.as_slice() == key` compare, so a 32-bit match cannot return a
    /// wrong key.
    hash: u32,
    key: NodeKey,
    value: V,
    /// Index of the next node in this bucket's chain, or [`NIL`].
    next: u32,
}

/// A chaining hash table keyed by raw bytes, sized to a power of two so the
/// bucket index is `hash & mask` and the [`reverse-binary cursor`](KeyDict::scan)
/// is well-defined.
pub struct KeyDict<V> {
    /// Head node index per bucket, or [`NIL`]. One `u32` per bucket, not one
    /// `Option<usize>` — see [`NIL`].
    buckets: Vec<u32>,
    /// A lossy 64-way fingerprint of the first byte of every key in each hash
    /// bucket. SCAN MATCH with a literal prefix can skip buckets whose bit is
    /// absent while retaining the normal reverse-binary cursor. A collision is
    /// only a false positive (the caller still checks the complete glob), never
    /// a missed key.
    /// SCAN first-byte prefilter, ONE BYTE per bucket.
    ///
    /// (frankenredis-hwcm1 shipped this as `Vec<u64>`; BlackThrush 2026-08-26
    /// narrowed it.) This is a pure RAM/false-positive trade on the keyspace
    /// index, which is the project's most universal loss: the u64 form cost
    /// **8.4 B/key** at load factor ~1 -- its own attribution test says so, and
    /// the ledger row that landed it recorded keyspace moving 1.7612x -> 1.8112x
    /// against live Redis 7.2.4 as a result. One byte per bucket costs 1.05
    /// B/key instead; two bytes per bucket cost 2.1 B/key.
    ///
    /// The fingerprint is a bitmask, so it can only ever RULE A BUCKET OUT --
    /// every surviving candidate is still emitted for the caller's full MATCH
    /// validation, exactly as before. Going from 64 bits to 16 takes the
    /// aliasing from 4-way to 16-way, so a singleton bucket that cannot contain
    /// the wanted byte is now visited with probability 1/16 rather than 1/64 --
    /// still skipping ~94 pct of buckets where a prefilter-free scan visits all
    /// of them.
    ///
    /// 8 bits was tried first and REJECTED by this module's own effectiveness
    /// test: at 8 slots the index can only come from three bits of the byte, and
    /// the corpus that matters here -- ASCII key names -- puts almost none of
    /// its entropy there.
    first_byte_bits: Vec<u16>,
    /// Arena of key/value cells. Removed cells become `None` and their slot is
    /// pushed into `free`, so high-churn workloads do not allocate a fresh node
    /// per insert. This removes the pass226 `Box<Node>` allocation penalty while
    /// keeping key ownership and chain order semantics unchanged.
    ///
    /// `Option<Node<V>>` is the same size as `Node<V>`: `key: Box<[u8]>` is a
    /// non-null pointer, so the `None` discriminant lives in that niche. Pinned by
    /// `node_layout_is_compact_uhthd` so a future field reorder cannot silently add
    /// a word per key.
    ///
    /// Held in fixed-size chunks rather than one doubling `Vec` -- see [`NodeArena`].
    nodes: NodeArena<V>,
    free: Vec<u32>,
    /// `buckets.len() - 1`; bucket index = `hash & mask`.
    mask: u64,
    count: usize,
    /// While set, [`maybe_shrink`](Self::maybe_shrink) is a no-op. Held for the span of
    /// a bulk remove-then-reinsert whose net key count is unchanged, so the table does
    /// not shrink into the drain and grow back out of the refill. See the note on
    /// `maybe_shrink`. (frankenredis-4f8vx)
    shrink_suspended: bool,
    hasher: foldhash::quality::RandomState,
}

impl<V> Default for KeyDict<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// (uhthd) Equality is by CONTENT, deliberately not by layout.
///
/// Two dicts holding the same keys almost never share a bucket layout: each carries
/// its own randomly-seeded hasher, so the same key lands in different buckets and
/// the arenas are ordered by insertion history. A derived `PartialEq` would compare
/// those and report "not equal" for two keyspaces that are observably identical --
/// which is exactly what the `Store` equality assertions in the test suite mean to
/// check.
///
/// This is O(n) probes rather than an O(n) memcmp, which is the price of the
/// randomised seeding that defeats hash flooding.
impl<V: PartialEq> PartialEq for KeyDict<V> {
    fn eq(&self, other: &Self) -> bool {
        if self.count != other.count {
            return false;
        }
        self.iter()
            .all(|(key, value)| other.get(key).is_some_and(|theirs| theirs == value))
    }
}

impl<V: Eq> Eq for KeyDict<V> {}

/// (uhthd) `Store` derives `Debug`, so the keyspace has to be printable. This
/// deliberately prints a SUMMARY rather than the entries: a `Store` debug-print of a
/// million-key keyspace would be unusable, and `Entry` values can hold whole
/// collections. The counts are what anyone reading a `Store` dump actually wants
/// from this field.
impl<V> std::fmt::Debug for KeyDict<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyDict")
            .field("len", &self.count)
            .field("buckets", &self.buckets.len())
            .field("arena_slots", &self.nodes.len())
            .field("free_slots", &self.free.len())
            .finish()
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
        let buckets = vec![NIL; n];
        Self {
            buckets,
            first_byte_bits: vec![0; n],
            nodes: {
                let mut arena = NodeArena::new();
                arena.reserve_slots(capacity);
                arena
            },
            free: Vec::new(),
            mask: (n as u64) - 1,
            count: 0,
            shrink_suspended: false,
            hasher: foldhash::quality::RandomState::default(),
        }
    }

    /// Reserve room for at least `additional` more inserts without resizing the
    /// bucket table or growing the live-node arena.
    ///
    /// (uhthd) Together with `bucket_count`, `storage_slots` and `capacity` this is
    /// the sizing/observability surface: `capacity` is read by `Store`'s
    /// flush-releases-capacity assertions and the other three by this module's own
    /// growth, churn and presized-build tests. None is on a production path, so a
    /// lib-only build sees them as unused.
    #[allow(dead_code)]
    pub fn reserve(&mut self, additional: usize) {
        let needed = self.count.saturating_add(additional);
        if needed > self.buckets.len() {
            self.resize_buckets(Self::bucket_count_for_capacity(needed));
        }
        self.nodes
            .reserve_slots(self.nodes.len() + additional.saturating_sub(self.free.len()));
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
    #[allow(dead_code)]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Number of arena slots allocated for nodes, including free slots retained
    /// for reuse. Exposed for the churn guard; not part of Redis-visible state.
    #[inline]
    #[allow(dead_code)]
    pub fn storage_slots(&self) -> usize {
        self.nodes.len()
    }

    /// Suspend or resume the shrink policy for the span of a BULK operation whose net
    /// key count is unchanged. (frankenredis-4f8vx)
    ///
    /// Resuming does NOT retroactively shrink; call [`Self::shrink_if_sparse`] once
    /// afterwards if the caller genuinely ended smaller than it started. Keeping those
    /// two separate is deliberate: a caller that swaps N keys for N wants neither, and
    /// one that net-removes wants exactly one shrink, not one per removed key.
    #[inline]
    pub fn set_shrink_suspended(&mut self, suspended: bool) {
        self.shrink_suspended = suspended;
    }

    /// Apply the shrink policy once, now. No-op while suspended.
    #[inline]
    pub fn shrink_if_sparse(&mut self) {
        self.maybe_shrink();
    }

    /// Low 32 bits of the key's hash. See [`Node::hash`] for why 32 is exact for
    /// bucket selection and safe for the pre-filter.
    #[inline]
    fn hash_key(&self, key: &[u8]) -> u32 {
        self.hasher.hash_one(key) as u32
    }

    #[inline]
    fn bucket_of(&self, hash: u32) -> usize {
        (u64::from(hash) & self.mask) as usize
    }

    #[inline]
    fn first_byte_bit(key: &[u8]) -> u16 {
        // MIX the byte into the slot index; do not just keep its low bits. The
        // `u64` form used `byte & 63`, which is fine at 64 slots and breaks down
        // as the mask narrows: `byte & 7` keeps only the low three bits, and
        // ASCII keys carry almost no entropy there -- `b'p'` and `b'x'` differ by
        // exactly 8, so they share a slot and the prefilter stops filtering. A
        // Fibonacci multiply spreads all eight bits of the byte over the four
        // index bits, so the 16 slots take 16 byte values each wherever the
        // input's entropy happens to sit.
        let byte = key.first().copied().unwrap_or_default();
        1_u16 << ((u32::from(byte).wrapping_mul(0x9E37_79B1) >> 28) & 15)
    }

    fn rebuild_first_byte_bits(&mut self) {
        for bucket in 0..self.buckets.len() {
            self.rebuild_first_byte_bits_for_bucket(bucket);
        }
    }

    fn rebuild_first_byte_bits_for_bucket(&mut self, bucket: usize) {
        let mut bits = 0;
        let mut node = self.buckets[bucket];
        while node != NIL {
            let current = self.nodes.get(node);
            bits |= Self::first_byte_bit(current.key.as_slice());
            node = current.next;
        }
        self.first_byte_bits[bucket] = bits;
    }

    /// Take a free arena slot or push a new one, returning its index.
    ///
    /// The `u32` arena bound is enforced HERE rather than trusted, because it is what
    /// makes every `NIL`-sentinel link and the 32-bit `Node::hash` sound. Exceeding it
    /// fails loudly instead of aliasing a live node with the sentinel.
    fn alloc_node(&mut self, node: Node<V>) -> u32 {
        if let Some(idx) = self.free.pop() {
            *self.nodes.slot_mut(idx) = Some(node);
            idx
        } else {
            assert!(
                self.nodes.len() < (NIL as usize),
                "KeyDict node arena exceeded u32 addressing ({} slots)",
                self.nodes.len()
            );
            self.nodes.push(node)
        }
    }

    /// Borrow the value for `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        let h = self.hash_key(key);
        let mut cur = self.buckets[self.bucket_of(h)];
        while cur != NIL {
            let node = self.nodes.get(cur);
            if node.hash == h && node.key.as_slice() == key {
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
        while cur != NIL {
            let node = self.nodes.get(cur);
            if node.hash == h && node.key.as_slice() == key {
                return Some(&mut self.nodes.get_mut(cur).value);
            }
            cur = node.next;
        }
        None
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Borrow the STORED key bytes alongside the value.
    ///
    /// (uhthd) The `Store` wiring needs this because several side maps
    /// (`expiry_deadlines`, `hash_field_expires`, `volatile_keys`) are keyed by the
    /// canonical key rather than by the caller's argv slice, and the caller's slice
    /// is a borrow of a network buffer that does not outlive the command. Returning
    /// the dict's own copy is what lets those maps be built without re-deriving the
    /// key from the request. Mirrors `HashMap::get_key_value`.
    pub fn get_key_value(&self, key: &[u8]) -> Option<(&[u8], &V)> {
        let h = self.hash_key(key);
        let mut cur = self.buckets[self.bucket_of(h)];
        while cur != NIL {
            let node = self.nodes.get(cur);
            if node.hash == h && node.key.as_slice() == key {
                return Some((node.key.as_slice(), &node.value));
            }
            cur = node.next;
        }
        None
    }

    /// Node-arena slots reserved, the analogue of `HashMap::capacity`.
    ///
    /// (uhthd) Reported off the ARENA and not off the bucket array because it is the
    /// arena that holds the keys and values, and because `release_empty_keyspace_capacity`
    /// asserts this reaches exactly 0 after a flush — a bucket-derived figure could
    /// not, since the table keeps its four-slot floor.
    #[inline]
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.nodes.capacity()
    }

    /// Release memory the live entries do not need: compact the arena, shrink the
    /// bucket table to the smallest power of two that still holds `count`, and hand
    /// back both Vecs' spare capacity.
    ///
    /// (uhthd) `Store::release_empty_keyspace_capacity` runs this after a flush and
    /// asserts `capacity() == 0`, so a plain `Vec::shrink_to_fit` on `nodes` is NOT
    /// enough: removal leaves `None` holes and `nodes.len()` stays at the high-water
    /// mark, so `shrink_to_fit` would only drop capacity BEYOND that mark and a
    /// flushed keyspace would keep its whole arena forever. The holes are therefore
    /// compacted out first.
    ///
    /// Compaction RENUMBERS nodes, which invalidates every bucket head and `next`
    /// link, so the bucket table is rebuilt from the surviving nodes rather than
    /// patched — the same rehash `resize_buckets` performs, and equally safe for the
    /// reverse-binary cursor, which derives everything from `hash & mask`.
    pub fn shrink_to_fit(&mut self) {
        if !self.free.is_empty() {
            self.nodes.compact();
            self.free.clear();
            self.free.shrink_to_fit();
            debug_assert_eq!(self.nodes.len(), self.count);
            // Every index moved, so relink from scratch rather than patching.
            let mask = self.mask;
            self.buckets.fill(NIL);
            for (idx, node) in self.nodes.slots_mut() {
                let node = node.as_mut().expect("holes were just compacted out");
                let b = (u64::from(node.hash) & mask) as usize;
                node.next = self.buckets[b];
                self.buckets[b] = idx as u32;
            }
            self.rebuild_first_byte_bits();
        }
        self.nodes.shrink_to_fit();
        let target = Self::bucket_count_for_capacity(self.count);
        if target < self.buckets.len() {
            self.resize_buckets(target);
        }
        self.buckets.shrink_to_fit();
        self.first_byte_bits.shrink_to_fit();
    }

    /// Insert `key`/`value`, returning the previous value if the key existed.
    /// The key bytes are owned once, inline in the node when short enough
    /// ([`NodeKey`]), with no `Arc` header and no separate block.
    ///
    /// (frankenredis-uhthd) TAKES A BORROW, and that is load-bearing rather than
    /// stylistic. This read `key: Box<[u8]>`, so the caller allocated a block and
    /// handed it over; an inline key would then COPY out of that block and drop it,
    /// paying for the allocation anyway and adding a copy. That is the exact shape
    /// that made the earlier key-arena attempt measure a LOSS, and its retry predicate
    /// says so: retry only with the caller-side allocation removed. `Store`'s single
    /// insert site now passes a slice and `store_key_from_slice` is gone from it.
    ///
    /// `AsRef<[u8]>` rather than `&[u8]` so the existing owned-key callers in tests
    /// still compile unchanged; only the production path had an allocation to lose.
    pub fn insert(&mut self, key: impl AsRef<[u8]>, value: V) -> Option<V> {
        let key = key.as_ref();
        let h = self.hash_key(key);
        let b = self.bucket_of(h);
        // Overwrite in place if present.
        let mut cur = self.buckets[b];
        while cur != NIL {
            let node = self.nodes.get_mut(cur);
            if node.hash == h && node.key.as_slice() == key {
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
            key: NodeKey::from_slice(key),
            value,
            next: head,
        });
        self.buckets[b] = idx;
        self.first_byte_bits[b] |= Self::first_byte_bit(self.nodes.get(idx).key.as_slice());
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
        let mut prev = NIL;
        let mut cur = self.buckets[b];
        while cur != NIL {
            let node = self.nodes.get(cur);
            let next = node.next;
            if node.hash == h && node.key.as_slice() == key {
                let removed = self
                    .nodes
                    .slot_mut(cur)
                    .take()
                    .expect("bucket chain points at live node");
                if prev != NIL {
                    self.nodes.get_mut(prev).next = removed.next;
                } else {
                    self.buckets[b] = removed.next;
                }
                self.free.push(cur);
                self.count -= 1;
                self.rebuild_first_byte_bits_for_bucket(b);
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
        // (frankenredis-4f8vx) A BULK operation that removes N keys and puts N back has
        // a net-zero effect on the table, but the removal half drives `count` toward
        // zero first, so the shrink policy fires, halves the table repeatedly, and then
        // the insert half grows it back through the same doublings. Every one of those
        // resizes rehashes every surviving node.
        //
        // SWAPDB is exactly that shape: it drains both databases and refills them.
        // Measured with callgrind on a SWAPDB-only workload at 1,000 keys/DB,
        // `resize_buckets` was 5.39 pct of the whole profile -- a frame that should be
        // ~0 when the key count never actually changes.
        if self.shrink_suspended {
            return;
        }
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
        let mut buckets: Vec<u32> = vec![NIL; new_len];
        for (idx, node) in self.nodes.slots_mut() {
            if let Some(node) = node {
                let b = (u64::from(node.hash) & new_mask) as usize;
                node.next = buckets[b];
                buckets[b] = idx as u32;
            }
        }
        self.buckets = buckets;
        self.first_byte_bits = vec![0; new_len];
        self.mask = new_mask;
        self.rebuild_first_byte_bits();
    }

    /// Remove all entries (keeps the allocated bucket array, like `HashMap::clear`).
    pub fn clear(&mut self) {
        self.buckets.fill(NIL);
        self.first_byte_bits.fill(0);
        self.nodes.clear();
        self.free.clear();
        self.count = 0;
    }

    /// Iterate all (key, value) pairs in unspecified order.
    pub fn iter(&self) -> KeyDictIter<'_, V> {
        KeyDictIter {
            dict: self,
            bucket: 0,
            current: NIL,
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
            while node != NIL {
                let n = self.nodes.get(node);
                emit(n.key.as_slice(), &n.value);
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

    /// The same reverse-binary SCAN as [`Self::scan`], except hash buckets that
    /// cannot contain `first_byte` are skipped. The fingerprint has deliberate
    /// 64-way collisions, so it only rules buckets out; every candidate is still
    /// emitted for the caller's full MATCH validation.
    pub fn scan_first_byte<F: FnMut(&[u8], &V)>(
        &self,
        cursor: u64,
        count: usize,
        first_byte: u8,
        mut emit: F,
    ) -> u64 {
        let wanted = Self::first_byte_bit(&[first_byte]);
        let mut v = cursor;
        let mut emitted = 0usize;
        loop {
            let b = (v & self.mask) as usize;
            if self.first_byte_bits[b] & wanted != 0 {
                let mut node = self.buckets[b];
                while node != NIL {
                    let n = self.nodes.get(node);
                    emit(n.key.as_slice(), &n.value);
                    emitted += 1;
                    node = n.next;
                }
            }
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
        // Walk a chain as an iterator of arena indices, terminating on the NIL
        // sentinel. `successors` needs an `Option`, so the sentinel is mapped to
        // `None` here rather than being represented as one in the node.
        let chain = |head: u32| {
            std::iter::successors(Some(head), |&idx| match self.nodes.get(idx).next {
                NIL => None,
                next => Some(next),
            })
        };
        for _ in 0..64 {
            let b = reduce(next_rand(), nb);
            let head = self.buckets[b];
            if head != NIL {
                let chain_len = chain(head).count();
                let pick = reduce(next_rand(), chain_len);
                let chosen = chain(head).nth(pick).expect("pick is within chain length");
                let chosen = self.nodes.get(chosen);
                return Some((chosen.key.as_slice(), &chosen.value));
            }
        }
        // Fallback: first non-empty bucket from a random origin.
        let start = reduce(next_rand(), nb);
        for i in 0..self.buckets.len() {
            let b = (start + i) % self.buckets.len();
            let head = self.buckets[b];
            if head != NIL {
                let head = self.nodes.get(head);
                return Some((head.key.as_slice(), &head.value));
            }
        }
        None
    }
}

/// Iterator over live `KeyDict` entries in bucket/chain order.
pub struct KeyDictIter<'a, V> {
    dict: &'a KeyDict<V>,
    bucket: usize,
    current: u32,
}

impl<'a, V> Iterator for KeyDictIter<'a, V> {
    type Item = (&'a [u8], &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current != NIL {
                let node = self.dict.nodes.get(self.current);
                self.current = node.next;
                return Some((node.key.as_slice(), &node.value));
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
    /// (frankenredis-uhthd) WHERE THE REMAINING BYTES/KEY GO.
    ///
    /// The wired keyspace measures ~149.5 B/key against Redis 7.2.4's ~82.6. Three
    /// candidate levers were each worth "about 15 B/key" by arithmetic -- a key
    /// arena, an inline small value, a denser node -- which is not a basis for
    /// choosing between them. This attributes real resident bytes to components so
    /// the next lever is picked on measurement.
    ///
    /// It has already earned its place twice: it priced `frankenredis-hwcm1`'s SCAN
    /// prefilter at 8.4 B/key (one `u64` per bucket at load factor ~1, which nobody
    /// had costed in bytes/key), and it is what exposed the key-arena rejection --
    /// see the REJECT row, and note the trap below.
    ///
    /// **`key BYTES` is PAYLOAD, not footprint.** It sums `k.len()`, so per-key
    /// allocator overhead lands in UNACCOUNTED and arena capacity slack would not
    /// appear at all. Reading UNACCOUNTED as "what an arena would save" is exactly
    /// the mistake that made the arena look like a 21.7 B/key win in-process while
    /// it measured a 6.5 B/key LOSS against a live server.
    #[test]
    #[ignore = "RSS attribution; run explicitly with --ignored --nocapture"]
    fn keydict_byte_attribution_uhthd() {
        use std::mem::size_of;

        fn rss_bytes() -> usize {
            let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
            let pages: usize = s
                .split_whitespace()
                .nth(1)
                .and_then(|f| f.parse().ok())
                .unwrap_or(0);
            pages * 4096
        }

        // A 40-byte payload stands in for `Store`'s `Entry`, which IS 40 (pinned by
        // a const assert next to its definition). This read `[u64; 6]` -- 48 -- for a
        // long time, and an 8-byte-wide stand-in overstates the node arena by 8 B/key,
        // which is one whole lever's worth on the line this instrument exists to rank.
        #[derive(Clone)]
        struct EntrySized([u64; 5]);

        let n = 1_000_000usize;
        let r0 = rss_bytes();
        let mut kd: KeyDict<EntrySized> = KeyDict::new();
        for i in 0..n {
            // Same shape DEBUG POPULATE uses: key:N
            kd.insert(
                format!("key:{i}").into_bytes().into_boxed_slice(),
                EntrySized([0; 5]),
            );
        }
        let r1 = rss_bytes();
        let resident = r1 - r0;

        let node = size_of::<Option<Node<EntrySized>>>();
        let nodes_bytes = kd.nodes.capacity() * node;
        let buckets_bytes = kd.buckets.capacity() * size_of::<u32>();
        let free_bytes = kd.free.capacity() * size_of::<u32>();
        // (frankenredis-hwcm1) One u64 per BUCKET for the SCAN first-byte prefilter.
        // Counted explicitly because at load factor ~1 that is 8 bytes per KEY --
        // the same order as the levers this attribution ranks.
        let first_byte_bits_bytes = kd.first_byte_bits.capacity() * size_of::<u16>();
        let key_bytes: usize = kd.iter().map(|(k, _)| k.len()).sum();
        let accounted =
            nodes_bytes + buckets_bytes + free_bytes + first_byte_bits_bytes + key_bytes;

        let per = |b: usize| b as f64 / n as f64;
        println!(
            "KeyDict byte attribution (N={n}, 40-byte value, key:N keys)\n  resident            {:8.1} B/key  ({:.1} MB)\n  node arena          {:8.1} B/key  (cap {} x {} B/node)\n  bucket table        {:8.1} B/key  (cap {})\n  free list           {:8.1} B/key\n  SCAN first-byte     {:8.1} B/key  (hwcm1 prefilter, 2 B/bucket)\n  key PAYLOAD         {:8.1} B/key  (k.len() only -- NOT footprint)\n  accounted           {:8.1} B/key\n  UNACCOUNTED         {:8.1} B/key  <- per-allocation overhead on {} key blocks",
            per(resident),
            resident as f64 / 1e6,
            per(nodes_bytes),
            kd.nodes.capacity(),
            node,
            per(buckets_bytes),
            kd.buckets.capacity(),
            per(free_bytes),
            per(first_byte_bits_bytes),
            per(key_bytes),
            per(accounted),
            per(resident.saturating_sub(accounted)),
            n,
        );
        std::hint::black_box(&kd);
        assert!(resident > 0, "no resident growth measured");
    }

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

    /// (uhthd) The per-key byte cost IS the feature here, so the layout that
    /// produces it is asserted rather than assumed.
    ///
    /// Three separate properties, each of which a plausible edit would silently
    /// break:
    ///
    /// 1. **A bucket slot is 4 bytes.** At load factor 1 there is one bucket per
    ///    key, so widening this to `Option<usize>` costs 12 B/key on its own.
    /// 2. **`Option<Node<V>>` is the same size as `Node<V>`.** The arena stores
    ///    `Option`s so removal can vacate a slot; that is free only while
    ///    `key: Box<[u8]>` donates its non-null niche. Replacing the key with a
    ///    niche-less representation (a raw index pair, say) would add a word to
    ///    every node without touching any line that looks like a size decision.
    /// 3. **A node carrying a 48-byte value fits in 72 bytes.** 4 (hash) + 16
    ///    (key) + 48 (value) + 4 (next) with no padding. This is the number the
    ///    RAM claim is built from; `<=` rather than `==` so a smaller future
    ///    layout is not a test failure.
    ///
    /// A negative control is built in: the assertions are written against a
    /// concrete 48-byte payload, so a regression to `Option<usize>` links or a
    /// lost niche fails here, not merely in an RSS benchmark that nobody runs.
    #[test]
    fn node_layout_is_compact_uhthd() {
        use std::mem::size_of;

        /// Stand-in for `Store`'s `Entry`, which is pinned at EXACTLY 40 bytes by a
        /// const assert beside its definition. The payload exists to give the type its
        /// size; nothing reads it.
        ///
        /// This was `[u64; 6]` against a `<= 48` budget on `Entry`. Both numbers were
        /// loose in the same direction, so the node came out 72 here while the real
        /// `Node<Entry>` is 64 -- an 8 B/key overstatement on the single line this
        /// module exists to keep honest. Pinned with `==`, not `<=`, for that reason.
        #[allow(dead_code)]
        struct EntrySized([u64; 5]);
        assert_eq!(size_of::<EntrySized>(), 40);

        assert_eq!(size_of::<u32>(), 4, "bucket slot must stay 4 bytes");
        // NESTED NICHE, and this is the assertion that makes the inline-key lever pay
        // rather than cost. `NodeKey` already spends the `Heap(Box<[u8]>)` pointer's
        // null value on its OWN discriminant. If the outer `Option` could not find a
        // second niche in that same pointer it would add a whole word to every node,
        // turning a -20.6 B/key lever into a -12.6 one silently.
        assert_eq!(
            size_of::<Option<Node<EntrySized>>>(),
            size_of::<Node<EntrySized>>(),
            "arena Option must ride the NodeKey pointer niche, not add a word per key"
        );
        assert_eq!(size_of::<NodeKey>(), 24, "len 1 + 15 inline, or a 16-byte Box");
        assert_eq!(
            size_of::<Node<EntrySized>>(),
            72,
            "hash 4 + next 4 + key NodeKey 24 + value 40; a change here is bytes per key"
        );
    }

    /// (uhthd) The 32-bit `Node::hash` is a pre-filter, never a decision: two keys
    /// whose stored hashes are equal must still be told apart by the byte compare.
    ///
    /// Constructing a genuine 32-bit collision would require inverting foldhash, so
    /// this drives the property from the other side — force every key into ONE
    /// bucket by keeping the table at its 4-bucket minimum is not possible either
    /// (it grows), so instead insert enough keys that chains are exercised and
    /// assert every single key still resolves to its own value. A `hash`-only
    /// match that skipped `*node.key == *key` would return a neighbour's value for
    /// at least one of these.
    #[test]
    fn chain_lookup_distinguishes_keys_not_just_hashes_uhthd() {
        let n = 20_000usize;
        let mut d: KeyDict<usize> = KeyDict::new();
        for i in 0..n {
            d.insert(format!("key:{i:08}").into_bytes().into_boxed_slice(), i);
        }
        assert_eq!(d.len(), n);
        for i in 0..n {
            assert_eq!(
                d.get(format!("key:{i:08}").as_bytes()),
                Some(&i),
                "key {i} resolved to the wrong node"
            );
        }
        // Absent keys that share the live keys' shape must still miss.
        for i in n..n + 500 {
            assert_eq!(d.get(format!("key:{i:08}").as_bytes()), None);
        }
    }

    /// (uhthd) `Store::release_empty_keyspace_capacity` asserts `capacity() == 0`
    /// after a flush, so this pins the property that assertion depends on — AND the
    /// case that makes it non-trivial.
    ///
    /// The negative case is the churned dict: `remove` leaves a `None` hole and
    /// `nodes.len()` stays at the high-water mark, so an implementation that only
    /// called `Vec::shrink_to_fit` would drop capacity beyond the mark and leave a
    /// flushed keyspace holding its entire arena forever. Building 5,000 keys,
    /// deleting them all, and demanding 0 is exactly that mistake's shape.
    #[test]
    fn shrink_to_fit_releases_a_flushed_keyspace_uhthd() {
        let mut d: KeyDict<u64> = KeyDict::with_capacity(5_000);
        for i in 0..5_000u64 {
            d.insert(format!("key:{i}").into_bytes().into_boxed_slice(), i);
        }
        assert!(d.capacity() >= 5_000);

        // Remove one at a time (the churn path that leaves holes), not clear().
        for i in 0..5_000u64 {
            assert_eq!(d.remove(format!("key:{i}").as_bytes()), Some(i));
        }
        assert_eq!(d.len(), 0);
        d.shrink_to_fit();
        assert_eq!(d.capacity(), 0, "a flushed keyspace must release its arena");

        // And the dict is still usable afterwards.
        d.insert(k("after"), 7);
        assert_eq!(d.get(b"after"), Some(&7));
    }

    /// (uhthd) Compaction RENUMBERS every node, which invalidates every bucket head
    /// and `next` link. This is the test that a half-done implementation fails: it
    /// shrinks a dict that is still HOLDING data behind holes, then demands that
    /// every survivor is still reachable by lookup, by iteration, and by a full
    /// SCAN. Forgetting to rebuild the bucket table leaves survivors linked by stale
    /// indices — lookups miss, or worse, return a neighbour.
    #[test]
    fn shrink_to_fit_preserves_every_surviving_key_uhthd() {
        let mut d: KeyDict<u64> = KeyDict::new();
        for i in 0..4_000u64 {
            d.insert(format!("key:{i}").into_bytes().into_boxed_slice(), i);
        }
        // Punch holes: drop every third key, so `free` is non-empty and the arena
        // is fragmented rather than merely over-allocated.
        let mut survivors: Vec<u64> = Vec::new();
        for i in 0..4_000u64 {
            if i % 3 == 0 {
                assert_eq!(d.remove(format!("key:{i}").as_bytes()), Some(i));
            } else {
                survivors.push(i);
            }
        }
        assert_eq!(d.len(), survivors.len());

        d.shrink_to_fit();

        assert_eq!(d.len(), survivors.len(), "shrink must not lose entries");
        for &i in &survivors {
            assert_eq!(
                d.get(format!("key:{i}").as_bytes()),
                Some(&i),
                "key {i} unreachable after compaction"
            );
        }
        for i in (0..4_000u64).filter(|i| i % 3 == 0) {
            assert_eq!(
                d.get(format!("key:{i}").as_bytes()),
                None,
                "removed key {i} came back after compaction"
            );
        }
        // Iteration and a full SCAN must both see exactly the survivors.
        let mut iterated: Vec<u64> = d.iter().map(|(_, v)| *v).collect();
        iterated.sort_unstable();
        assert_eq!(iterated, survivors);

        let mut scanned: Vec<u64> = Vec::new();
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 32, |_, v| scanned.push(*v));
            if cursor == 0 {
                break;
            }
        }
        scanned.sort_unstable();
        scanned.dedup();
        assert_eq!(scanned, survivors, "SCAN must still reach every survivor");
    }

    /// (uhthd) `get_key_value` must hand back the dict's OWN key bytes, not the
    /// lookup slice — that is the whole reason the `Store` wiring needs it, since
    /// the lookup slice borrows a network buffer that does not outlive the command.
    /// Probing with a separately-allocated equal slice and asserting the returned
    /// bytes are equal but at a DIFFERENT address is what distinguishes a real
    /// implementation from one that echoes its argument back.
    #[test]
    fn get_key_value_returns_the_stored_key_not_the_probe_uhthd() {
        let mut d: KeyDict<u32> = KeyDict::new();
        d.insert(k("alpha"), 1);

        let probe: Vec<u8> = b"alpha".to_vec();
        let (stored, value) = d.get_key_value(&probe).expect("present");
        assert_eq!(stored, b"alpha");
        assert_eq!(value, &1);
        assert_ne!(
            stored.as_ptr(),
            probe.as_ptr(),
            "must return the dict's own key, not the caller's slice"
        );
        assert_eq!(d.get_key_value(b"absent"), None);
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
    fn scan_first_byte_skips_irrelevant_buckets_without_missing_prefix_keys_hwcm1() {
        let mut dict: KeyDict<u32> = KeyDict::with_capacity(10_128);
        for i in 0..10_000 {
            dict.insert(format!("x-noise:{i:05}").into_bytes().into_boxed_slice(), i);
        }
        for i in 0..128 {
            dict.insert(
                format!("p-match:{i:03}").into_bytes().into_boxed_slice(),
                10_000 + i,
            );
        }

        let mut cursor = 0;
        let mut seen = Vec::new();
        let mut examined = 0usize;
        loop {
            cursor = dict.scan_first_byte(cursor, 11, b'p', |key, value| {
                examined += 1;
                if key.starts_with(b"p-match:") {
                    seen.push(*value);
                }
            });
            if cursor == 0 {
                break;
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, (10_000..10_128).collect::<Vec<_>>());
        assert!(
            examined < 1_024,
            "must skip almost all x-noise buckets, examined {examined} keys"
        );
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

    /// (frankenredis-uhthd) COMPACTION ACROSS A CHUNK BOUNDARY.
    ///
    /// The node arena is fixed-size chunks, so a slot index is `(i >> SHIFT, i & MASK)`
    /// and `compact` moves survivors DOWN across chunk boundaries -- from chunk 2 into
    /// chunk 0 -- before `shrink_to_fit` hands whole chunks back. Every index the
    /// bucket heads and `next` links hold is stale afterwards.
    ///
    /// `shrink_to_fit_preserves_every_surviving_key_uhthd` already covers renumbering,
    /// but it uses 4,000 keys, which is UNDER `ARENA_CHUNK_LEN` -- it lives entirely in
    /// chunk 0 and therefore cannot see a boundary bug at all. This one is deliberately
    /// sized past three chunks and asserts that it really got there, so it cannot
    /// silently decay into the single-chunk case the other test already covers.
    ///
    /// The wrong implementations this fails: reading a moved node through the read
    /// index instead of the write index; allocating the next chunk one push late (which
    /// corrupts exactly slot `ARENA_CHUNK_LEN`); truncating chunks before compacting;
    /// and enumerating `slots_mut` over chunk CAPACITY rather than the live prefix,
    /// which renumbers survivors past the holes at the end of every chunk.
    #[test]
    fn compaction_moves_survivors_across_chunk_boundaries_uhthd() {
        const N: u32 = (ARENA_CHUNK_LEN as u32) * 3 + 517;
        let mut d: KeyDict<u32> = KeyDict::new();
        for i in 0..N {
            d.insert(format!("key:{i:07}").into_bytes().into_boxed_slice(), i);
        }
        assert!(
            d.storage_slots() > ARENA_CHUNK_LEN,
            "ANTI-VACUITY: this test is only meaningful if the arena spans several \
             chunks, but it holds {} slots against a {ARENA_CHUNK_LEN}-slot chunk",
            d.storage_slots()
        );

        // Keep one key in every 7. The survivors are spread over every chunk, so
        // compaction has to pull them down across boundaries rather than within one.
        let survives = |i: u32| i % 7 == 0;
        for i in 0..N {
            if !survives(i) {
                assert_eq!(d.remove(format!("key:{i:07}").as_bytes()), Some(i));
            }
        }
        let expected: Vec<u32> = (0..N).filter(|i| survives(*i)).collect();
        assert_eq!(d.len(), expected.len());

        d.shrink_to_fit();

        // 1. Every survivor is still reachable by lookup, and nothing else is.
        for i in 0..N {
            let got = d.get(format!("key:{i:07}").as_bytes()).copied();
            assert_eq!(
                got,
                survives(i).then_some(i),
                "key:{i:07} after cross-chunk compaction"
            );
        }
        // 2. And by iteration -- which walks the bucket chains, so a stale `next`
        //    link shows up here even when `get` happens to land right.
        let mut seen: Vec<u32> = d.iter().map(|(_, v)| *v).collect();
        seen.sort_unstable();
        assert_eq!(seen, expected, "iteration after cross-chunk compaction");
        // 3. And by a full reverse-binary SCAN.
        let mut scanned: Vec<u32> = Vec::new();
        let mut cursor = 0u64;
        loop {
            cursor = d.scan(cursor, 16, |_, v| scanned.push(*v));
            if cursor == 0 {
                break;
            }
        }
        scanned.sort_unstable();
        assert_eq!(scanned, expected, "SCAN after cross-chunk compaction");

        // 4. The chunks the survivors vacated were actually handed back: the arena
        //    keeps only the chunks its dense prefix needs.
        assert_eq!(
            d.capacity(),
            expected.len().div_ceil(ARENA_CHUNK_LEN) * ARENA_CHUNK_LEN,
            "shrink_to_fit must release whole vacated chunks"
        );

        // 5. The dict still accepts new keys, reusing the compacted arena.
        d.insert(k("after"), 12345);
        assert_eq!(d.get(b"after"), Some(&12345));
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
