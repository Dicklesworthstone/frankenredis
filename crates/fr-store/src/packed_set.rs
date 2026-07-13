//! Packed string-set encoding — the succinct, listpack-style representation for
//! SMALL generic (non-integer) sets that sit behind `OBJECT ENCODING listpack`.
//!
//! fr currently stores such sets in an `IndexSet<Vec<u8>>`: one heap block plus a
//! hash-table slot *per member*. Redis instead packs a small set into a single
//! contiguous listpack buffer (one allocation, cache-friendly linear scan), only
//! promoting to a hash table at the `set-max-listpack-entries` / `-value`
//! threshold. fr already does the integer case (`SetValue::Int` = sorted
//! `Vec<i64>`); this is the string analogue (frankenredis-9mh3o).
//!
//! STEP 1 (this file): the primitive + an `IndexSet`-equivalence proof. Wiring it
//! into `SetValue` (SADD/SREM/SISMEMBER/SMEMBERS/…) is a mechanical follow-up to
//! be done when fr-store is not being concurrently edited. Behaviour is identical
//! to an insertion-ordered `IndexSet`: dedup on insert, iteration in insertion
//! order, removal preserves the order of the survivors — so SMEMBERS/SSCAN/SPOP
//! output is byte-for-byte unchanged.

/// A set of byte-string members packed into one buffer as a sequence of
/// `[varint length][raw bytes]` records, in insertion order.
///
/// Membership and removal are an O(n) linear scan, which is the correct trade
/// below the listpack→hashtable threshold (n ≤ 128, members ≤ 64 bytes): the
/// whole set is one allocation walked linearly in cache, versus n pointer
/// chases into separately-allocated `Vec`s plus hash-table overhead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackedStrSet {
    buf: Vec<u8>,
    len: usize,
}

impl PackedStrSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Size of the packed payload in bytes (varint headers + member bytes).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }

    /// Iterate members in insertion order (matches `IndexSet` iteration).
    #[must_use]
    pub fn iter(&self) -> PackedStrSetIter<'_> {
        PackedStrSetIter {
            buf: &self.buf,
            pos: 0,
        }
    }

    #[must_use]
    pub fn contains(&self, member: &[u8]) -> bool {
        self.iter().any(|m| m == member)
    }

    /// Insert `member`; returns `true` if it was newly added, `false` if it was
    /// already present (matching `IndexSet::insert`).
    pub fn insert(&mut self, member: &[u8]) -> bool {
        if self.contains(member) {
            return false;
        }
        write_varint(&mut self.buf, member.len());
        self.buf.extend_from_slice(member);
        self.len += 1;
        true
    }

    /// Append `member` WITHOUT the duplicate-scan `insert` performs — the caller
    /// guarantees `member` is not already present (bulk RDB/build path). O(member.len())
    /// per call versus `insert`'s O(n) `contains` scan, so building from N unique
    /// members is O(total bytes) instead of O(n²).
    pub fn append(&mut self, member: &[u8]) {
        write_varint(&mut self.buf, member.len());
        self.buf.extend_from_slice(member);
        self.len += 1;
    }

    /// Remove `member`; returns `true` if it was present. Survivors keep their
    /// relative (insertion) order.
    pub fn remove(&mut self, member: &[u8]) -> bool {
        let mut pos = 0;
        while pos < self.buf.len() {
            let (mlen, data_start) = read_varint(&self.buf, pos);
            let data_end = data_start + mlen;
            if self.buf[data_start..data_end] == *member {
                self.buf.drain(pos..data_end);
                self.len -= 1;
                return true;
            }
            pos = data_end;
        }
        false
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.len = 0;
    }
}

impl<'a> FromIterator<&'a [u8]> for PackedStrSet {
    fn from_iter<I: IntoIterator<Item = &'a [u8]>>(iter: I) -> Self {
        let mut s = Self::new();
        for m in iter {
            s.insert(m);
        }
        s
    }
}

/// Borrowing iterator over packed members, in insertion order.
pub struct PackedStrSetIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for PackedStrSetIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (mlen, data_start) = read_varint(self.buf, self.pos);
        let data_end = data_start + mlen;
        self.pos = data_end;
        Some(&self.buf[data_start..data_end])
    }
}

/// LEB128 unsigned varint: 1 byte for lengths < 128 (the common case for
/// listpack-eligible members ≤ 64 bytes), growing 7 bits at a time.
fn write_varint(buf: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if n == 0 {
            break;
        }
    }
}

fn encode_varint_array(mut n: usize) -> ([u8; 10], usize) {
    let mut buf = [0u8; 10];
    let mut len = 0usize;
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        buf[len] = byte;
        len += 1;
        if n == 0 {
            break;
        }
    }
    (buf, len)
}

/// Read a LEB128 varint starting at `pos`; returns `(value, index_after_varint)`.
fn read_varint(buf: &[u8], mut pos: usize) -> (usize, usize) {
    let mut result = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = buf[pos];
        pos += 1;
        result |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (result, pos)
}

// `SetMember` / `SetHashTable` (the former inline-or-heap IndexSet backing for
// the Hash variant) were superseded by `CompactStrSet` (frankenredis-ideww).

/// Storage-promotion thresholds: above these a packed set switches to the
/// hashtable so membership/removal stay sub-linear. They only bound how large
/// the O(n) packed scan grows — the observable OBJECT ENCODING `listpack`/
/// `hashtable` flag is tracked separately (and stickily) by the Store from the
/// *configured* thresholds, so the exact storage-promotion point is unobservable.
const PACKED_MAX_ENTRIES: usize = 128;
const PACKED_MAX_VALUE: usize = 64;

/// Storage for a generic (non-integer) set: a packed listpack-style buffer while
/// small, promoting to an `IndexSet` hashtable past the threshold. Drop-in for
/// the former `IndexSet` alias — same insertion-ordered iteration and identical
/// insert/contains/remove semantics (the PackedStrSet proptest above proves the
/// packed half), so SMEMBERS/SSCAN/SPOP output is byte-for-byte unchanged.
/// (frankenredis-9mh3o)
#[derive(Clone, Debug)]
pub enum GenericSet {
    Packed(PackedStrSet),
    Hash(CompactStrSet),
}

impl Default for GenericSet {
    fn default() -> Self {
        GenericSet::Packed(PackedStrSet::new())
    }
}

impl GenericSet {
    #[must_use]
    pub fn with_capacity_and_hasher(n: usize, _hasher: foldhash::quality::RandomState) -> Self {
        if n > PACKED_MAX_ENTRIES {
            // (cc_fr) Actually honor the hint. The previous `CompactStrSet::new()` ignored `n`, so a
            // large set-algebra `*STORE` destination rehashed O(log n) times building the result
            // (`CompactFieldMap::rehash` was 8% self on SINTERSTORE of two 5000-member sets). Reserve
            // the slot table for `n` entries; the STORE path calls `shrink_to_fit` before storing
            // (`SetValue::from_index_set`), so RAM stays at parity with redis's incrementally-grown
            // dst dict and transient (non-STORE) callers free the reservation on drop. `buf_bytes = 0`
            // lets the arena grow to exactly the members (no over-reserved payload).
            GenericSet::Hash(CompactStrSet::with_capacity(n, 0))
        } else {
            GenericSet::Packed(PackedStrSet::with_capacity(n.saturating_mul(8)))
        }
    }

    /// Release capacity reserved past the live members. Preserves membership and iteration order,
    /// so any reply built from the set is byte-identical. Used on set-algebra `*STORE` results,
    /// which are pre-sized to an upper bound during the build. (cc_fr)
    pub(crate) fn shrink_to_fit(&mut self) {
        match self {
            GenericSet::Hash(h) => h.shrink_to_fit(),
            GenericSet::Packed(_) => {}
        }
    }

    /// (frankenredis-saddnodbl) Build a hashtable set directly from
    /// possibly-duplicate borrowed members, deduping via the set's OWN `insert`
    /// (first occurrence kept, insertion order preserved) and returning the
    /// unique/added count. Only applies when the result is unambiguously a
    /// hashtable (`> PACKED_MAX_ENTRIES` members); returns `None` otherwise so the
    /// caller's small/large-value-aware path handles it. This lets the bulk SADD
    /// builder skip the separate throwaway uniqueness `HashSet` (which re-hashes
    /// every member a second time) — byte-identical result to dedup-then-build.
    #[must_use]
    pub fn try_from_str_members_hash_dedup<M: AsRef<[u8]>>(members: &[M]) -> Option<(Self, u64)> {
        if members.len() <= PACKED_MAX_ENTRIES {
            return None;
        }
        let bytes: usize = members.iter().map(|m| m.as_ref().len() + 2).sum();
        let mut h = CompactStrSet::with_capacity(members.len(), bytes);
        let mut added = 0_u64;
        for m in members {
            if h.insert(m.as_ref()) {
                added += 1;
            }
        }
        Some((GenericSet::Hash(h), added))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            GenericSet::Packed(p) => p.len(),
            GenericSet::Hash(h) => h.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn contains(&self, member: &[u8]) -> bool {
        match self {
            GenericSet::Packed(p) => p.contains(member),
            GenericSet::Hash(h) => h.contains(member),
        }
    }

    /// nth member in insertion order (powers SPOP/SRANDMEMBER index selection).
    #[must_use]
    pub fn get_index(&self, idx: usize) -> Option<&[u8]> {
        match self {
            GenericSet::Packed(p) => p.iter().nth(idx),
            GenericSet::Hash(h) => h.get_index(idx),
        }
    }

    fn promote(&mut self) {
        if let GenericSet::Packed(p) = self {
            let mut h = CompactStrSet::new();
            for m in p.iter() {
                h.insert(m);
            }
            *self = GenericSet::Hash(h);
        }
    }

    pub fn insert(&mut self, member: Vec<u8>) -> bool {
        if let GenericSet::Packed(p) = self
            && (p.len() >= PACKED_MAX_ENTRIES || member.len() > PACKED_MAX_VALUE)
        {
            self.promote();
        }
        match self {
            GenericSet::Packed(p) => p.insert(&member),
            GenericSet::Hash(h) => h.insert(&member),
        }
    }

    /// (frankenredis-saddfast) Borrowed-member insert: byte-identical in result
    /// and final state to [`Self::insert`] with `member.to_vec()`, but allocates
    /// an owned member only on a genuine `Hash`-encoding miss. The `Packed`
    /// listpack already copies the bytes into its backing buffer, and a `Hash`
    /// duplicate add (the overwhelmingly common case in `SADD myset element`
    /// once the keyspace saturates) needs no allocation at all — matching redis's
    /// `dict`, which never allocates an sds on a duplicate `setTypeAdd`. The
    /// promotion check fires under the identical condition as `insert`, so the
    /// observable encoding transition is unchanged.
    pub fn insert_borrowed(&mut self, member: &[u8]) -> bool {
        if let GenericSet::Packed(p) = self
            && (p.len() >= PACKED_MAX_ENTRIES || member.len() > PACKED_MAX_VALUE)
        {
            self.promote();
        }
        match self {
            GenericSet::Packed(p) => p.insert(member),
            // CompactStrSet::insert returns true iff newly added — exactly the
            // IndexSet contains-then-insert split, byte-for-byte.
            GenericSet::Hash(h) => h.insert(member),
        }
    }

    /// Bulk-build a generic set from already-unique members (NO duplicate check),
    /// choosing the final Packed-vs-Hash encoding once and filling it in a single
    /// O(total bytes) pass. Byte-identical (members + iteration order + encoding)
    /// to inserting `members` in order via [`Self::insert_borrowed`] starting from
    /// an empty set — the loop's mid-stream Packed→Hash promotion preserves
    /// insertion order, and the final encoding is `Hash` iff `count > PACKED_MAX_ENTRIES`
    /// or some member exceeds `PACKED_MAX_VALUE`, the same predicate decided here.
    /// Skips the per-insert O(n) `PackedStrSet::contains` scan, so an N-member
    /// build is O(N) instead of O(N²). Callers must guarantee uniqueness.
    #[must_use]
    pub fn from_unique_str_members<M: AsRef<[u8]>>(members: &[M]) -> Self {
        let n = members.len();
        let packed =
            n <= PACKED_MAX_ENTRIES && members.iter().all(|m| m.as_ref().len() <= PACKED_MAX_VALUE);
        if packed {
            let bytes: usize = members.iter().map(|m| m.as_ref().len() + 2).sum();
            let mut p = PackedStrSet::with_capacity(bytes);
            for m in members {
                p.append(m.as_ref());
            }
            GenericSet::Packed(p)
        } else {
            let bytes: usize = members.iter().map(|m| m.as_ref().len() + 2).sum();
            let mut h = CompactStrSet::with_capacity(n, bytes);
            for m in members {
                h.insert(m.as_ref());
            }
            GenericSet::Hash(h)
        }
    }

    pub fn shift_remove(&mut self, member: &[u8]) -> bool {
        match self {
            GenericSet::Packed(p) => p.remove(member),
            GenericSet::Hash(h) => h.shift_remove(member),
        }
    }

    /// (frankenredis-spopfast) Remove and return the member at `idx`. For the
    /// `Hash` (hashtable) encoding this is an O(1) `swap_remove_index` instead
    /// of an O(n) shift: a hashtable-encoded set's iteration order is already
    /// unspecified (redis's `dict` is unordered too), so SPOP's random removal
    /// need not preserve order — turning SPOP on a large set from O(n) into O(1)
    /// per element. The `Packed` (listpack) encoding keeps the order-preserving
    /// remove, matching redis's ordered listpack delete on small sets.
    pub fn pop_index(&mut self, idx: usize) -> Option<Vec<u8>> {
        match self {
            GenericSet::Packed(p) => {
                let member = p.iter().nth(idx)?.to_vec();
                p.remove(&member);
                Some(member)
            }
            GenericSet::Hash(h) => h.swap_remove_index(idx),
        }
    }

    /// (frankenredis-sremfast) Remove `member` without preserving iteration
    /// order for the `Hash` encoding — an O(1) `swap_remove` rather than the
    /// O(n) `shift_remove`. Safe because a hashtable-encoded set's order is
    /// unspecified (redis's `dict` is unordered). `Packed` (listpack) keeps the
    /// order-preserving remove to match redis's small-set listpack delete.
    pub fn swap_remove(&mut self, member: &[u8]) -> bool {
        match self {
            GenericSet::Packed(p) => p.remove(member),
            GenericSet::Hash(h) => h.swap_remove(member),
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        match self {
            GenericSet::Packed(p) => {
                let survivors: Vec<Vec<u8>> =
                    p.iter().filter(|m| keep(m)).map(|m| m.to_vec()).collect();
                let mut np = PackedStrSet::with_capacity(p.byte_len());
                for m in &survivors {
                    np.insert(m);
                }
                *p = np;
            }
            GenericSet::Hash(h) => h.retain(keep),
        }
    }

    #[must_use]
    pub fn iter(&self) -> GenericSetIter<'_> {
        match self {
            GenericSet::Packed(p) => GenericSetIter::Packed(p.iter()),
            GenericSet::Hash(h) => GenericSetIter::Hash(h.iter()),
        }
    }
}

/// Set equality is order-independent (matches `IndexSet`'s `PartialEq`), so a
/// Packed and a Hash set with the same members compare equal.
impl PartialEq for GenericSet {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|m| other.contains(m))
    }
}
impl Eq for GenericSet {}

impl FromIterator<Vec<u8>> for GenericSet {
    fn from_iter<I: IntoIterator<Item = Vec<u8>>>(iter: I) -> Self {
        let mut s = GenericSet::default();
        for m in iter {
            s.insert(m);
        }
        s
    }
}

impl IntoIterator for GenericSet {
    type Item = Vec<u8>;
    type IntoIter = std::vec::IntoIter<Vec<u8>>;
    fn into_iter(self) -> Self::IntoIter {
        let owned: Vec<Vec<u8>> = match self {
            GenericSet::Packed(p) => p.iter().map(<[u8]>::to_vec).collect(),
            GenericSet::Hash(h) => h.iter().map(<[u8]>::to_vec).collect(),
        };
        owned.into_iter()
    }
}

/// Borrowing iterator over a `GenericSet`'s members in insertion order.
pub enum GenericSetIter<'a> {
    Packed(PackedStrSetIter<'a>),
    Hash(CompactStrSetIter<'a>),
}

impl<'a> Iterator for GenericSetIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            GenericSetIter::Packed(it) => it.next(),
            GenericSetIter::Hash(it) => it.next(),
        }
    }
}

// `HashFieldBytes` / `FieldHashTable` (the former inline-or-heap IndexMap backing
// for the Hash variant) were superseded by `CompactFieldMap` (frankenredis-ideww).

/// Storage for a hash's field→value map: a packed listpack-style buffer while
/// small, promoting to an `IndexMap` hashtable past the threshold. Drop-in for
/// the former `IndexMap` alias — same insertion-ordered iteration and identical
/// get/insert/contains/remove semantics, so HGETALL/HKEYS/HVALS/HSCAN output is
/// byte-for-byte unchanged. (frankenredis-9mh3o step 3)
#[derive(Clone, Debug)]
pub enum HashFieldMap {
    Packed(PackedStrMap),
    Hash(CompactFieldMap),
}

impl Default for HashFieldMap {
    fn default() -> Self {
        HashFieldMap::Packed(PackedStrMap::new())
    }
}

impl HashFieldMap {
    /// (frankenredis-qxfmr) Build a map from already-unique pairs in ONE O(n)
    /// pass, instead of N incremental `insert`s that each do an O(n) `locate` /
    /// `contains_key` scan (O(n²) total) plus a mid-stream Packed→Hash promotion
    /// copy. Used by the RDB / bulk-load path where the input fields are unique.
    ///
    /// Byte-identical to inserting the same unique pairs one at a time: the
    /// `Packed`-vs-`Hash` choice is the SAME predicate `insert` reaches —
    /// `Packed` iff `len <= PACKED_MAX_ENTRIES` and every field/value
    /// `<= PACKED_MAX_VALUE`, else `Hash` — and both variants keep insertion
    /// order (the `PackedStrMap` buffer order, the `IndexMap` insertion order),
    /// exactly as the incremental path's final state. Caller MUST guarantee the
    /// pairs have no duplicate fields.
    #[must_use]
    pub fn from_unique_pairs(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        let to_hash = pairs.len() > PACKED_MAX_ENTRIES
            || pairs
                .iter()
                .any(|(f, v)| f.len() > PACKED_MAX_VALUE || v.len() > PACKED_MAX_VALUE);
        if to_hash {
            let bytes: usize = pairs.iter().map(|(f, v)| f.len() + v.len() + 10).sum();
            let mut h = CompactFieldMap::with_capacity(pairs.len(), bytes);
            for (field, value) in pairs {
                h.insert(&field, &value);
            }
            HashFieldMap::Hash(h)
        } else {
            let bytes: usize = pairs.iter().map(|(f, v)| f.len() + v.len() + 10).sum();
            let mut p = PackedStrMap::with_capacity(bytes);
            for (field, value) in pairs {
                p.append(&field, &value);
            }
            HashFieldMap::Packed(p)
        }
    }

    /// Borrowed-input twin of [`Self::from_unique_pairs`] for the RESTORE/RDB-load
    /// path: the field/value bytes are COPIED into the packed/hash storage by
    /// `append`/`insert` either way, so taking borrowed slices (e.g. zero-copy
    /// listpack spans) avoids materialising N transient owned `Vec<u8>` per hash
    /// just to drop them after the copy. Byte-identical to building the owned
    /// `Vec<(Vec<u8>,Vec<u8>)>` and calling `from_unique_pairs`.
    /// Caller MUST guarantee the pairs have no duplicate fields.
    /// (BlackThrush: RESTORE decode zero-copy span build)
    #[must_use]
    pub fn from_unique_pairs_borrowed(pairs: &[(&[u8], &[u8])]) -> Self {
        let to_hash = pairs.len() > PACKED_MAX_ENTRIES
            || pairs
                .iter()
                .any(|(f, v)| f.len() > PACKED_MAX_VALUE || v.len() > PACKED_MAX_VALUE);
        if to_hash {
            let bytes: usize = pairs.iter().map(|(f, v)| f.len() + v.len() + 10).sum();
            let mut h = CompactFieldMap::with_capacity(pairs.len(), bytes);
            for (field, value) in pairs {
                h.insert(field, value);
            }
            HashFieldMap::Hash(h)
        } else {
            let bytes: usize = pairs.iter().map(|(f, v)| f.len() + v.len() + 10).sum();
            let mut p = PackedStrMap::with_capacity(bytes);
            for (field, value) in pairs {
                p.append(field, value);
            }
            HashFieldMap::Packed(p)
        }
    }

    /// (frankenredis-saddnodbl) Build a hashtable hash directly from a FLAT
    /// borrowed `[f0,v0,f1,v1,…]` slice, de-duping/last-wins via the map's OWN
    /// `insert` and returning the added (new-field) count. Only applies when the
    /// result is unambiguously a hashtable (`> PACKED_MAX_ENTRIES` pairs); returns
    /// `None` otherwise so the caller's Packed-capable path handles it. Lets the
    /// bulk HSET/HMSET builder skip its separate uniqueness `HashSet` (a second
    /// hash of every field). Byte-identical to dedup-then-build: `CompactFieldMap`
    /// keeps insertion order and overwrites on a repeat field exactly like the
    /// incremental loop.
    #[must_use]
    pub fn try_from_flat_pairs_hash_dedup(flat: &[&[u8]]) -> Option<(Self, usize)> {
        let npairs = flat.len() / 2;
        if npairs <= PACKED_MAX_ENTRIES {
            return None;
        }
        let bytes: usize = flat.iter().map(|s| s.len() + 5).sum();
        let mut h = CompactFieldMap::with_capacity(npairs, bytes);
        let mut added = 0_usize;
        let (pairs, _) = flat.as_chunks::<2>();
        for p in pairs {
            if h.insert(p[0], p[1]).is_none() {
                added += 1;
            }
        }
        Some((HashFieldMap::Hash(h), added))
    }

    /// Apply a borrowed flat HSET payload to an existing packed hash by building
    /// a transient overlay of the command fields, then rebuilding the final
    /// map once. This avoids K repeated listpack scans for variadic HSET against
    /// small-but-nonempty hashes while preserving insertion order exactly:
    /// existing fields keep their current slots, new fields append in first
    /// command occurrence order, and duplicate command fields use the last value.
    #[must_use]
    pub fn try_update_existing_packed_borrowed(&mut self, flat: &[&[u8]]) -> Option<usize> {
        let pair_count = flat.len() / 2;
        let HashFieldMap::Packed(packed) = self else {
            return None;
        };
        if packed.is_empty() || pair_count < 8 {
            return None;
        }
        if packed
            .iter()
            .any(|(field, value)| field.len() > PACKED_MAX_VALUE || value.len() > PACKED_MAX_VALUE)
            || flat
                .as_chunks::<2>()
                .0
                .iter()
                .any(|pair| pair[0].len() > PACKED_MAX_VALUE || pair[1].len() > PACKED_MAX_VALUE)
        {
            return None;
        }

        struct Pending<'a> {
            field: &'a [u8],
            value: &'a [u8],
            existed: bool,
        }

        let mut pending: Vec<Pending<'_>> = Vec::with_capacity(pair_count);
        let mut field_to_pending: std::collections::HashMap<
            &[u8],
            usize,
            foldhash::quality::RandomState,
        > = std::collections::HashMap::with_capacity_and_hasher(
            pair_count,
            foldhash::quality::RandomState::default(),
        );

        let (pairs, _) = flat.as_chunks::<2>();
        for pair in pairs {
            if let Some(&idx) = field_to_pending.get(pair[0]) {
                pending[idx].value = pair[1];
            } else {
                let idx = pending.len();
                field_to_pending.insert(pair[0], idx);
                pending.push(Pending {
                    field: pair[0],
                    value: pair[1],
                    existed: false,
                });
            }
        }

        let mut added = 0_usize;
        let rebuilt = {
            let mut pairs: Vec<(&[u8], &[u8])> = Vec::with_capacity(packed.len() + pending.len());
            for (field, value) in packed.iter() {
                if let Some(&idx) = field_to_pending.get(field) {
                    pending[idx].existed = true;
                    pairs.push((field, pending[idx].value));
                } else {
                    pairs.push((field, value));
                }
            }
            for item in &pending {
                if !item.existed {
                    pairs.push((item.field, item.value));
                    added += 1;
                }
            }
            HashFieldMap::from_unique_pairs_borrowed(&pairs)
        };
        *self = rebuilt;
        Some(added)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            HashFieldMap::Packed(p) => p.len(),
            HashFieldMap::Hash(h) => h.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(&self, field: &[u8]) -> Option<&[u8]> {
        match self {
            HashFieldMap::Packed(p) => p.get(field),
            HashFieldMap::Hash(h) => h.get(field),
        }
    }

    #[must_use]
    pub fn contains_key(&self, field: &[u8]) -> bool {
        match self {
            HashFieldMap::Packed(p) => p.contains_key(field),
            HashFieldMap::Hash(h) => h.contains_key(field),
        }
    }

    #[must_use]
    pub fn get_index(&self, idx: usize) -> Option<(&[u8], &[u8])> {
        match self {
            HashFieldMap::Packed(p) => p.get_index(idx),
            HashFieldMap::Hash(h) => h.get_index(idx),
        }
    }

    fn promote(&mut self) {
        if let HashFieldMap::Packed(p) = self {
            let mut h = CompactFieldMap::new();
            for (k, v) in p.iter() {
                h.insert(k, v);
            }
            *self = HashFieldMap::Hash(h);
        }
    }

    /// Insert/overwrite, returning the previous value (matches `IndexMap::insert`).
    pub fn insert(&mut self, field: Vec<u8>, value: Vec<u8>) -> Option<Vec<u8>> {
        // (cc_fr) Test the O(1) promotion PRECONDITION (at entry cap / oversized field or value)
        // before the O(n) `contains_key` locate scan: promotion is impossible below all caps, so a
        // steady-state small-hash HSET short-circuits and skips this scan entirely — collapsing the
        // packed HSET's two locate scans (this guard + insert's own) to one. `&&` reorder;
        // `contains_key` is a pure read, so the promotion decision is byte-identical.
        if let HashFieldMap::Packed(p) = self
            && (p.len() >= PACKED_MAX_ENTRIES
                || field.len() > PACKED_MAX_VALUE
                || value.len() > PACKED_MAX_VALUE)
            && !p.contains_key(&field)
        {
            self.promote();
        }
        match self {
            HashFieldMap::Packed(p) => p.insert(field, value),
            HashFieldMap::Hash(h) => h.insert(&field, &value),
        }
    }

    /// (frankenredis-hsetfast) Borrowed-field upsert: returns `true` iff the
    /// field was newly added. Byte-identical in result and final state to
    /// `insert(field.to_vec(), value).is_none()`, but does NOT allocate an owned
    /// field key when the field already exists — it overwrites the value slot in
    /// place. Redis's `hashTypeSet` on an existing field likewise keeps the field
    /// sds and frees/replaces only the value sds, so a `HSET myhash f v` against a
    /// saturated keyspace (the duplicate-field steady state) allocates a field
    /// key in fr exactly where redis allocates none. The `Hash` (hashtable)
    /// overwrite also collapses the old contains_key-then-insert double probe into
    /// a single `get_mut`. The promotion check fires under the identical condition
    /// as `insert` (new field only), so the encoding transition is unchanged.
    pub fn insert_borrowed(&mut self, field: &[u8], value: Vec<u8>) -> bool {
        // (cc_fr) O(1) promotion precondition before the O(n) `contains_key` locate scan (see
        // `insert`): the steady-state small-hash HSET (below all caps) short-circuits and skips
        // this scan, so a packed HSET does ONE locate (insert_borrowed's) instead of two. `&&`
        // reorder; `contains_key` is a pure read ⇒ byte-identical promotion decision.
        if let HashFieldMap::Packed(p) = self
            && (p.len() >= PACKED_MAX_ENTRIES
                || field.len() > PACKED_MAX_VALUE
                || value.len() > PACKED_MAX_VALUE)
            && !p.contains_key(field)
        {
            self.promote();
        }
        match self {
            HashFieldMap::Packed(p) => p.insert_borrowed(field, value),
            // CompactFieldMap::insert_borrowed keeps an existing field's
            // position and reports "newly added" directly, matching the IndexMap
            // get_mut/insert split byte-for-byte while avoiding old-value
            // allocation on duplicate-field HSET.
            HashFieldMap::Hash(h) => h.insert_borrowed(field, &value),
        }
    }

    pub fn shift_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        match self {
            HashFieldMap::Packed(p) => p.shift_remove(field),
            HashFieldMap::Hash(h) => h.shift_remove(field),
        }
    }

    /// (frankenredis-sremfast) Remove `field` without preserving iteration order
    /// for the `Hash` encoding — an O(1) `swap_remove` rather than the O(n)
    /// `shift_remove`. HDEL of k fields from a large hashtable-encoded hash was
    /// O(k·n) on the insertion-ordered `IndexMap`; redis's `dict` does O(k). A
    /// hashtable-encoded hash's field order is unspecified (redis's `dict` is
    /// unordered too), so swapping is safe. `Packed` (listpack) keeps the
    /// order-preserving remove to match redis's small-hash listpack delete.
    pub fn swap_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        match self {
            HashFieldMap::Packed(p) => p.shift_remove(field),
            HashFieldMap::Hash(h) => h.swap_remove(field),
        }
    }

    /// (frankenredis-ym6ih) Remove `field`, returning only whether it existed —
    /// for HDEL, which counts removed fields and discards the value. Avoids the
    /// owned-value allocation that `swap_remove` makes on the hashtable path.
    /// Same final state and order semantics as `swap_remove(field).is_some()`.
    pub fn delete(&mut self, field: &[u8]) -> bool {
        match self {
            HashFieldMap::Packed(p) => p.shift_remove(field).is_some(),
            HashFieldMap::Hash(h) => h.delete(field),
        }
    }

    #[must_use]
    pub fn iter(&self) -> HashFieldMapIter<'_> {
        match self {
            HashFieldMap::Packed(p) => HashFieldMapIter::Packed(p.iter()),
            HashFieldMap::Hash(h) => HashFieldMapIter::Hash(h.iter()),
        }
    }

    pub fn keys(&self) -> HashFieldMapKeyIter<'_> {
        match self {
            // Packed hashes are tiny (<= PACKED_MAX_ENTRIES); the value decode is
            // negligible. The hashtable-range variant skips the value entirely.
            HashFieldMap::Packed(p) => HashFieldMapKeyIter::Packed(p.iter()),
            HashFieldMap::Hash(h) => HashFieldMapKeyIter::Hash(h.field_iter()),
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(|(_, v)| v)
    }
}

/// Map equality is order-independent on (field, value) pairs (matches
/// `IndexMap`'s `PartialEq`), so a Packed and a Hash map with the same entries
/// compare equal.
impl PartialEq for HashFieldMap {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}
impl Eq for HashFieldMap {}

impl FromIterator<(Vec<u8>, Vec<u8>)> for HashFieldMap {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>>(iter: I) -> Self {
        let mut m = HashFieldMap::default();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

/// (CrimsonHawk) Field-only iterator over a `HashFieldMap` (HKEYS / HSCAN
/// NOVALUES). The hashtable-range arm skips the per-entry value decode.
pub enum HashFieldMapKeyIter<'a> {
    Packed(PackedStrMapIter<'a>),
    Hash(CompactFieldMapFieldIter<'a>),
}

impl<'a> Iterator for HashFieldMapKeyIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            HashFieldMapKeyIter::Packed(it) => it.next().map(|(k, _)| k),
            HashFieldMapKeyIter::Hash(it) => it.next(),
        }
    }
}

/// Borrowing iterator over a `HashFieldMap`'s (field, value) pairs.
pub enum HashFieldMapIter<'a> {
    Packed(PackedStrMapIter<'a>),
    Hash(CompactFieldMapIter<'a>),
}

impl<'a> Iterator for HashFieldMapIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            HashFieldMapIter::Packed(it) => it.next(),
            HashFieldMapIter::Hash(it) => it.next(),
        }
    }
}

// ─────────────── compact arena+index field map (frankenredis-ideww) ──────────

/// (frankenredis-ideww) Compact insertion-ordered field→value map for the
/// hashtable-range hash encoding (129+ fields). Stores every field+value pair
/// contiguously in ONE arena (no per-entry heap block, no per-entry stored u64
/// hash) with a small open-addressing index for O(1) lookup and an `order` list
/// for O(1) positional access + insertion-order iteration. Targets
/// ~redis-listpack RAM (~35-41 B/field vs the current `IndexMap` ~127) while
/// KEEPING O(1) get/insert — vs redis's listpack which is compact but O(n) scan.
/// Drop-in for the `IndexMap<HashFieldBytes,HashFieldBytes>` surface used by
/// `HashFieldMap::Hash`; NOT yet wired in (validated by an equivalence test vs
/// `IndexMap` first).
///
/// Entry layout in `buf`: `[flen varint][field][vlen varint][value]`. A value
/// update appends a fresh entry (old bytes become dead, reclaimed by `compact`
/// once dead exceeds half the arena) and keeps the field's order position, so
/// HGETALL/HKEYS/HVALS order is byte-for-byte identical to `IndexMap::insert`.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // wired into HashFieldMap::Hash in a follow-up (frankenredis-ideww)
pub struct CompactFieldMap {
    buf: Vec<u8>,
    /// `buf` offsets of live entries, in insertion order. `order.len()` == count.
    order: Vec<u32>,
    /// (frankenredis-ym6ih) Back-pointer: `slot_of[pos]` is the `slots` index
    /// that points at order position `pos` (so `slots[slot_of[pos]] == pos + 2`).
    /// Lets `swap_remove` repoint a moved entry's slot in O(1) without re-probing
    /// by its field bytes (killing a probe + an owned-field allocation per
    /// delete). `slot_of.len()` == `order.len()`; rebuilt by `rehash`.
    slot_of: Vec<u32>,
    /// Open-addressing slots (linear probe). 0 = EMPTY, 1 = TOMBSTONE, else the
    /// occupant's `pos_in_order + 2`. `slots.len()` is a power of two (or 0).
    slots: Vec<u32>,
    /// (CrimsonHawk) Per-slot 1-byte hash tag (top byte of the field hash),
    /// parallel to `slots` (`tags.len() == slots.len()`). Probing compares the
    /// tag before touching the arena, so a tag mismatch skips the
    /// `order`→arena decode + `memcmp` entirely — the SwissTable h2 trick. This
    /// closes the per-probe arena-indirection cost vs redis's pointer-in-dict
    /// entries on membership-heavy ops (SINTER/SDIFF/`contains`). A slot always
    /// holds the same field until tombed (swap_remove repoints keep the field),
    /// and TOMB/EMPTY are checked via `slots` before the tag, so tags only need
    /// writing where a slot becomes occupied (insert + rehash); deletes leave a
    /// stale-but-ignored tag. Tag collisions are harmless — the `memcmp` still
    /// confirms. Stored bytes are transient (never serialised).
    tags: Vec<u8>,
    /// Dead (unreferenced) bytes in `buf`, from value updates / removals.
    dead: usize,
    /// Tombstone slot count (for the rehash-on-load trigger).
    tombs: usize,
    state: foldhash::quality::RandomState,
}

#[allow(dead_code)]
const CFM_EMPTY: u32 = 0;
#[allow(dead_code)]
const CFM_TOMB: u32 = 1;

#[allow(dead_code)]
fn cfm_decode(buf: &[u8], off: u32) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let off = off as usize;
    let (flen, p) = read_varint(buf, off);
    let (fs, fe) = (p, p + flen);
    let (vlen, p2) = read_varint(buf, fe);
    let (vs, ve) = (p2, p2 + vlen);
    (fs..fe, vs..ve)
}

/// (CrimsonHawk) Decode ONLY the field byte-range of an entry, skipping the
/// value-length varint that `cfm_decode` also reads. Membership probing
/// (`lookup_slot`) compares only the field, so reading the value varint per
/// probe is wasted — and for the set encoding (members carry an empty value)
/// it is pure overhead on the SINTER/SDIFF/`contains` hot loops. Byte-identical
/// field range to `cfm_decode(..).0`.
#[inline]
fn cfm_field_range(buf: &[u8], off: u32) -> std::ops::Range<usize> {
    let off = off as usize;
    let (flen, p) = read_varint(buf, off);
    p..p + flen
}

#[allow(dead_code)]
impl CompactFieldMap {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// (frankenredis-cfm-presize) Build an empty map already sized for `entries`
    /// inserts and ~`buf_bytes` of arena payload. Pre-sizing `slots` to a
    /// power-of-two big enough that the load factor stays < 0.75 across all
    /// `entries` inserts means the per-insert grow check never fires `rehash`,
    /// and reserving `buf`/`order`/`slot_of` removes the incremental reallocs.
    /// Byte-identical to `new()` + the same insert sequence (`insert` maintains
    /// `slot_of` incrementally; the only thing skipped is intermediate rehashing
    /// and buffer growth). Used by the unique-pairs bulk builders (RDB / DEBUG
    /// RELOAD load of a hashtable-encoded hash).
    #[must_use]
    pub(crate) fn with_capacity(entries: usize, buf_bytes: usize) -> Self {
        let mut m = Self::default();
        if entries > 0 {
            m.buf.reserve(buf_bytes);
            m.order.reserve(entries);
            m.slot_of.reserve(entries);
            let cap = ((entries + 1) * 2).next_power_of_two().max(8);
            m.slots = vec![CFM_EMPTY; cap];
            m.tags = vec![0u8; cap];
        }
        m
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn hash(&self, field: &[u8]) -> u64 {
        use std::hash::BuildHasher;
        self.state.hash_one(field)
    }

    fn entry_size(&self, off: u32) -> usize {
        let (_, vr) = cfm_decode(&self.buf, off);
        vr.end - off as usize
    }

    /// Returns the `order` position of `field`, or `None`.
    fn lookup(&self, field: &[u8]) -> Option<usize> {
        self.lookup_slot(field).map(|(pos, _)| pos)
    }

    /// Returns `(order_position, slot_index)` for `field`, or `None`. The slot
    /// index lets removers tombstone/repoint the slot directly instead of
    /// re-probing by field bytes (frankenredis-ym6ih).
    fn lookup_slot(&self, field: &[u8]) -> Option<(usize, usize)> {
        self.lookup_slot_prehashed(field, self.hash(field))
    }

    /// (CrimsonHawk) `lookup_slot` with a precomputed hash, so an insert can hash
    /// the field ONCE and reuse it for both the existence probe and the empty-slot
    /// placement (the new-field path re-hashed the same bytes a second time). `h`
    /// MUST equal `self.hash(field)`; byte-identical to `lookup_slot`.
    fn lookup_slot_prehashed(&self, field: &[u8], h: u64) -> Option<(usize, usize)> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let tag = (h >> 56) as u8;
        let mut slot = (h as usize) & mask;
        loop {
            let s = self.slots[slot];
            if s == CFM_EMPTY {
                return None;
            }
            // Compare the 1-byte hash tag before the arena decode + memcmp; a
            // mismatch (the common case for a colliding-slot probe) skips both.
            if s != CFM_TOMB && self.tags[slot] == tag {
                let pos = (s - 2) as usize;
                let fr = cfm_field_range(&self.buf, self.order[pos]);
                if &self.buf[fr] == field {
                    return Some((pos, slot));
                }
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Rebuild `slots` at `new_cap` (power of two), dropping tombstones and
    /// re-probing every live entry from `order`.
    fn rehash(&mut self, new_cap: usize) {
        let cap = new_cap.next_power_of_two().max(8);
        let mut slots = vec![CFM_EMPTY; cap];
        let mut tags = vec![0u8; cap];
        let mut slot_of = vec![0u32; self.order.len()];
        let mask = cap - 1;
        for (pos, &off) in self.order.iter().enumerate() {
            let fr = cfm_field_range(&self.buf, off);
            // Re-hash from the field bytes already in `buf`.
            let h = {
                use std::hash::BuildHasher;
                self.state.hash_one(&self.buf[fr])
            };
            let mut slot = (h as usize) & mask;
            while slots[slot] != CFM_EMPTY {
                slot = (slot + 1) & mask;
            }
            slots[slot] = (pos as u32) + 2;
            tags[slot] = (h >> 56) as u8;
            slot_of[pos] = slot as u32;
        }
        self.slots = slots;
        self.tags = tags;
        self.slot_of = slot_of;
        self.tombs = 0;
    }

    /// Rebuild the slot table at the smallest power-of-two that fits the live entries and release
    /// unused `buf`/`order`/`slot_of` capacity. `rehash` rebuilds from `order`, so insertion order
    /// — hence iteration order and every reply — is byte-identical. Skips the rehash when the slot
    /// table is already minimal, so it is ~free on a tightly-built map. (cc_fr set-algebra presize)
    pub(crate) fn shrink_to_fit(&mut self) {
        let target = (((self.order.len() + 1) * 2).next_power_of_two()).max(8);
        if target < self.slots.len() {
            self.rehash(target);
        }
        self.buf.shrink_to_fit();
        self.order.shrink_to_fit();
        self.slot_of.shrink_to_fit();
    }

    fn append_entry(&mut self, field: &[u8], value: &[u8]) -> u32 {
        let off = self.buf.len() as u32;
        write_varint(&mut self.buf, field.len());
        self.buf.extend_from_slice(field);
        write_varint(&mut self.buf, value.len());
        self.buf.extend_from_slice(value);
        off
    }

    /// Insert `field`→`value`; returns the previous value if the field existed.
    /// Matches `IndexMap::insert` (existing field keeps its position).
    pub(crate) fn insert(&mut self, field: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        // (CrimsonHawk) Hash the field ONCE and reuse it for the existence probe and
        // (on the new-field path) the empty-slot placement — the placement re-hashed the
        // same bytes. `h` is stable across the rehash below (it hashes field bytes, not
        // slot layout), so reuse is byte-identical.
        let h = self.hash(field);
        if let Some((pos, _)) = self.lookup_slot_prehashed(field, h) {
            let old_off = self.order[pos];
            let (_, vr) = cfm_decode(&self.buf, old_off);
            let old_value = self.buf[vr.clone()].to_vec();
            if value.len() == vr.len() {
                self.buf[vr].copy_from_slice(value);
                return Some(old_value);
            }
            self.dead += self.entry_size(old_off);
            let new_off = self.append_entry(field, value);
            self.order[pos] = new_off;
            self.maybe_compact();
            return Some(old_value);
        }
        // New field. Ensure load factor < 0.75 (count slots used incl tombstones).
        let used = self.order.len() + self.tombs + 1;
        if self.slots.is_empty() || used * 4 >= self.slots.len() * 3 {
            let target = (self.order.len() + 1) * 2;
            self.rehash(target.max(self.slots.len()));
        }
        let new_off = self.append_entry(field, value);
        let pos = self.order.len();
        self.order.push(new_off);
        // Probe for an EMPTY or reusable TOMBSTONE slot (reuse the hash from above).
        let mask = self.slots.len() - 1;
        let tag = (h >> 56) as u8;
        let mut slot = (h as usize) & mask;
        let mut first_tomb: Option<usize> = None;
        loop {
            let s = self.slots[slot];
            if s == CFM_EMPTY {
                let target = first_tomb.unwrap_or(slot);
                if self.slots[target] == CFM_TOMB {
                    self.tombs -= 1;
                }
                self.slots[target] = (pos as u32) + 2;
                self.tags[target] = tag;
                self.slot_of.push(target as u32);
                break;
            }
            if s == CFM_TOMB && first_tomb.is_none() {
                first_tomb = Some(slot);
            }
            slot = (slot + 1) & mask;
        }
        self.maybe_compact();
        None
    }

    /// Borrowed-field upsert for callers that only need to know whether the
    /// field was new. Existing-field updates avoid allocating the old value; if
    /// the replacement value has the same byte length, the arena entry is
    /// rewritten in place instead of appending a dead record.
    pub(crate) fn insert_borrowed(&mut self, field: &[u8], value: &[u8]) -> bool {
        // (CrimsonHawk) Hash once; reuse for the probe and the new-field placement.
        let h = self.hash(field);
        if let Some((pos, _)) = self.lookup_slot_prehashed(field, h) {
            let old_off = self.order[pos];
            let (_, vr) = cfm_decode(&self.buf, old_off);
            if value.len() == vr.len() {
                self.buf[vr].copy_from_slice(value);
            } else {
                self.dead += self.entry_size(old_off);
                let new_off = self.append_entry(field, value);
                self.order[pos] = new_off;
                self.maybe_compact();
            }
            return false;
        }

        // New field. Ensure load factor < 0.75 (count slots used incl tombstones).
        let used = self.order.len() + self.tombs + 1;
        if self.slots.is_empty() || used * 4 >= self.slots.len() * 3 {
            let target = (self.order.len() + 1) * 2;
            self.rehash(target.max(self.slots.len()));
        }
        let new_off = self.append_entry(field, value);
        let pos = self.order.len();
        self.order.push(new_off);
        let mask = self.slots.len() - 1;
        let tag = (h >> 56) as u8;
        let mut slot = (h as usize) & mask;
        let mut first_tomb: Option<usize> = None;
        loop {
            let s = self.slots[slot];
            if s == CFM_EMPTY {
                let target = first_tomb.unwrap_or(slot);
                if self.slots[target] == CFM_TOMB {
                    self.tombs -= 1;
                }
                self.slots[target] = (pos as u32) + 2;
                self.tags[target] = tag;
                self.slot_of.push(target as u32);
                break;
            }
            if s == CFM_TOMB && first_tomb.is_none() {
                first_tomb = Some(slot);
            }
            slot = (slot + 1) & mask;
        }
        self.maybe_compact();
        true
    }

    #[must_use]
    pub(crate) fn get(&self, field: &[u8]) -> Option<&[u8]> {
        let pos = self.lookup(field)?;
        let (_, vr) = cfm_decode(&self.buf, self.order[pos]);
        Some(&self.buf[vr])
    }

    #[must_use]
    pub(crate) fn contains_key(&self, field: &[u8]) -> bool {
        self.lookup(field).is_some()
    }

    /// The (field, value) at insertion-order index `idx`.
    #[must_use]
    pub(crate) fn get_index(&self, idx: usize) -> Option<(&[u8], &[u8])> {
        let off = *self.order.get(idx)?;
        let (fr, vr) = cfm_decode(&self.buf, off);
        Some((&self.buf[fr], &self.buf[vr]))
    }

    /// (CrimsonHawk) Field bytes at order position `idx`, skipping the value
    /// decode. For the set encoding (members carry an empty value) the value
    /// range is always discarded by callers, so reading its varint per element
    /// is pure overhead on set iteration (SMEMBERS/SPOP/SUNION/SINTER base-walk).
    #[must_use]
    pub(crate) fn field_at(&self, idx: usize) -> Option<&[u8]> {
        let off = *self.order.get(idx)?;
        Some(&self.buf[cfm_field_range(&self.buf, off)])
    }

    #[must_use]
    pub(crate) fn iter(&self) -> CompactFieldMapIter<'_> {
        CompactFieldMapIter { map: self, pos: 0 }
    }

    /// (CrimsonHawk) Field-only iterator (skips the value decode per entry) for
    /// keys-only consumers like HKEYS / HSCAN NOVALUES on a hashtable-range hash.
    #[must_use]
    pub(crate) fn field_iter(&self) -> CompactFieldMapFieldIter<'_> {
        CompactFieldMapFieldIter { map: self, pos: 0 }
    }

    /// (frankenredis-ym6ih) Swap-remove the live entry at order position `pos`,
    /// whose index slot is `slot`. O(1) and probe-free: tombstone `slot`, move
    /// the last entry into the gap, and repoint *its* slot via the `slot_of`
    /// back-pointer (no re-probe, no owned-field allocation). Callers reclaim the
    /// dead arena bytes (`self.dead += entry_size`) and read any return value
    /// before calling. Order is NOT preserved.
    fn remove_at(&mut self, pos: usize, slot: usize) {
        self.slots[slot] = CFM_TOMB;
        self.tombs += 1;
        let last = self.order.len() - 1;
        if pos != last {
            self.order[pos] = self.order[last];
            let moved_slot = self.slot_of[last] as usize;
            self.slots[moved_slot] = (pos as u32) + 2;
            self.slot_of[pos] = moved_slot as u32;
        }
        self.order.pop();
        self.slot_of.pop();
        self.maybe_compact();
    }

    /// Order-preserving remove (HDEL on small/listpack-range hashes). O(n).
    pub(crate) fn shift_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        let pos = self.lookup(field)?;
        let off = self.order[pos];
        let (_, vr) = cfm_decode(&self.buf, off);
        let value = self.buf[vr].to_vec();
        self.dead += self.entry_size(off);
        self.order.remove(pos);
        // Positions shifted → rebuild the index from `order`.
        self.rehash(self.slots.len().max(8));
        self.maybe_compact();
        Some(value)
    }

    /// Unordered remove (HDEL on hashtable-range hashes, where order is
    /// unspecified). O(1): swap the last entry into the gap. Returns the removed
    /// value; use [`delete`](Self::delete) when the value is discarded.
    pub(crate) fn swap_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        let (pos, slot) = self.lookup_slot(field)?;
        let off = self.order[pos];
        let (_, vr) = cfm_decode(&self.buf, off);
        let value = self.buf[vr].to_vec();
        self.dead += self.entry_size(off);
        self.remove_at(pos, slot);
        Some(value)
    }

    /// (frankenredis-ym6ih) Unordered remove that does NOT allocate the removed
    /// value — for HDEL/SREM, which only need a removed/not-found flag. Otherwise
    /// identical to [`swap_remove`](Self::swap_remove). One probe + zero owned
    /// allocations per delete (vs the prior 3 probes + 2 allocs).
    pub(crate) fn delete(&mut self, field: &[u8]) -> bool {
        let Some((pos, slot)) = self.lookup_slot(field) else {
            return false;
        };
        self.dead += self.entry_size(self.order[pos]);
        self.remove_at(pos, slot);
        true
    }

    /// Swap-remove the entry at insertion-order position `idx`, returning its
    /// (field, value). O(1), order NOT preserved (matches `IndexMap::swap_remove_index`).
    pub(crate) fn remove_index(&mut self, idx: usize) -> Option<(Vec<u8>, Vec<u8>)> {
        if idx >= self.order.len() {
            return None;
        }
        let off = self.order[idx];
        let (fr, vr) = cfm_decode(&self.buf, off);
        let field = self.buf[fr].to_vec();
        let value = self.buf[vr].to_vec();
        self.dead += self.entry_size(off);
        let slot = self.slot_of[idx] as usize;
        self.remove_at(idx, slot);
        Some((field, value))
    }

    /// Reclaim dead arena bytes and/or shrink-rebuild the index when either has
    /// grown past half. Offsets change, so `order` + `slots` are rebuilt.
    fn maybe_compact(&mut self) {
        if self.dead * 2 > self.buf.len() && self.dead > 64 {
            let mut new_buf = Vec::with_capacity(self.buf.len() - self.dead);
            let mut new_order = Vec::with_capacity(self.order.len());
            for &off in &self.order {
                let (fr, vr) = cfm_decode(&self.buf, off);
                let new_off = new_buf.len() as u32;
                write_varint(&mut new_buf, fr.end - fr.start);
                new_buf.extend_from_slice(&self.buf[fr]);
                write_varint(&mut new_buf, vr.end - vr.start);
                new_buf.extend_from_slice(&self.buf[vr]);
                new_order.push(new_off);
            }
            self.buf = new_buf;
            self.order = new_order;
            self.dead = 0;
            self.rehash(self.slots.len().max(8));
        } else if self.tombs * 4 >= self.slots.len() {
            self.rehash(self.slots.len().max(8));
        }
    }
}

/// Insertion-order iterator over a [`CompactFieldMap`].
#[allow(dead_code)]
pub struct CompactFieldMapIter<'a> {
    map: &'a CompactFieldMap,
    pos: usize,
}

#[allow(dead_code)]
impl<'a> Iterator for CompactFieldMapIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        let pair = self.map.get_index(self.pos)?;
        self.pos += 1;
        Some(pair)
    }
}

/// (CrimsonHawk) Field-only insertion-order iterator over a [`CompactFieldMap`],
/// decoding just the field (no value varint/slice) per entry.
pub struct CompactFieldMapFieldIter<'a> {
    map: &'a CompactFieldMap,
    pos: usize,
}

impl<'a> Iterator for CompactFieldMapFieldIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        let f = self.map.field_at(self.pos)?;
        self.pos += 1;
        Some(f)
    }
}

/// (frankenredis-ideww) Member-only compact set for the hashtable-range set
/// encoding — a thin wrapper over [`CompactFieldMap`] (members map to an empty
/// value), so it inherits the arena+index compactness (vs the heavy `IndexSet`)
/// and O(1) membership while keeping `IndexSet`'s insertion-order semantics
/// byte-for-byte. Drop-in for the `IndexSet<SetMember>` surface used by
/// `GenericSet::Hash`.
#[derive(Clone, Debug, Default)]
pub struct CompactStrSet {
    inner: CompactFieldMap,
}

impl CompactStrSet {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// (frankenredis-cfm-presize) Pre-sized empty set for `entries` inserts and
    /// ~`buf_bytes` of member payload — delegates to
    /// [`CompactFieldMap::with_capacity`] so the bulk unique-members build skips
    /// incremental `rehash`/realloc. Byte-identical to `new()` + the same inserts.
    #[must_use]
    pub(crate) fn with_capacity(entries: usize, buf_bytes: usize) -> Self {
        Self {
            inner: CompactFieldMap::with_capacity(entries, buf_bytes),
        }
    }

    /// Release capacity reserved past the live members (see
    /// [`CompactFieldMap::shrink_to_fit`]). Byte-identical membership + order.
    pub(crate) fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub(crate) fn contains(&self, member: &[u8]) -> bool {
        self.inner.contains_key(member)
    }

    #[must_use]
    pub(crate) fn get_index(&self, idx: usize) -> Option<&[u8]> {
        self.inner.field_at(idx)
    }

    /// Insert `member`; returns `true` if it was newly added (matches `IndexSet::insert`).
    pub(crate) fn insert(&mut self, member: &[u8]) -> bool {
        self.inner.insert(member, b"").is_none()
    }

    pub(crate) fn shift_remove(&mut self, member: &[u8]) -> bool {
        self.inner.shift_remove(member).is_some()
    }

    pub(crate) fn swap_remove(&mut self, member: &[u8]) -> bool {
        // (frankenredis-ym6ih) Members carry an empty value, so route through the
        // value-free `delete` (one probe, no allocation per remove).
        self.inner.delete(member)
    }

    /// Swap-remove the member at insertion-order position `idx` (matches
    /// `IndexSet::swap_remove_index`); powers SPOP/SRANDMEMBER.
    pub(crate) fn swap_remove_index(&mut self, idx: usize) -> Option<Vec<u8>> {
        self.inner.remove_index(idx).map(|(m, _)| m)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let survivors: Vec<Vec<u8>> = self
            .inner
            .iter()
            .filter(|(m, _)| keep(m))
            .map(|(m, _)| m.to_vec())
            .collect();
        let mut next = CompactFieldMap::new();
        for m in &survivors {
            next.insert(m, b"");
        }
        self.inner = next;
    }

    #[must_use]
    pub(crate) fn iter(&self) -> CompactStrSetIter<'_> {
        CompactStrSetIter {
            map: &self.inner,
            pos: 0,
        }
    }
}

/// Insertion-order iterator over a [`CompactStrSet`]. Yields member bytes via
/// the value-skipping `field_at` (members carry an empty value). (CrimsonHawk)
pub struct CompactStrSetIter<'a> {
    map: &'a CompactFieldMap,
    pos: usize,
}

impl<'a> Iterator for CompactStrSetIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        let m = self.map.field_at(self.pos)?;
        self.pos += 1;
        Some(m)
    }
}

/// (frankenredis-p8wd1) Compact storage for ONE stream entry's fields: an
/// ORDERED list of (field, value) byte pairs packed contiguously into a single
/// buffer (`[flen varint][field][vlen varint][value]` × count), instead of a
/// `Vec<(Vec<u8>,Vec<u8>)>` — which costs a 24-byte `Vec` header + a heap block
/// per field AND per value (~6 allocs / entry). Stream fields are an ordered
/// list (NO dedup — field names may repeat) read as a whole (XRANGE/XREAD), so
/// no key index is needed; mirrors redis's listpack-packed stream entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // wired into Value::Stream storage in a follow-up (frankenredis-p8wd1)
pub struct PackedStreamFields {
    buf: Vec<u8>,
    count: u32,
}

#[allow(dead_code)]
impl PackedStreamFields {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pack an ordered list of (field, value) pairs.
    #[must_use]
    pub fn from_pairs<F: AsRef<[u8]>, V: AsRef<[u8]>>(pairs: &[(F, V)]) -> Self {
        let cap: usize = pairs
            .iter()
            .map(|(f, v)| f.as_ref().len() + v.as_ref().len() + 4)
            .sum();
        let mut buf = Vec::with_capacity(cap);
        for (f, v) in pairs {
            write_varint(&mut buf, f.as_ref().len());
            buf.extend_from_slice(f.as_ref());
            write_varint(&mut buf, v.as_ref().len());
            buf.extend_from_slice(v.as_ref());
        }
        Self {
            buf,
            count: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate the (field, value) pairs in insertion order, borrowed.
    #[must_use]
    pub fn iter(&self) -> PackedStreamFieldsIter<'_> {
        PackedStreamFieldsIter {
            buf: &self.buf,
            pos: 0,
        }
    }

    /// Materialize back to owned (field, value) pairs (the former representation).
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()
    }
}

/// Borrowing iterator over a [`PackedStreamFields`]'s (field, value) pairs.
#[allow(dead_code)]
pub struct PackedStreamFieldsIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for PackedStreamFieldsIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (flen, p) = read_varint(self.buf, self.pos);
        let (fs, fe) = (p, p + flen);
        let (vlen, p2) = read_varint(self.buf, fe);
        let (vs, ve) = (p2, p2 + vlen);
        self.pos = ve;
        Some((&self.buf[fs..fe], &self.buf[vs..ve]))
    }
}

// ─────────────────────── packed stream LOG (arena per stream) ───────────────

const PACKED_STREAM_NODE_MAX_ENTRIES: usize = 100;

/// (frankenredis-p8wd1 step 3) A whole stream's entries stored as ONE shared
/// arena plus a sorted stream-node index, replacing
/// `BTreeMap<StreamId, PackedStreamFields>` (a separate heap allocation **and**
/// a 28-byte value — `Vec` header + count — *per entry*).
///
/// Each entry's fields are appended to `arena` in the exact
/// `[flen varint][field][vlen varint][value]` × count layout of
/// [`PackedStreamFields`], so the bytes (and therefore DUMP / DEBUG DIGEST /
/// XRANGE output) are byte-identical; only the *container* changes. The index
/// value shrinks from 28 bytes + a per-entry heap block to a 16-byte
/// [`FieldSpan`] into the shared arena. XADD appends (stream IDs are monotonic),
/// XDEL/XTRIM remove from the index and mark the freed span dead; the arena is
/// compacted once dead bytes exceed half its length.
///
/// Reads hand back a [`FieldsRef`] view whose `iter`/`to_pairs`/`len` mirror
/// `PackedStreamFields`, so the call sites are unchanged.
#[derive(Clone, Debug, Default)]
pub struct PackedStreamLog {
    arena: Vec<u8>,
    /// Non-tail nodes indexed by their first entry id. The active tail is kept
    /// separately so monotonic XADD mutates it without traversing the B-tree;
    /// arbitrary insert/remove operations temporarily fold the tail back into
    /// this exact general-purpose directory.
    nodes: std::collections::BTreeMap<(u64, u64), StreamNode>,
    tail: Option<StreamNode>,
    /// (frankenredis-p8wd1 step 4 / Redis SAMEFIELDS) Interned field NAMES for
    /// this stream, indexed by the per-entry `[field_idx]` written into the
    /// arena. Stream schemas are near-always stable, so each name is stored ONCE
    /// for the whole stream instead of repeated in every entry — for a
    /// 1000-entry stream with fields `user_id`/`event`/`ts` that turns ~3 names
    /// per entry into 3 names total + a 1-byte index per field. Bounded by the
    /// number of DISTINCT field names the stream has ever used (tiny + stable for
    /// normal schemas; it is append-only so indices stay valid across arena
    /// compaction — a churning schema is the only case it grows past the live
    /// set). NOT serialized — DUMP/RESTORE/DIGEST go through `to_pairs`, which
    /// reconstructs the names, so the observable bytes are unchanged.
    field_dict: Vec<Box<[u8]>>,
    /// Bytes in `arena` belonging to removed/overwritten entries (compaction hint).
    dead: usize,
    len: usize,
}

/// Logical equality: the SAME ids in order with the SAME decoded (field, value)
/// pairs. (Two logs with equal content may differ in raw `arena`/`field_dict`
/// after compaction or different field-insertion order, so a derived `PartialEq`
/// would be wrong.)
impl PartialEq for PackedStreamLog {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .zip(other.iter())
                .all(|((ia, fa), (ib, fb))| ia == ib && fa.to_pairs() == fb.to_pairs())
    }
}

#[derive(Clone, Copy, Debug)]
struct FieldSpan {
    /// Offset of this entry's packed bytes in the arena.
    off: usize,
    /// Length of the packed bytes.
    len: u32,
    /// Number of (field, value) pairs.
    count: u32,
}

#[derive(Clone, Debug)]
struct StreamNode {
    entries: Vec<StreamNodeEntry>,
}

#[derive(Clone, Copy, Debug)]
struct StreamNodeEntry {
    id: (u64, u64),
    span: FieldSpan,
}

impl StreamNode {
    fn with_entry(id: (u64, u64), span: FieldSpan) -> Self {
        let mut entries = Vec::with_capacity(PACKED_STREAM_NODE_MAX_ENTRIES);
        entries.push(StreamNodeEntry { id, span });
        Self { entries }
    }

    fn first_id(&self) -> Option<(u64, u64)> {
        self.entries.first().map(|entry| entry.id)
    }

    fn last_id(&self) -> Option<(u64, u64)> {
        self.entries.last().map(|entry| entry.id)
    }

    fn position(&self, id: (u64, u64)) -> Result<usize, usize> {
        self.entries.binary_search_by_key(&id, |entry| entry.id)
    }
}

/// A borrowed view over one entry's packed fields. The arena holds
/// `[field_idx varint][vlen varint][value]` per field; the field NAME is
/// recovered from the owning log's `field_dict`. Mirrors the read surface of
/// [`PackedStreamFields`] so stream call sites need no change.
#[derive(Clone, Copy)]
pub struct FieldsRef<'a> {
    buf: &'a [u8],
    dict: &'a [Box<[u8]>],
    count: u32,
}

impl<'a> FieldsRef<'a> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate the (field, value) pairs in insertion order, borrowed.
    #[must_use]
    pub fn iter(&self) -> FieldsRefIter<'a> {
        FieldsRefIter {
            buf: self.buf,
            dict: self.dict,
            pos: 0,
        }
    }

    /// Materialize to owned (field, value) pairs.
    #[must_use]
    pub fn to_pairs(self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.iter().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()
    }
}

/// Borrowing iterator over a [`FieldsRef`]'s (field, value) pairs. Decodes
/// `[field_idx][vlen][value]` and resolves the name via the field dict.
pub struct FieldsRefIter<'a> {
    buf: &'a [u8],
    dict: &'a [Box<[u8]>],
    pos: usize,
}

impl<'a> Iterator for FieldsRefIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (idx, p) = read_varint(self.buf, self.pos);
        let (vlen, p2) = read_varint(self.buf, p);
        let (vs, ve) = (p2, p2 + vlen);
        self.pos = ve;
        let name: &[u8] = self.dict.get(idx).map_or(&[][..], |n| n);
        Some((name, &self.buf[vs..ve]))
    }
}

impl PackedStreamLog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn span_slice(&self, span: &FieldSpan) -> FieldsRef<'_> {
        FieldsRef {
            buf: &self.arena[span.off..span.off + span.len as usize],
            dict: &self.field_dict,
            count: span.count,
        }
    }

    /// Return the index of `name` in the field dict, appending it if new. Linear
    /// scan: stream field-name cardinality is small and stable in practice.
    fn intern_field(&mut self, name: &[u8]) -> usize {
        if let Some(i) = self.field_dict.iter().position(|n| &**n == name) {
            i
        } else {
            self.field_dict.push(name.into());
            self.field_dict.len() - 1
        }
    }

    fn node_key_for(&self, id: (u64, u64)) -> Option<(u64, u64)> {
        self.nodes
            .range(..=id)
            .next_back()
            .and_then(|(key, node)| node.position(id).is_ok().then_some(*key))
    }

    fn flush_tail(&mut self) {
        let Some(node) = self.tail.take() else {
            return;
        };
        let key = node
            .first_id()
            .expect("active stream tail contains at least one entry");
        assert!(
            self.nodes.insert(key, node).is_none(),
            "active stream tail is absent from the completed-node directory"
        );
    }

    fn restore_tail(&mut self) {
        if self.tail.is_none() {
            self.tail = self.nodes.pop_last().map(|(_, node)| node);
        }
    }

    fn insert_new_span(&mut self, id: (u64, u64), span: FieldSpan) {
        if self.nodes.is_empty() {
            self.nodes.insert(id, StreamNode::with_entry(id, span));
            self.len += 1;
            return;
        }

        if let Some(key) = self.nodes.range(..=id).next_back().map(|(key, _)| *key) {
            let node_len = self.nodes.get(&key).map_or(0, |node| node.entries.len());
            let node_last = self.nodes.get(&key).and_then(StreamNode::last_id);
            if node_last.is_some_and(|last_id| id > last_id) {
                if node_len >= PACKED_STREAM_NODE_MAX_ENTRIES {
                    self.nodes.insert(id, StreamNode::with_entry(id, span));
                } else if let Some(node) = self.nodes.get_mut(&key) {
                    node.entries.push(StreamNodeEntry { id, span });
                }
                self.len += 1;
                return;
            }

            let mut node = self.nodes.remove(&key).expect("node key came from map");
            let pos = node
                .position(id)
                .expect_err("new stream id was checked absent before insertion");
            node.entries.insert(pos, StreamNodeEntry { id, span });
            self.reinsert_node_after_insert(node, pos);
            self.len += 1;
            return;
        }

        let first_key = self
            .nodes
            .keys()
            .next()
            .copied()
            .expect("non-empty stream index has a first node");
        let mut node = self.nodes.remove(&first_key).expect("first key exists");
        node.entries.insert(0, StreamNodeEntry { id, span });
        self.reinsert_node_after_insert(node, 0);
        self.len += 1;
    }

    fn reinsert_node_after_insert(&mut self, mut node: StreamNode, inserted_pos: usize) {
        if node.entries.len() > PACKED_STREAM_NODE_MAX_ENTRIES {
            let split_at = if inserted_pos == node.entries.len() - 1 {
                PACKED_STREAM_NODE_MAX_ENTRIES
            } else {
                node.entries.len() / 2
            };
            let right_entries = node.entries.split_off(split_at);
            let right = StreamNode {
                entries: right_entries,
            };
            if let Some(left_key) = node.first_id() {
                self.nodes.insert(left_key, node);
            }
            let right_key = right
                .first_id()
                .expect("split right node contains at least one entry");
            self.nodes.insert(right_key, right);
        } else {
            let key = node
                .first_id()
                .expect("reinserted stream node contains at least one entry");
            self.nodes.insert(key, node);
        }
    }

    fn insert_span_fallback(&mut self, id: (u64, u64), span: FieldSpan) -> bool {
        self.flush_tail();
        let replaced = if let Some(key) = self.node_key_for(id) {
            let old_len = {
                let node = self.nodes.get_mut(&key).expect("node key came from map");
                let pos = node.position(id).expect("node contains requested id");
                let old = std::mem::replace(&mut node.entries[pos].span, span);
                old.len as usize
            };
            self.dead += old_len;
            self.maybe_compact();
            true
        } else {
            self.insert_new_span(id, span);
            false
        };
        self.restore_tail();
        replaced
    }

    /// Insert/overwrite `id`'s fields (packed into the arena). Returns `true` if
    /// an entry with this id already existed (whose old bytes are now dead).
    pub fn insert<F: AsRef<[u8]>, V: AsRef<[u8]>>(
        &mut self,
        id: (u64, u64),
        pairs: &[(F, V)],
    ) -> bool {
        let off = self.arena.len();
        for (f, v) in pairs {
            let idx = self.intern_field(f.as_ref());
            write_varint(&mut self.arena, idx);
            write_varint(&mut self.arena, v.as_ref().len());
            self.arena.extend_from_slice(v.as_ref());
        }
        let span = FieldSpan {
            off,
            len: u32::try_from(self.arena.len() - off).unwrap_or(u32::MAX),
            count: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
        };

        // XADD appends IDs strictly above the stream watermark. Keep that active
        // tail outside the B-tree so 99 of every 100 default-sized appends only
        // touch the tail Vec. Direct callers that overwrite, insert out of order,
        // or remove entries fold the tail into the exact B-tree fallback first.
        if self.tail.is_none() {
            debug_assert!(self.nodes.is_empty());
            self.tail = Some(StreamNode::with_entry(id, span));
            self.len += 1;
            return false;
        }

        let mut strictly_after_last = false;
        let appended_to_last = {
            let node = self.tail.as_mut().expect("stream tail is non-empty");
            let last_id = node
                .last_id()
                .expect("the active stream tail contains at least one entry");
            if id > last_id {
                strictly_after_last = true;
                if node.entries.len() < PACKED_STREAM_NODE_MAX_ENTRIES {
                    node.entries.push(StreamNodeEntry { id, span });
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if appended_to_last {
            self.len += 1;
            return false;
        }
        if strictly_after_last {
            let full_tail = self
                .tail
                .replace(StreamNode::with_entry(id, span))
                .expect("stream tail is non-empty");
            let key = full_tail
                .first_id()
                .expect("promoted stream node contains at least one entry");
            assert!(
                self.nodes.insert(key, full_tail).is_none(),
                "promoted stream node key is unique"
            );
            self.len += 1;
            return false;
        }

        self.insert_span_fallback(id, span)
    }

    /// Exact pre-monotonic-tier insertion, retained only for same-binary benchmark/test proof.
    #[cfg(any(test, feature = "bench-reference"))]
    pub fn bench_insert_fallback<F: AsRef<[u8]>, V: AsRef<[u8]>>(
        &mut self,
        id: (u64, u64),
        pairs: &[(F, V)],
    ) -> bool {
        let off = self.arena.len();
        for (f, v) in pairs {
            let idx = self.intern_field(f.as_ref());
            write_varint(&mut self.arena, idx);
            write_varint(&mut self.arena, v.as_ref().len());
            self.arena.extend_from_slice(v.as_ref());
        }
        let span = FieldSpan {
            off,
            len: u32::try_from(self.arena.len() - off).unwrap_or(u32::MAX),
            count: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
        };
        self.insert_span_fallback(id, span)
    }

    #[cfg(any(test, feature = "bench-reference"))]
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::type_complexity)]
    pub fn bench_node_layout(&self) -> Vec<((u64, u64), Vec<(u64, u64)>)> {
        self.nodes
            .values()
            .chain(self.tail.iter())
            .map(|node| {
                (
                    node.first_id()
                        .expect("stream directory contains only non-empty nodes"),
                    node.entries.iter().map(|entry| entry.id).collect(),
                )
            })
            .collect()
    }

    /// Bulk-build a log from entries supplied in **strictly id-ascending** order
    /// (the RESTORE / RDB-load case — upstream serializes stream entries sorted).
    /// Produces an arena / `field_dict` / node index byte-identical to inserting
    /// the same entries one at a time, but in O(n): the node index is filled in
    /// `PACKED_STREAM_NODE_MAX_ENTRIES`-sized chunks — the exact boundary
    /// [`Self::insert_new_span`]'s append branch produces — with no per-entry
    /// `BTreeMap` range lookup or in-node binary search (`node_key_for`, the
    /// stream-RESTORE hot path). Shared by the RESTORE command and the RDB-file /
    /// DEBUG RELOAD loader. The caller MUST guarantee strictly-increasing ids;
    /// verify first and fall back to per-entry [`Self::insert`] otherwise (that
    /// path tolerates reordering / overwrites).
    #[must_use]
    pub fn from_sorted_entries<'a, F, V, I>(entries: I) -> Self
    where
        F: AsRef<[u8]> + 'a,
        V: AsRef<[u8]> + 'a,
        I: IntoIterator<Item = ((u64, u64), &'a [(F, V)])>,
    {
        let mut log = Self::new();
        let mut node_entries: Vec<StreamNodeEntry> =
            Vec::with_capacity(PACKED_STREAM_NODE_MAX_ENTRIES);
        let mut node_first: Option<(u64, u64)> = None;
        let mut total = 0usize;
        for (id, pairs) in entries {
            let off = log.arena.len();
            for (f, v) in pairs {
                let idx = log.intern_field(f.as_ref());
                write_varint(&mut log.arena, idx);
                write_varint(&mut log.arena, v.as_ref().len());
                log.arena.extend_from_slice(v.as_ref());
            }
            let span = FieldSpan {
                off,
                len: u32::try_from(log.arena.len() - off).unwrap_or(u32::MAX),
                count: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
            };
            node_first.get_or_insert(id);
            node_entries.push(StreamNodeEntry { id, span });
            total += 1;
            if node_entries.len() == PACKED_STREAM_NODE_MAX_ENTRIES {
                let key = node_first.take().expect("node_first set on first push");
                let full = std::mem::replace(
                    &mut node_entries,
                    Vec::with_capacity(PACKED_STREAM_NODE_MAX_ENTRIES),
                );
                log.nodes.insert(key, StreamNode { entries: full });
            }
        }
        if let Some(key) = node_first {
            log.nodes.insert(
                key,
                StreamNode {
                    entries: node_entries,
                },
            );
        }
        log.len = total;
        log.restore_tail();
        log
    }

    #[must_use]
    pub fn get(&self, id: (u64, u64)) -> Option<FieldsRef<'_>> {
        if let Some(node) = self.tail.as_ref()
            && let Ok(pos) = node.position(id)
        {
            return Some(self.span_slice(&node.entries[pos].span));
        }
        let key = self.node_key_for(id)?;
        let node = self.nodes.get(&key)?;
        let pos = node.position(id).ok()?;
        Some(self.span_slice(&node.entries[pos].span))
    }

    #[must_use]
    pub fn contains_key(&self, id: (u64, u64)) -> bool {
        self.tail
            .as_ref()
            .is_some_and(|node| node.position(id).is_ok())
            || self.node_key_for(id).is_some()
    }

    /// Remove `id`; returns `true` if it existed. The freed span is marked dead
    /// and the arena compacted once dead bytes exceed half its length.
    pub fn remove(&mut self, id: (u64, u64)) -> bool {
        if let Some(position) = self.tail.as_ref().and_then(|node| node.position(id).ok()) {
            let removed = self
                .tail
                .as_mut()
                .expect("stream tail is present")
                .entries
                .remove(position);
            self.len -= 1;
            self.dead += removed.span.len as usize;
            if self
                .tail
                .as_ref()
                .is_some_and(|node| node.entries.is_empty())
            {
                self.tail = self.nodes.pop_last().map(|(_, node)| node);
            }
            self.maybe_compact();
            return true;
        }

        let Some(key) = self.node_key_for(id) else {
            return false;
        };
        let mut node = self.nodes.remove(&key).expect("node key came from map");
        let pos = node.position(id).expect("node contains requested id");
        let removed = node.entries.remove(pos);
        self.len -= 1;
        self.dead += removed.span.len as usize;
        if let Some(new_key) = node.first_id() {
            self.nodes.insert(new_key, node);
        }
        self.maybe_compact();
        true
    }

    #[must_use]
    pub fn last_id(&self) -> Option<(u64, u64)> {
        self.tail
            .as_ref()
            .and_then(StreamNode::last_id)
            .or_else(|| {
                self.nodes
                    .values()
                    .next_back()
                    .and_then(StreamNode::last_id)
            })
    }

    #[must_use]
    pub fn first_id(&self) -> Option<(u64, u64)> {
        self.nodes
            .values()
            .next()
            .and_then(StreamNode::first_id)
            .or_else(|| self.tail.as_ref().and_then(StreamNode::first_id))
    }

    /// Smallest id with its fields (BTreeMap-compatible).
    #[must_use]
    pub fn first_key_value(&self) -> Option<(&(u64, u64), FieldsRef<'_>)> {
        self.nodes
            .values()
            .next()
            .or(self.tail.as_ref())
            .and_then(|node| node.entries.first())
            .map(|entry| (&entry.id, self.span_slice(&entry.span)))
    }

    /// Largest id with its fields (BTreeMap-compatible).
    #[must_use]
    pub fn last_key_value(&self) -> Option<(&(u64, u64), FieldsRef<'_>)> {
        self.tail
            .as_ref()
            .or_else(|| self.nodes.values().next_back())
            .and_then(|node| node.entries.last())
            .map(|entry| (&entry.id, self.span_slice(&entry.span)))
    }

    /// Iterate `(&id, FieldsRef)` in ascending id order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&(u64, u64), FieldsRef<'_>)> {
        self.nodes
            .values()
            .chain(self.tail.iter())
            .flat_map(move |node| {
                node.entries
                    .iter()
                    .map(move |entry| (&entry.id, self.span_slice(&entry.span)))
            })
    }

    /// Iterate field views only (for the memory estimate).
    pub fn values(&self) -> impl Iterator<Item = FieldsRef<'_>> {
        self.iter().map(|(_, fields)| fields)
    }

    /// Iterate the stream ids in ascending order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &(u64, u64)> {
        self.nodes
            .values()
            .chain(self.tail.iter())
            .flat_map(|node| node.entries.iter().map(|entry| &entry.id))
    }

    /// Iterate `(&id, FieldsRef)` over a stream-id range; double-ended for
    /// XREVRANGE's `.rev()`.
    pub fn range<R: std::ops::RangeBounds<(u64, u64)>>(
        &self,
        bounds: R,
    ) -> impl DoubleEndedIterator<Item = (&(u64, u64), FieldsRef<'_>)> {
        let lower = match bounds.start_bound() {
            std::ops::Bound::Included(id) | std::ops::Bound::Excluded(id) => self
                .nodes
                .range(..=*id)
                .next_back()
                .map_or(*id, |(key, _)| *key),
            std::ops::Bound::Unbounded => (0, 0),
        };
        self.nodes
            .range(lower..)
            .map(|(_, node)| node)
            .chain(self.tail.iter())
            .flat_map(move |node| {
                node.entries
                    .iter()
                    .map(move |entry| (&entry.id, self.span_slice(&entry.span)))
            })
            .filter(move |(id, _)| stream_id_in_bounds(&bounds, id))
    }

    fn maybe_compact(&mut self) {
        if self.arena.len() > 64 && self.dead > self.arena.len() / 2 {
            self.compact();
        }
    }

    /// Rebuild the arena from the live spans (in id order), dropping dead bytes.
    fn compact(&mut self) {
        let mut new_arena = Vec::with_capacity(self.arena.len().saturating_sub(self.dead));
        for node in self.nodes.values_mut().chain(self.tail.iter_mut()) {
            for entry in &mut node.entries {
                let start = entry.span.off;
                let end = entry.span.off + entry.span.len as usize;
                let new_off = new_arena.len();
                new_arena.extend_from_slice(&self.arena[start..end]);
                entry.span.off = new_off;
            }
        }
        self.arena = new_arena;
        self.dead = 0;
    }
}

/// Frozen pre-`frankenredis-5tjc0` all-nodes-in-B-tree stream directory. This
/// type exists only so `xadd_append` can execute both layouts in one benchmark
/// binary; production code never contains or branches on the reference layout.
#[cfg(any(test, feature = "bench-reference"))]
#[derive(Clone, Debug, Default)]
#[doc(hidden)]
pub struct PackedStreamLogBTreeReference {
    arena: Vec<u8>,
    nodes: std::collections::BTreeMap<(u64, u64), StreamNode>,
    field_dict: Vec<Box<[u8]>>,
    dead: usize,
    len: usize,
}

#[cfg(any(test, feature = "bench-reference"))]
impl PackedStreamLogBTreeReference {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn span_slice(&self, span: &FieldSpan) -> FieldsRef<'_> {
        FieldsRef {
            buf: &self.arena[span.off..span.off + span.len as usize],
            dict: &self.field_dict,
            count: span.count,
        }
    }

    fn intern_field(&mut self, name: &[u8]) -> usize {
        if let Some(index) = self
            .field_dict
            .iter()
            .position(|candidate| &**candidate == name)
        {
            index
        } else {
            self.field_dict.push(name.into());
            self.field_dict.len() - 1
        }
    }

    fn node_key_for(&self, id: (u64, u64)) -> Option<(u64, u64)> {
        self.nodes
            .range(..=id)
            .next_back()
            .and_then(|(key, node)| node.position(id).is_ok().then_some(*key))
    }

    fn insert_new_span(&mut self, id: (u64, u64), span: FieldSpan) {
        if self.nodes.is_empty() {
            self.nodes.insert(id, StreamNode::with_entry(id, span));
            self.len += 1;
            return;
        }

        if let Some(key) = self.nodes.range(..=id).next_back().map(|(key, _)| *key) {
            let node_len = self.nodes.get(&key).map_or(0, |node| node.entries.len());
            let node_last = self.nodes.get(&key).and_then(StreamNode::last_id);
            if node_last.is_some_and(|last_id| id > last_id) {
                if node_len >= PACKED_STREAM_NODE_MAX_ENTRIES {
                    self.nodes.insert(id, StreamNode::with_entry(id, span));
                } else if let Some(node) = self.nodes.get_mut(&key) {
                    node.entries.push(StreamNodeEntry { id, span });
                }
                self.len += 1;
                return;
            }

            let mut node = self.nodes.remove(&key).expect("node key came from B-tree");
            let position = node
                .position(id)
                .expect_err("new stream id was checked absent before insertion");
            node.entries.insert(position, StreamNodeEntry { id, span });
            self.reinsert_node_after_insert(node, position);
            self.len += 1;
            return;
        }

        let first_key = self
            .nodes
            .keys()
            .next()
            .copied()
            .expect("non-empty stream index has a first node");
        let mut node = self.nodes.remove(&first_key).expect("first key exists");
        node.entries.insert(0, StreamNodeEntry { id, span });
        self.reinsert_node_after_insert(node, 0);
        self.len += 1;
    }

    fn reinsert_node_after_insert(&mut self, mut node: StreamNode, inserted_pos: usize) {
        if node.entries.len() > PACKED_STREAM_NODE_MAX_ENTRIES {
            let split_at = if inserted_pos == node.entries.len() - 1 {
                PACKED_STREAM_NODE_MAX_ENTRIES
            } else {
                node.entries.len() / 2
            };
            let right_entries = node.entries.split_off(split_at);
            let right = StreamNode {
                entries: right_entries,
            };
            if let Some(left_key) = node.first_id() {
                self.nodes.insert(left_key, node);
            }
            let right_key = right
                .first_id()
                .expect("split right node contains at least one entry");
            self.nodes.insert(right_key, right);
        } else {
            let key = node
                .first_id()
                .expect("reinserted stream node contains at least one entry");
            self.nodes.insert(key, node);
        }
    }

    fn insert_span_fallback(&mut self, id: (u64, u64), span: FieldSpan) -> bool {
        if let Some(key) = self.node_key_for(id) {
            let old_len = {
                let node = self.nodes.get_mut(&key).expect("node key came from B-tree");
                let position = node.position(id).expect("node contains requested id");
                let old = std::mem::replace(&mut node.entries[position].span, span);
                old.len as usize
            };
            self.dead += old_len;
            self.maybe_compact();
            true
        } else {
            self.insert_new_span(id, span);
            false
        }
    }

    pub fn insert<F: AsRef<[u8]>, V: AsRef<[u8]>>(
        &mut self,
        id: (u64, u64),
        pairs: &[(F, V)],
    ) -> bool {
        let off = self.arena.len();
        for (field, value) in pairs {
            let index = self.intern_field(field.as_ref());
            write_varint(&mut self.arena, index);
            write_varint(&mut self.arena, value.as_ref().len());
            self.arena.extend_from_slice(value.as_ref());
        }
        let span = FieldSpan {
            off,
            len: u32::try_from(self.arena.len() - off).unwrap_or(u32::MAX),
            count: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
        };
        if self.nodes.is_empty() {
            self.nodes.insert(id, StreamNode::with_entry(id, span));
            self.len += 1;
            return false;
        }

        let mut strictly_after_last = false;
        let appended_to_last = {
            let mut last_entry = self.nodes.last_entry().expect("stream index is non-empty");
            let node = last_entry.get_mut();
            let last_id = node
                .last_id()
                .expect("a stream index node contains at least one entry");
            if id > last_id {
                strictly_after_last = true;
                if node.entries.len() < PACKED_STREAM_NODE_MAX_ENTRIES {
                    node.entries.push(StreamNodeEntry { id, span });
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if appended_to_last {
            self.len += 1;
            return false;
        }
        if strictly_after_last {
            self.nodes.insert(id, StreamNode::with_entry(id, span));
            self.len += 1;
            return false;
        }

        self.insert_span_fallback(id, span)
    }

    pub fn remove(&mut self, id: (u64, u64)) -> bool {
        let Some(key) = self.node_key_for(id) else {
            return false;
        };
        let mut node = self.nodes.remove(&key).expect("node key came from B-tree");
        let position = node.position(id).expect("node contains requested id");
        let removed = node.entries.remove(position);
        self.len -= 1;
        self.dead += removed.span.len as usize;
        if let Some(new_key) = node.first_id() {
            self.nodes.insert(new_key, node);
        }
        self.maybe_compact();
        true
    }

    fn maybe_compact(&mut self) {
        if self.arena.len() > 64 && self.dead > self.arena.len() / 2 {
            let mut new_arena = Vec::with_capacity(self.arena.len().saturating_sub(self.dead));
            for node in self.nodes.values_mut() {
                for entry in &mut node.entries {
                    let start = entry.span.off;
                    let end = entry.span.off + entry.span.len as usize;
                    let new_off = new_arena.len();
                    new_arena.extend_from_slice(&self.arena[start..end]);
                    entry.span.off = new_off;
                }
            }
            self.arena = new_arena;
            self.dead = 0;
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn first_id(&self) -> Option<(u64, u64)> {
        self.nodes.values().next().and_then(StreamNode::first_id)
    }

    #[must_use]
    pub fn last_id(&self) -> Option<(u64, u64)> {
        self.nodes
            .values()
            .next_back()
            .and_then(StreamNode::last_id)
    }

    #[must_use]
    #[allow(clippy::type_complexity)]
    pub fn contents(&self) -> Vec<((u64, u64), Vec<(Vec<u8>, Vec<u8>)>)> {
        self.nodes
            .values()
            .flat_map(|node| {
                node.entries
                    .iter()
                    .map(|entry| (entry.id, self.span_slice(&entry.span).to_pairs()))
            })
            .collect()
    }

    #[must_use]
    #[allow(clippy::type_complexity)]
    pub fn layout(&self) -> Vec<((u64, u64), Vec<(u64, u64)>)> {
        self.nodes
            .iter()
            .map(|(key, node)| (*key, node.entries.iter().map(|entry| entry.id).collect()))
            .collect()
    }

    #[must_use]
    pub fn range_ids<R: std::ops::RangeBounds<(u64, u64)>>(&self, bounds: R) -> Vec<(u64, u64)> {
        let lower = match bounds.start_bound() {
            std::ops::Bound::Included(id) | std::ops::Bound::Excluded(id) => self
                .nodes
                .range(..=*id)
                .next_back()
                .map_or(*id, |(key, _)| *key),
            std::ops::Bound::Unbounded => (0, 0),
        };
        self.nodes
            .range(lower..)
            .flat_map(|(_, node)| node.entries.iter().map(|entry| entry.id))
            .filter(|id| stream_id_in_bounds(&bounds, id))
            .collect()
    }
}

fn stream_id_in_bounds<R: std::ops::RangeBounds<(u64, u64)> + ?Sized>(
    bounds: &R,
    id: &(u64, u64),
) -> bool {
    let start_ok = match bounds.start_bound() {
        std::ops::Bound::Included(start) => id >= start,
        std::ops::Bound::Excluded(start) => id > start,
        std::ops::Bound::Unbounded => true,
    };
    let end_ok = match bounds.end_bound() {
        std::ops::Bound::Included(end) => id <= end,
        std::ops::Bound::Excluded(end) => id < end,
        std::ops::Bound::Unbounded => true,
    };
    start_ok && end_ok
}

// ───────────────────────── packed string MAP (for small hashes) ────────────

/// Packed field→value map for SMALL hashes: a sequence of
/// `[vint klen][k][vint vlen][v]` records in insertion order, one allocation
/// instead of an `IndexMap` (heap block + hash slot per field). Mirrors
/// `PackedStrSet`; insert of an existing field keeps its position and replaces
/// the value in place (matching `IndexMap::insert`), so HGETALL/HKEYS/HVALS
/// order is byte-for-byte unchanged. (frankenredis-9mh3o step 3)
#[derive(Clone, Debug, Default)]
pub struct PackedStrMap {
    buf: Vec<u8>,
    len: usize,
}

/// Byte offsets of one record located by field: `(record_start, value_enc_start,
/// value_start, value_end)` where `record_start` begins `[klen]`,
/// `value_enc_start` begins `[vlen]`, `value_start..value_end` is the raw value.
struct Located {
    record_start: usize,
    value_enc_start: usize,
    value_start: usize,
    value_end: usize,
}

impl PackedStrMap {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }

    fn locate(&self, field: &[u8]) -> Option<Located> {
        let mut pos = 0;
        while pos < self.buf.len() {
            let record_start = pos;
            let (klen, k_start) = read_varint(&self.buf, pos);
            let k_end = k_start + klen;
            let (vlen, v_start) = read_varint(&self.buf, k_end);
            let v_end = v_start + vlen;
            if self.buf[k_start..k_end] == *field {
                return Some(Located {
                    record_start,
                    value_enc_start: k_end,
                    value_start: v_start,
                    value_end: v_end,
                });
            }
            pos = v_end;
        }
        None
    }

    #[must_use]
    pub fn get(&self, field: &[u8]) -> Option<&[u8]> {
        self.locate(field)
            .map(|l| &self.buf[l.value_start..l.value_end])
    }

    #[must_use]
    pub fn contains_key(&self, field: &[u8]) -> bool {
        self.locate(field).is_some()
    }

    /// Insert/overwrite `field`→`value`. Returns the previous value if the field
    /// existed (its position is preserved, value replaced in place); `None` if
    /// newly added (appended). Matches `IndexMap::insert`.
    pub fn insert(&mut self, field: Vec<u8>, value: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(l) = self.locate(&field) {
            let old = self.buf[l.value_start..l.value_end].to_vec();
            let mut encoded = Vec::with_capacity(value.len() + 2);
            write_varint(&mut encoded, value.len());
            encoded.extend_from_slice(&value);
            self.buf.splice(l.value_enc_start..l.value_end, encoded);
            Some(old)
        } else {
            write_varint(&mut self.buf, field.len());
            self.buf.extend_from_slice(&field);
            write_varint(&mut self.buf, value.len());
            self.buf.extend_from_slice(&value);
            self.len += 1;
            None
        }
    }

    /// (frankenredis-qxfmr) Append a guaranteed-NEW field/value to the end of the
    /// buffer WITHOUT the O(n) `locate` scan `insert` performs. Caller MUST
    /// guarantee the field is not already present — appending a duplicate would
    /// create two records for the same field. Byte-identical to `insert(field,
    /// value)` on a field that does not yet exist; used to bulk-build a fresh map
    /// from already-unique pairs in one O(n) pass instead of N×O(n) inserts.
    pub fn append(&mut self, field: &[u8], value: &[u8]) {
        write_varint(&mut self.buf, field.len());
        self.buf.extend_from_slice(field);
        write_varint(&mut self.buf, value.len());
        self.buf.extend_from_slice(value);
        self.len += 1;
    }

    /// Borrowed-field upsert for callers that only need "was this field new?"
    /// instead of the previous value. Existing-field updates preserve the record
    /// position exactly like `IndexMap::insert`, but avoid materializing the
    /// field key and old value.
    pub fn insert_borrowed(&mut self, field: &[u8], value: Vec<u8>) -> bool {
        if let Some(l) = self.locate(field) {
            let (value_len_prefix, value_len_prefix_len) = encode_varint_array(value.len());
            let new_encoded_len = value_len_prefix_len + value.len();
            if new_encoded_len == l.value_end - l.value_enc_start {
                let value_start = l.value_enc_start + value_len_prefix_len;
                self.buf[l.value_enc_start..value_start]
                    .copy_from_slice(&value_len_prefix[..value_len_prefix_len]);
                self.buf[value_start..l.value_end].copy_from_slice(&value);
            } else {
                let mut encoded = Vec::with_capacity(new_encoded_len);
                encoded.extend_from_slice(&value_len_prefix[..value_len_prefix_len]);
                encoded.extend_from_slice(&value);
                self.buf.splice(l.value_enc_start..l.value_end, encoded);
            }
            false
        } else {
            write_varint(&mut self.buf, field.len());
            self.buf.extend_from_slice(field);
            write_varint(&mut self.buf, value.len());
            self.buf.extend_from_slice(&value);
            self.len += 1;
            true
        }
    }

    /// Remove `field`; returns its value if present. Survivors keep order.
    pub fn shift_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        let l = self.locate(field)?;
        let old = self.buf[l.value_start..l.value_end].to_vec();
        self.buf.drain(l.record_start..l.value_end);
        self.len -= 1;
        Some(old)
    }

    /// nth (field, value) in insertion order (powers HRANDFIELD index selection).
    #[must_use]
    pub fn get_index(&self, idx: usize) -> Option<(&[u8], &[u8])> {
        self.iter().nth(idx)
    }

    #[must_use]
    pub fn iter(&self) -> PackedStrMapIter<'_> {
        PackedStrMapIter {
            buf: &self.buf,
            pos: 0,
        }
    }
}

impl FromIterator<(Vec<u8>, Vec<u8>)> for PackedStrMap {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, Vec<u8>)>>(iter: I) -> Self {
        let mut m = Self::new();
        for (k, v) in iter {
            m.insert(k, v);
        }
        m
    }
}

/// Borrowing iterator over (field, value) pairs, in insertion order.
pub struct PackedStrMapIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for PackedStrMapIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (klen, k_start) = read_varint(self.buf, self.pos);
        let k_end = k_start + klen;
        let (vlen, v_start) = read_varint(self.buf, k_end);
        let v_end = v_start + vlen;
        self.pos = v_end;
        Some((&self.buf[k_start..k_end], &self.buf[v_start..v_end]))
    }
}

// ───────────────────────── packed string LIST (for small lists) ─────────────

/// Packed element list for SMALL lists: a sequence of `[vint len][elem]` records
/// in order, one allocation instead of a `VecDeque<Vec<u8>>` (heap block per
/// element). Front operations and random insert/remove shift the buffer (O(n)),
/// which is the right trade below the list-max-listpack threshold (n ≤ 128) and
/// MATCHES redis's listpack list node — redis's quicklist only avoids the shift
/// for LARGE lists by chaining listpack nodes, so a single packed buffer is the
/// correct small-list representation. (frankenredis-9mh3o step 4)
///
/// `allow(dead_code)`: the primitive + its VecDeque-equivalence proptest land
/// first; wiring it into `Value::List` is the follow-up (step 4b).
#[derive(Clone, Debug, Default)]
pub struct PackedList {
    buf: Vec<u8>,
    len: usize,
}

impl PackedList {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    #[expect(
        dead_code,
        reason = "packed-list public helper kept for follow-up wiring"
    )]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    #[expect(
        dead_code,
        reason = "packed-list public helper kept for follow-up wiring"
    )]
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }

    /// `(record_start, elem_start, elem_end)` of the `idx`-th element.
    fn bounds(&self, idx: usize) -> Option<(usize, usize, usize)> {
        if idx >= self.len {
            return None;
        }
        let mut pos = 0;
        for _ in 0..idx {
            let (elen, e_start) = read_varint(&self.buf, pos);
            pos = e_start + elen;
        }
        let record_start = pos;
        let (elen, e_start) = read_varint(&self.buf, pos);
        Some((record_start, e_start, e_start + elen))
    }

    fn encode(elem: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(elem.len() + 2);
        write_varint(&mut out, elem.len());
        out.extend_from_slice(elem);
        out
    }

    pub fn push_back(&mut self, elem: &[u8]) {
        write_varint(&mut self.buf, elem.len());
        self.buf.extend_from_slice(elem);
        self.len += 1;
    }

    pub fn push_front(&mut self, elem: &[u8]) {
        let enc = Self::encode(elem);
        self.buf.splice(0..0, enc);
        self.len += 1;
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        let (_rs, es, ee) = self.bounds(0)?;
        let out = self.buf[es..ee].to_vec();
        self.buf.drain(0..ee);
        self.len -= 1;
        Some(out)
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        let (rs, es, ee) = self.bounds(self.len.checked_sub(1)?)?;
        debug_assert_eq!(ee, self.buf.len(), "last record must end the buffer");
        let out = self.buf[es..ee].to_vec();
        self.buf.truncate(rs);
        self.len -= 1;
        Some(out)
    }

    /// (cc_fr) Batch LPOP-count: collect the first `count.min(len)` element values in order, then
    /// drain the whole front span in ONE `buf.drain` shift. `pop_front` × count re-shifts the
    /// remaining buffer on every call, so popping `count` of `n` is O(count·n) (quadratic when
    /// count ~ n); this is O(n). Byte-identical values + residual buffer to `count` `pop_front`s.
    pub fn drain_front_n(&mut self, count: usize) -> Vec<Vec<u8>> {
        let n = count.min(self.len);
        let mut out = Vec::with_capacity(n);
        let mut pos = 0;
        for _ in 0..n {
            let (elen, e_start) = read_varint(&self.buf, pos);
            let e_end = e_start + elen;
            out.push(self.buf[e_start..e_end].to_vec());
            pos = e_end;
        }
        self.buf.drain(0..pos);
        self.len -= n;
        out
    }

    /// (cc_fr) Batch RPOP-count: pop the last `count.min(len)` elements in POP ORDER (last element
    /// first, matching `pop_back` repeated). Scans ONCE to the split point, collects the tail, and
    /// `truncate`s — O(len). `pop_back` × count is O(count·len) because each `pop_back` re-scans from
    /// the front via `bounds(len-1)` (no backlen). Byte-identical values + residual to `count`
    /// `pop_back`s.
    pub fn drain_back_n(&mut self, count: usize) -> Vec<Vec<u8>> {
        let n = count.min(self.len);
        let keep = self.len - n;
        // Scan to the start of element `keep` (the first element to remove).
        let mut pos = 0;
        for _ in 0..keep {
            let (elen, e_start) = read_varint(&self.buf, pos);
            pos = e_start + elen;
        }
        let truncate_at = pos;
        // Collect the removed tail front-to-back, then reverse for pop_back order.
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let (elen, e_start) = read_varint(&self.buf, pos);
            let e_end = e_start + elen;
            out.push(self.buf[e_start..e_end].to_vec());
            pos = e_end;
        }
        self.buf.truncate(truncate_at);
        self.len -= n;
        out.reverse();
        out
    }

    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&[u8]> {
        let (_rs, es, ee) = self.bounds(idx)?;
        Some(&self.buf[es..ee])
    }

    /// Replace the element at `idx` (LSET); returns false if out of range.
    pub fn set(&mut self, idx: usize, elem: &[u8]) -> bool {
        let Some((rs, _es, ee)) = self.bounds(idx) else {
            return false;
        };
        self.buf.splice(rs..ee, Self::encode(elem));
        true
    }

    /// Insert `elem` BEFORE index `idx` (`idx == len` appends), matching
    /// `VecDeque::insert`.
    pub fn insert(&mut self, idx: usize, elem: &[u8]) {
        if idx >= self.len {
            self.push_back(elem);
            return;
        }
        let (rs, _es, _ee) = self.bounds(idx).expect("idx < len");
        self.buf.splice(rs..rs, Self::encode(elem));
        self.len += 1;
    }

    pub fn remove(&mut self, idx: usize) -> Option<Vec<u8>> {
        let (rs, es, ee) = self.bounds(idx)?;
        let out = self.buf[es..ee].to_vec();
        self.buf.drain(rs..ee);
        self.len -= 1;
        Some(out)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let survivors: Vec<Vec<u8>> = self
            .iter()
            .filter(|e| keep(e))
            .map(<[u8]>::to_vec)
            .collect();
        let mut nb = PackedList::with_capacity(self.buf.len());
        for e in &survivors {
            nb.push_back(e);
        }
        *self = nb;
    }

    #[must_use]
    pub fn iter(&self) -> PackedListIter<'_> {
        PackedListIter {
            buf: &self.buf,
            pos: 0,
        }
    }

    /// Iterator starting at element index `start`. A packed list is bounded by
    /// `PACKED_MAX_ENTRIES`, so the O(start) varint walk is trivially cheap.
    /// (frankenredis-3r9lz)
    pub fn iter_from(&self, start: usize) -> PackedListIter<'_> {
        let mut it = self.iter();
        for _ in 0..start {
            if it.next().is_none() {
                break;
            }
        }
        it
    }
}

impl<'a> FromIterator<&'a [u8]> for PackedList {
    fn from_iter<I: IntoIterator<Item = &'a [u8]>>(iter: I) -> Self {
        let mut l = PackedList::new();
        for e in iter {
            l.push_back(e);
        }
        l
    }
}

/// Borrowing iterator over packed list elements, front to back.
pub struct PackedListIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for PackedListIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (elen, e_start) = read_varint(self.buf, self.pos);
        let e_end = e_start + elen;
        self.pos = e_end;
        Some(&self.buf[e_start..e_end])
    }
}

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;

use fr_persist::listpack::ListpackValueSpan;

/// Storage for a list: a packed buffer while small, promoting to a chunked COW
/// deque (which keeps O(1) ends for large lists, redis's quicklist regime) past
/// the threshold. Drop-in for the former `VecDeque` — same front-to-back order
/// and identical push/pop/get/insert/remove/retain semantics, so
/// LRANGE/LINDEX/LPOP/etc. output is byte-for-byte unchanged.
/// (frankenredis-9mh3o step 4)
///
/// The large `Deque` payload is `Arc`-wrapped so that cloning a `ListValue`
/// (COPY, eviction sampling, any `Value::clone`) is an O(1) refcount bump
/// instead of a per-element heap clone of every `Vec<u8>` — redis pays a bulk
/// per-listpack-node memcpy at COPY time; we defer copying lazily to the first
/// mutation via `Arc::make_mut`. A uniquely-owned list (the normal push-built
/// path, refcount 1) make_mut's for free. A post-COPY write clones the outer
/// chunk directory and only the touched chunk (128 elements), not the whole
/// 50k-element list. (frankenredis-k8yfq / frankenredis-ng2b8.1)
#[derive(Clone, Debug)]
enum ListRepr {
    Packed(PackedList),
    Deque(Arc<ChunkedList>),
}

impl Default for ListRepr {
    fn default() -> Self {
        ListRepr::Packed(PackedList::new())
    }
}

const LIST_CHUNK_TARGET: usize = 128;

#[derive(Clone, Debug)]
enum ListChunk {
    Owned {
        elems: Arc<Vec<Vec<u8>>>,
        /// Exact listpack byte length if known. `0` means a mutable path touched
        /// the chunk and the value must be recomputed before append/seal.
        lp_bytes: u64,
        /// True when physical order is reversed so repeated LPUSH can append at
        /// the Vec tail instead of shifting the whole front chunk.
        front_biased: bool,
    },
    Listpack {
        bytes: Arc<Vec<u8>>,
        entries: Arc<Vec<ListpackValueSpan>>,
    },
}

impl ListChunk {
    fn from_vec(elems: Vec<Vec<u8>>) -> Self {
        let lp_bytes = owned_listpack_bytes(&elems);
        Self::Owned {
            elems: Arc::new(elems),
            lp_bytes,
            front_biased: false,
        }
    }

    fn from_front_vec(elems: Vec<Vec<u8>>) -> Self {
        let lp_bytes = owned_listpack_bytes(&elems);
        Self::Owned {
            elems: Arc::new(elems),
            lp_bytes,
            front_biased: true,
        }
    }

    fn from_listpack(bytes: Vec<u8>, entries: Vec<ListpackValueSpan>) -> Self {
        Self::Listpack {
            bytes: Arc::new(bytes),
            entries: Arc::new(entries),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Owned { elems, .. } => elems.len(),
            Self::Listpack { entries, .. } => entries.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, idx: usize) -> Option<&[u8]> {
        match self {
            Self::Owned {
                elems,
                front_biased,
                ..
            } => {
                if *front_biased {
                    elems
                        .get(elems.len().checked_sub(1 + idx)?)
                        .map(Vec::as_slice)
                } else {
                    elems.get(idx).map(Vec::as_slice)
                }
            }
            Self::Listpack { bytes, entries } => {
                entries.get(idx).map(|entry| entry.as_bytes(bytes))
            }
        }
    }

    fn make_mut(&mut self) -> &mut Vec<Vec<u8>> {
        if let Self::Listpack { bytes, entries } = self {
            let elems = entries
                .iter()
                .map(|entry| entry.as_bytes(bytes).to_vec())
                .collect();
            *self = Self::from_vec(elems);
        }
        match self {
            Self::Owned {
                elems,
                lp_bytes,
                front_biased,
            } => {
                *lp_bytes = 0;
                let elems = Arc::make_mut(elems);
                if *front_biased {
                    elems.reverse();
                    *front_biased = false;
                }
                elems
            }
            Self::Listpack { .. } => unreachable!("packed listpack node was materialized"),
        }
    }

    /// (frankenredis-99fwc) Seal a FULL `Owned` chunk into the compact
    /// `Listpack` representation — one packed blob instead of a `Vec<u8>` (24B
    /// header + a heap block) PER element. Called when a chunk becomes interior
    /// (a fresh chunk is started at the same end), so it is never appended to
    /// again; a later in-place mutation (`make_mut`) transparently re-materializes
    /// it. No-op for an already-`Listpack`/empty chunk or an over-budget encode.
    fn seal_if_owned(&mut self, fill: i64) {
        let Self::Owned {
            elems,
            lp_bytes,
            front_biased,
        } = self
        else {
            return;
        };
        if elems.is_empty() {
            return;
        }
        if *lp_bytes == 0 {
            *lp_bytes = owned_listpack_bytes(elems);
        }
        if list_node_exceeds_limit(fill, *lp_bytes, elems.len() as u64) {
            return;
        }
        let slices: Vec<&[u8]> = if *front_biased {
            elems.iter().rev().map(Vec::as_slice).collect()
        } else {
            elems.iter().map(Vec::as_slice).collect()
        };
        if let Some(blob) = fr_persist::encode_listpack_strings_blob(&slices)
            && let Ok(spans) = fr_persist::listpack::decode_value_spans(&blob)
        {
            *self = Self::from_listpack(blob, spans);
        }
    }

    fn accepts_append(&mut self, elem: &[u8], fill: i64) -> bool {
        match self {
            Self::Owned {
                elems, lp_bytes, ..
            } => {
                if elems.is_empty() {
                    return true;
                }
                if *lp_bytes == 0 {
                    *lp_bytes = owned_listpack_bytes(elems);
                }
                quicklist_packed_node_accepts_local(elems.len(), *lp_bytes, elem.len(), fill)
            }
            Self::Listpack { bytes, entries } => quicklist_packed_node_accepts_local(
                entries.len(),
                bytes.len() as u64,
                elem.len(),
                fill,
            ),
        }
    }

    fn push_back_owned(&mut self, elem: Vec<u8>) {
        let added = list_lp_entry_bytes(&elem);
        if let Self::Listpack { bytes, entries } = self {
            let elems = entries
                .iter()
                .map(|entry| entry.as_bytes(bytes).to_vec())
                .collect();
            *self = Self::from_vec(elems);
        }
        if let Self::Owned {
            elems,
            lp_bytes,
            front_biased,
        } = self
        {
            if *lp_bytes == 0 {
                *lp_bytes = owned_listpack_bytes(elems);
            }
            let elems = Arc::make_mut(elems);
            if *front_biased {
                elems.insert(0, elem);
            } else {
                elems.push(elem);
            }
            *lp_bytes += added;
        }
    }

    fn push_front_owned(&mut self, elem: Vec<u8>) {
        let added = list_lp_entry_bytes(&elem);
        if let Self::Listpack { bytes, entries } = self {
            let elems = entries
                .iter()
                .map(|entry| entry.as_bytes(bytes).to_vec())
                .collect();
            *self = Self::from_vec(elems);
        }
        if let Self::Owned {
            elems,
            lp_bytes,
            front_biased,
        } = self
        {
            if *lp_bytes == 0 {
                *lp_bytes = owned_listpack_bytes(elems);
            }
            let elems = Arc::make_mut(elems);
            if !*front_biased {
                elems.reverse();
                *front_biased = true;
            }
            elems.push(elem);
            *lp_bytes += added;
        }
    }

    fn iter(&self) -> ListChunkIter<'_> {
        match self {
            Self::Owned {
                elems,
                front_biased,
                ..
            } => {
                if *front_biased {
                    ListChunkIter::OwnedRev(elems.iter().rev())
                } else {
                    ListChunkIter::Owned(elems.iter())
                }
            }
            Self::Listpack { bytes, entries } => ListChunkIter::Listpack {
                bytes,
                entries: entries.iter(),
            },
        }
    }

    fn iter_from(&self, start: usize) -> ListChunkIter<'_> {
        match self {
            Self::Owned {
                elems,
                front_biased,
                ..
            } => {
                let start = start.min(elems.len());
                if *front_biased {
                    ListChunkIter::OwnedRev(elems[..elems.len() - start].iter().rev())
                } else {
                    ListChunkIter::Owned(elems[start..].iter())
                }
            }
            Self::Listpack { bytes, entries } => {
                let start = start.min(entries.len());
                ListChunkIter::Listpack {
                    bytes,
                    entries: entries[start..].iter(),
                }
            }
        }
    }

    fn iter_rev(&self) -> ListChunkRevIter<'_> {
        match self {
            Self::Owned {
                elems,
                front_biased,
                ..
            } => {
                if *front_biased {
                    ListChunkRevIter::Owned(elems.iter())
                } else {
                    ListChunkRevIter::OwnedRev(elems.iter().rev())
                }
            }
            Self::Listpack { bytes, entries } => ListChunkRevIter::Listpack {
                bytes,
                entries: entries.iter().rev(),
            },
        }
    }
}

fn owned_listpack_bytes(elems: &[Vec<u8>]) -> u64 {
    LIST_LP_OVERHEAD
        + elems
            .iter()
            .map(|elem| list_lp_entry_bytes(elem))
            .sum::<u64>()
}

fn quicklist_packed_node_accepts_local(
    current_count: usize,
    current_bytes: u64,
    next_value_len: usize,
    fill: i64,
) -> bool {
    const QUICKLIST_SIZE_ESTIMATE_OVERHEAD: u64 = 8;
    let trial_bytes = current_bytes
        .saturating_add(next_value_len as u64)
        .saturating_add(QUICKLIST_SIZE_ESTIMATE_OVERHEAD);
    if fill >= 0 {
        if trial_bytes > LIST_SIZE_SAFETY_LIMIT {
            return false;
        }
        let count_limit = if fill == 0 { 1 } else { fill as usize };
        return current_count < count_limit;
    }
    trial_bytes <= list_neg_fill_size(fill)
}

#[derive(Clone, Debug, Default)]
struct ChunkedList {
    chunks: VecDeque<ListChunk>,
    len: usize,
}

pub(crate) struct RetainedListpackChunk<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) entries: &'a [ListpackValueSpan],
}

pub(crate) struct QuicklistPackedNode<'a> {
    pub(crate) bytes: Cow<'a, [u8]>,
}

impl ChunkedList {
    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, idx: usize) -> Option<&[u8]> {
        let (chunk_idx, local_idx) = self.locate(idx)?;
        self.chunks.get(chunk_idx)?.get(local_idx)
    }

    fn locate(&self, idx: usize) -> Option<(usize, usize)> {
        if idx >= self.len {
            return None;
        }
        // (frankenredis-vizeb) Walk from whichever END is nearer, mirroring
        // redis quicklist's head/tail-relative node walk: front for the first
        // half, back for the second. A front-only scan made deep-tail access
        // (LINDEX/LSET key -1 on a long list) O(num_chunks); choosing the
        // nearer end makes it O(min(idx, len-1-idx) / chunk) — O(1) at either
        // end. Byte-identical: the chunks partition the list in order, so the
        // (chunk_idx, local_idx) returned is exactly the front-walk result.
        if idx < self.len / 2 {
            let mut base = 0usize;
            for (chunk_idx, chunk) in self.chunks.iter().enumerate() {
                let next = base + chunk.len();
                if idx < next {
                    return Some((chunk_idx, idx - base));
                }
                base = next;
            }
            None
        } else {
            // `base` tracks the index of the first element of the current chunk
            // as we sweep chunks from the back.
            let mut base = self.len;
            for (chunk_idx, chunk) in self.chunks.iter().enumerate().rev() {
                base -= chunk.len();
                if idx >= base {
                    return Some((chunk_idx, idx - base));
                }
            }
            None
        }
    }

    fn push_back(&mut self, elem: Vec<u8>) {
        self.push_back_with_fill(elem, -2);
    }

    fn push_back_with_fill(&mut self, elem: Vec<u8>, fill: i64) {
        if let Some(back) = self.chunks.back_mut()
            && back.accepts_append(&elem, fill)
        {
            back.push_back_owned(elem);
            self.len += 1;
            return;
        }
        // (frankenredis-99fwc) The back chunk is complete and about to become
        // interior. Seal it only if it already satisfies the same quicklist node
        // boundary that DUMP/DEBUG serialization will later require.
        if let Some(back) = self.chunks.back_mut() {
            back.seal_if_owned(fill);
        }
        self.chunks
            .push_back(ListChunk::from_vec(Vec::from([elem])));
        self.len += 1;
    }

    fn push_front_with_fill(&mut self, elem: Vec<u8>, fill: i64) {
        if let Some(front) = self.chunks.front_mut()
            && front.accepts_append(&elem, fill)
        {
            front.push_front_owned(elem);
            self.len += 1;
            return;
        }
        if let Some(front) = self.chunks.front_mut() {
            front.seal_if_owned(fill);
        }
        self.chunks
            .push_front(ListChunk::from_front_vec(Vec::from([elem])));
        self.len += 1;
    }

    fn pop_front(&mut self) -> Option<Vec<u8>> {
        let out = self.chunks.front_mut()?.make_mut().remove(0);
        self.len -= 1;
        if self.chunks.front().is_some_and(ListChunk::is_empty) {
            self.chunks.pop_front();
        }
        Some(out)
    }

    fn pop_back(&mut self) -> Option<Vec<u8>> {
        let out = self.chunks.back_mut()?.make_mut().pop()?;
        self.len -= 1;
        if self.chunks.back().is_some_and(ListChunk::is_empty) {
            self.chunks.pop_back();
        }
        Some(out)
    }

    fn set(&mut self, idx: usize, elem: Vec<u8>) -> bool {
        let Some((chunk_idx, local_idx)) = self.locate(idx) else {
            return false;
        };
        let Some(chunk) = self.chunks.get_mut(chunk_idx) else {
            return false;
        };
        chunk.make_mut()[local_idx] = elem;
        true
    }

    fn insert(&mut self, idx: usize, elem: Vec<u8>) {
        if idx >= self.len {
            self.push_back(elem);
            return;
        }
        let Some((chunk_idx, local_idx)) = self.locate(idx) else {
            self.push_back(elem);
            return;
        };
        let chunk = &mut self.chunks[chunk_idx];
        chunk.make_mut().insert(local_idx, elem);
        self.len += 1;
        if chunk.len() > LIST_CHUNK_TARGET {
            let split_at = chunk.len() / 2;
            let right = chunk.make_mut().split_off(split_at);
            self.chunks
                .insert(chunk_idx + 1, ListChunk::from_vec(right));
        }
    }

    fn remove(&mut self, idx: usize) -> Option<Vec<u8>> {
        let (chunk_idx, local_idx) = self.locate(idx)?;
        let out = self.chunks[chunk_idx].make_mut().remove(local_idx);
        self.len -= 1;
        if self.chunks[chunk_idx].is_empty() {
            self.chunks.remove(chunk_idx);
        }
        Some(out)
    }

    fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let mut next = ChunkedList::default();
        for elem in self.iter() {
            if keep(elem) {
                next.push_back(elem.to_vec());
            }
        }
        *self = next;
    }

    fn iter(&self) -> ChunkedListIter<'_> {
        ChunkedListIter {
            chunks: self.chunks.iter(),
            current: None,
        }
    }

    /// Back-to-front iterator. O(n) total (vs O(n*chunks) for repeated `get(i)`
    /// in a reverse scan). (frankenredis-gjyzr)
    fn iter_rev(&self) -> ChunkedListRevIter<'_> {
        ChunkedListRevIter {
            chunks: self.chunks.iter().rev(),
            current: None,
        }
    }

    /// Forward iterator starting at element index `start`, seeking at the CHUNK
    /// level from whichever end is closer — O(min(start, len-start)/chunk + chunk)
    /// instead of the O(start) element-by-element `iter().skip(start)`. Mirrors
    /// redis's quicklistIndex, which walks ~start/128 nodes from the nearest end.
    /// (frankenredis-3r9lz)
    fn iter_from(&self, start: usize) -> ChunkedListIter<'_> {
        if start >= self.len {
            return ChunkedListIter {
                chunks: self.chunks.range(self.chunks.len()..),
                current: None,
            };
        }
        let (chunk_idx, base) = if start * 2 <= self.len {
            let mut base = 0usize;
            let mut idx = 0usize;
            for chunk in self.chunks.iter() {
                let n = chunk.len();
                if start < base + n {
                    break;
                }
                base += n;
                idx += 1;
            }
            (idx, base)
        } else {
            let mut base = self.len;
            let mut idx = self.chunks.len();
            for chunk in self.chunks.iter().rev() {
                base -= chunk.len();
                idx -= 1;
                if start >= base {
                    break;
                }
            }
            (idx, base)
        };
        let local = start - base;
        let mut chunks = self.chunks.range(chunk_idx..);
        let current = chunks.next().map(|c| c.iter_from(local));
        ChunkedListIter { chunks, current }
    }
}

pub(crate) enum RestoredListNode {
    Plain(Vec<u8>),
    Listpack {
        bytes: Vec<u8>,
        entries: Vec<ListpackValueSpan>,
    },
}

fn flush_restore_plain_chunk(out: &mut ChunkedList, chunk: &mut Vec<Vec<u8>>) {
    if chunk.is_empty() {
        return;
    }
    let chunk = std::mem::take(chunk);
    out.len += chunk.len();
    out.chunks.push_back(ListChunk::from_vec(chunk));
}

impl ChunkedList {
    /// Build the chunk list from restored QUICKLIST_2 nodes and accumulate the
    /// growth-state totals in the SAME pass.
    ///
    /// Returns `(list, raw_total, enc_total)` where `raw_total` sums element
    /// lengths and `enc_total` sums `list_lp_entry_bytes` per element — exactly
    /// the fold `ListValue::rebuild_growth_state` performs, so the caller can
    /// skip that second full iteration over every element. Byte-identical: the
    /// same elements are summed, in the same encoding rules, and `+` is
    /// associative. Keeping the per-element `list_lp_entry_bytes` call (rather
    /// than deriving `enc_total` from the listpack header's `total_bytes`) is
    /// load-bearing: a non-canonically-encoded payload must keep yielding the
    /// same `lp_bytes` / `forced_quicklist` — and hence the same
    /// `OBJECT ENCODING` — as the re-walk did. (frankenredis-c92f6)
    fn from_restored_nodes(nodes: Vec<RestoredListNode>) -> (Self, u64, u64) {
        let mut out = ChunkedList::default();
        let mut plain_chunk = Vec::with_capacity(LIST_CHUNK_TARGET);
        let mut raw_total: u64 = 0;
        let mut enc_total: u64 = 0;
        for node in nodes {
            match node {
                RestoredListNode::Plain(elem) => {
                    raw_total += elem.len() as u64;
                    enc_total += list_lp_entry_bytes(&elem);
                    plain_chunk.push(elem);
                    if plain_chunk.len() == LIST_CHUNK_TARGET {
                        flush_restore_plain_chunk(&mut out, &mut plain_chunk);
                        plain_chunk = Vec::with_capacity(LIST_CHUNK_TARGET);
                    }
                }
                RestoredListNode::Listpack { bytes, entries } => {
                    flush_restore_plain_chunk(&mut out, &mut plain_chunk);
                    // The decoded spans are still hot here; summing now avoids a
                    // second traversal of `bytes` through the chunk iterator.
                    for span in &entries {
                        let elem = span.as_bytes(&bytes);
                        raw_total += elem.len() as u64;
                        enc_total += list_lp_entry_bytes(elem);
                    }
                    out.len += entries.len();
                    out.chunks
                        .push_back(ListChunk::from_listpack(bytes, entries));
                }
            }
        }
        flush_restore_plain_chunk(&mut out, &mut plain_chunk);
        (out, raw_total, enc_total)
    }
}

impl From<VecDeque<Vec<u8>>> for ChunkedList {
    fn from(d: VecDeque<Vec<u8>>) -> Self {
        let mut out = ChunkedList::default();
        let mut chunk = Vec::with_capacity(LIST_CHUNK_TARGET);
        for elem in d {
            chunk.push(elem);
            if chunk.len() == LIST_CHUNK_TARGET {
                out.len += chunk.len();
                out.chunks.push_back(ListChunk::from_vec(chunk));
                chunk = Vec::with_capacity(LIST_CHUNK_TARGET);
            }
        }
        if !chunk.is_empty() {
            out.len += chunk.len();
            out.chunks.push_back(ListChunk::from_vec(chunk));
        }
        out
    }
}

pub struct ChunkedListIter<'a> {
    chunks: std::collections::vec_deque::Iter<'a, ListChunk>,
    current: Option<ListChunkIter<'a>>,
}

impl<'a> Iterator for ChunkedListIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(current) = &mut self.current
                && let Some(elem) = current.next()
            {
                return Some(elem);
            }
            let chunk = self.chunks.next()?;
            self.current = Some(chunk.iter());
        }
    }
}

enum ListChunkIter<'a> {
    Owned(std::slice::Iter<'a, Vec<u8>>),
    OwnedRev(std::iter::Rev<std::slice::Iter<'a, Vec<u8>>>),
    Listpack {
        bytes: &'a [u8],
        entries: std::slice::Iter<'a, ListpackValueSpan>,
    },
}

impl<'a> Iterator for ListChunkIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Owned(iter) => iter.next().map(Vec::as_slice),
            Self::OwnedRev(iter) => iter.next().map(Vec::as_slice),
            Self::Listpack { bytes, entries } => entries.next().map(|entry| entry.as_bytes(bytes)),
        }
    }
}

/// Back-to-front borrowing iterator over a `ChunkedList` — chunks in reverse,
/// elements within each chunk in reverse. (frankenredis-gjyzr)
pub struct ChunkedListRevIter<'a> {
    chunks: std::iter::Rev<std::collections::vec_deque::Iter<'a, ListChunk>>,
    current: Option<ListChunkRevIter<'a>>,
}

impl<'a> Iterator for ChunkedListRevIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(current) = &mut self.current
                && let Some(elem) = current.next()
            {
                return Some(elem);
            }
            let chunk = self.chunks.next()?;
            self.current = Some(chunk.iter_rev());
        }
    }
}

enum ListChunkRevIter<'a> {
    Owned(std::slice::Iter<'a, Vec<u8>>),
    OwnedRev(std::iter::Rev<std::slice::Iter<'a, Vec<u8>>>),
    Listpack {
        bytes: &'a [u8],
        entries: std::iter::Rev<std::slice::Iter<'a, ListpackValueSpan>>,
    },
}

impl<'a> Iterator for ListChunkRevIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Owned(iter) => iter.next().map(Vec::as_slice),
            Self::OwnedRev(iter) => iter.next().map(Vec::as_slice),
            Self::Listpack { bytes, entries } => entries.next().map(|entry| entry.as_bytes(bytes)),
        }
    }
}

// ── OBJECT ENCODING listpack/quicklist tracking (frankenredis-rc49s) ──
//
// Redis decides the listpack→quicklist transition at ADD time, not at query
// time: `listTypeTryConvertListpack` (t_list.c) converts when
// `quicklistNodeExceedsLimit(fill, lpBytes(existing) + sum(sdslen(added)),
// count)` — the newly-pushed elements are counted by their RAW byte length,
// while the existing listpack contributes its real encoded `lpBytes`. The
// result is therefore construction-order dependent and sticky, and CANNOT be
// reproduced by a stateless re-encode of the final contents (fr's old
// `list_fits_legacy_listpack_size` over-counted the last element by
// `encoded_len - raw_len`, flipping ~±1 element early/late at the 8 KiB
// boundary). We mirror the real semantics by tracking, incrementally, the
// exact `lpBytes` of the list and the sticky decision under the DEFAULT byte
// budget (`list-max-listpack-size = -2` ⇒ 8192; the only value for which
// `forced_quicklist` is consulted — other budgets fall back to the stateless
// estimate in `Store::object_encoding`).
const LIST_LP_OVERHEAD: u64 = 7; // 4-byte total-bytes + 2-byte count header + 0xFF EOF
const LIST_DEFAULT_BUDGET: u64 = 8192; // quicklistNodeLimit(-2) sz_limit
/// quicklist.c `SIZE_SAFETY_LIMIT` — a packed node is never allowed to exceed
/// this even when `list-max-listpack-size` is a positive (count) limit.
const LIST_SIZE_SAFETY_LIMIT: u64 = 8192;

/// quicklist.c `quicklistNodeLimit` size budget for a negative `fill`
/// (`optimization_level[] = {4096, 8192, 16384, 32768, 65536}`, clamped).
const fn list_neg_fill_size(fill: i64) -> u64 {
    const LV: [u64; 5] = [4096, 8192, 16384, 32768, 65536];
    let mut off = ((-fill) as usize).saturating_sub(1);
    if off >= LV.len() {
        off = LV.len() - 1;
    }
    LV[off]
}

/// quicklist.c `quicklistNodeExceedsLimit(fill, new_sz, new_count)` — the exact
/// redis predicate for whether a single packed (listpack) node has outgrown the
/// `list-max-listpack-size` budget. Negative fill ⇒ size budget; non-negative
/// fill ⇒ count budget, but a packed node still may not exceed
/// `SIZE_SAFETY_LIMIT`.
const fn list_node_exceeds_limit(fill: i64, new_sz: u64, new_count: u64) -> bool {
    if fill < 0 {
        new_sz > list_neg_fill_size(fill)
    } else if new_sz > LIST_SIZE_SAFETY_LIMIT {
        true
    } else {
        new_count > fill as u64
    }
}

/// Decimal integer that round-trips to its canonical form — mirrors
/// `parse_listpack_integer` in `lib.rs` so listpack int-encoding decisions
/// (and thus byte sizing) match the byte-exact encoder.
fn list_lp_int(entry: &[u8]) -> Option<i64> {
    if entry.is_empty() || entry.len() >= 21 {
        return None;
    }
    if !list_lp_int_bytes_are_canonical(entry) {
        None
    } else {
        std::str::from_utf8(entry).ok()?.parse::<i64>().ok()
    }
}

/// True iff `entry` is the canonical base-10 text of an integer: optional '-',
/// no '+', no redundant leading zero, and not "-0". Range is still enforced by
/// the parse in `list_lp_int`.
fn list_lp_int_bytes_are_canonical(entry: &[u8]) -> bool {
    let digits = match entry.first() {
        Some(b'-') => &entry[1..],
        Some(_) => entry,
        None => return false,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    if digits[0] == b'0' && digits.len() > 1 {
        return false;
    }
    if entry[0] == b'-' && digits == b"0" {
        return false;
    }
    true
}

/// Number of bytes `encode_listpack_backlen` emits for a `data_len`.
fn list_lp_backlen_bytes(data_len: u64) -> u64 {
    if data_len <= 127 {
        1
    } else if data_len < 16_383 {
        2
    } else if data_len < 2_097_151 {
        3
    } else if data_len < 268_435_455 {
        4
    } else {
        5
    }
}

/// Exact number of listpack bytes one element occupies (encoding header/int
/// width + payload + backlen) — mirrors `encode_listpack_entry` /
/// `encode_listpack_integer_entry` in `lib.rs`.
fn list_lp_entry_bytes(elem: &[u8]) -> u64 {
    let data_len: u64 = if let Some(v) = list_lp_int(elem) {
        if (0..=127).contains(&v) {
            1
        } else if (-4096..=4095).contains(&v) {
            2
        } else if i16::try_from(v).is_ok() {
            3
        } else if (-8_388_608..=8_388_607).contains(&v) {
            4
        } else if i32::try_from(v).is_ok() {
            5
        } else {
            9
        }
    } else {
        let header = if elem.len() < 64 {
            1
        } else if elem.len() < 4096 {
            2
        } else {
            5
        };
        header + elem.len() as u64
    };
    data_len + list_lp_backlen_bytes(data_len)
}

/// A list value plus the incrementally-maintained state backing its OBJECT
/// ENCODING report. The public method surface (push/pop/insert/set/remove/
/// retain/iter/...) is unchanged, so callers are unaffected. (frankenredis-rc49s)
#[derive(Clone, Debug)]
pub struct ListValue {
    repr: ListRepr,
    /// Exact `lpBytes` of this list encoded as a single listpack.
    lp_bytes: u64,
    /// Sticky listpack→quicklist decision. Set by the ADD-time / LSET-time
    /// conversion check (`note_command_grow` / `note_lset_grow`) under whatever
    /// `list-max-listpack-size` was active then; cleared by the AUTO shrink
    /// hysteresis. Consulted directly for the default (-2) budget and, via
    /// `forced_for_fill`, for non-default budgets.
    forced_quicklist: bool,
    /// `list-max-listpack-size` under which `forced_quicklist` was last
    /// evaluated. The non-(-2) encoding report trusts the sticky flag only when
    /// this matches the current config (so construction/load defaults baked
    /// under -2 cannot pollute a non-default report); the next mutation under
    /// the current config re-evaluates it. (frankenredis-lsetql)
    fill: i64,
    /// True once a grow-WRITE (`note_command_grow` / `note_lset_grow`) has
    /// evaluated `forced_quicklist` under a real `list-max-listpack-size`.
    /// Upstream decides listpack↔quicklist only at write time and the result is
    /// sticky, so for a write-decided list OBJECT ENCODING must trust the tracked
    /// flag REGARDLESS of a later bare `CONFIG SET list-max-listpack-size` (a
    /// threshold change with no intervening write must not flip the reported
    /// encoding). Bulk-built lists (load / RESTORE / COPY, via `From`/
    /// `FromIterator`) have no write-time decision under a non-default fill, so
    /// they keep `false` and the non-default report re-derives from current
    /// content. (frankenredis-a0p5p)
    decided_by_write: bool,
}

impl Default for ListValue {
    fn default() -> Self {
        ListValue {
            repr: ListRepr::default(),
            lp_bytes: LIST_LP_OVERHEAD,
            forced_quicklist: false,
            fill: -2,
            decided_by_write: false,
        }
    }
}

impl ListValue {
    /// Add `elem`'s encoded size to the running `lpBytes`. The sticky
    /// listpack→quicklist decision is NOT made here — redis decides once per
    /// command over the batch's RAW total via `note_command_grow`, so that
    /// multi-element commands (`RPUSH k a b c …`) are not over-counted by the
    /// per-element encoded inflation of earlier batch members.
    fn add_entry_bytes(&mut self, elem: &[u8]) {
        self.lp_bytes += list_lp_entry_bytes(elem);
    }

    /// Empty-listpack `lpBytes` (header + EOF) — the `lpBytes(existing)` term a
    /// command on a fresh key starts from.
    #[must_use]
    pub const fn empty_listpack_bytes() -> u64 {
        LIST_LP_OVERHEAD
    }

    /// Apply redis's ADD-time listpack→quicklist conversion for ONE command:
    /// `listTypeTryConvertListpack` converts (stickily) when
    /// `lpBytes(list before the command) + Σ sdslen(added) > sz_limit` under the
    /// default `-2` budget. `lp_before_command` is the list's `lpBytes`
    /// snapshotted BEFORE the command's pushes; `raw_add` is the sum of the RAW
    /// byte lengths of the newly-added elements. (frankenredis-rc49s)
    pub fn note_command_grow(&mut self, lp_before_command: u64, raw_add: u64, fill: i64) {
        self.fill = fill;
        self.decided_by_write = true;
        // After an ADD command the post-mutation length equals redis's
        // `lpLength(before) + add_length`, so `self.len()` is the count redis
        // feeds `quicklistNodeExceedsLimit`.
        if !self.forced_quicklist
            && list_node_exceeds_limit(fill, lp_before_command + raw_add, self.len() as u64)
        {
            self.forced_quicklist = true;
        }
    }

    /// Apply redis's LSET-time conversion. `lsetCommand` runs
    /// `listTypeTryConversionAppend(o, value)` — `LIST_CONV_GROWING` over the
    /// CURRENT full listpack plus the new value's raw length, with
    /// `count = lpLength + 1` — BEFORE the index range check, so even an
    /// out-of-range LSET can stickily convert a full listpack to quicklist.
    /// (frankenredis-lsetql)
    pub fn note_lset_grow(&mut self, value_raw_len: u64, fill: i64) {
        self.fill = fill;
        self.decided_by_write = true;
        if !self.forced_quicklist
            && list_node_exceeds_limit(fill, self.lp_bytes + value_raw_len, self.len() as u64 + 1)
        {
            self.forced_quicklist = true;
        }
    }

    /// OBJECT ENCODING hint for a NON-default budget: `true` when the sticky
    /// listpack→quicklist decision was made under the current `fill`. A flag
    /// baked under a different budget (e.g. the -2 default a freshly-loaded list
    /// starts with) is NOT trusted here — the caller falls back to the stateless
    /// current-content check. (frankenredis-lsetql)
    #[must_use]
    pub fn forced_for_fill(&self, fill: i64) -> bool {
        self.forced_quicklist && self.fill == fill
    }

    /// Apply redis's AUTO shrink hysteresis: convert quicklist→listpack only
    /// once well below the limit, avoiding flapping. (t_list.c
    /// listTypeTryConvertQuicklist, LIST_CONV_AUTO)
    fn shrink_hysteresis(&mut self) {
        if self.is_empty() {
            self.lp_bytes = LIST_LP_OVERHEAD;
        }
        if !self.forced_quicklist {
            return;
        }
        // redis `listTypeTryConvertQuicklist` (LIST_CONV_SHRINKING): a quicklist
        // collapses back to a single listpack node only when that node both fits
        // the limit AND has fallen to at most HALF of it (hysteresis, so it does
        // not flap around the boundary). For the default -2 budget this reduces
        // to `lp_bytes <= 4096`, matching the prior `LIST_DEFAULT_REVERT` gate.
        let fill = self.fill;
        let count = self.len() as u64;
        if list_node_exceeds_limit(fill, self.lp_bytes, count) {
            return;
        }
        let below_half = if fill < 0 {
            self.lp_bytes <= list_neg_fill_size(fill) / 2
        } else {
            count <= (fill as u64) / 2
        };
        if below_half {
            self.forced_quicklist = false;
        }
    }

    /// Account for a single element (with the given encoded size) leaving the
    /// listpack, in O(1).
    fn on_remove_one(&mut self, removed: &[u8]) {
        self.lp_bytes = self
            .lp_bytes
            .saturating_sub(list_lp_entry_bytes(removed))
            .max(LIST_LP_OVERHEAD);
        self.shrink_hysteresis();
    }

    /// Account for an arbitrary bulk removal (LREM/LTRIM) by recomputing
    /// `lp_bytes` from the survivors, then applying hysteresis.
    fn on_remove_bulk(&mut self) {
        self.lp_bytes = LIST_LP_OVERHEAD + self.iter().map(list_lp_entry_bytes).sum::<u64>();
        self.shrink_hysteresis();
    }

    /// Re-derive `lp_bytes` and `forced_quicklist` for a freshly-built list
    /// (load / RESTORE / internal bulk-build). The construction history is not
    /// available, so we treat the whole contents as a single bulk insertion:
    /// `forced` iff the total raw bytes would have exceeded the budget in one
    /// shot — the same test redis's bulk listpack→quicklist conversion applies.
    fn rebuild_growth_state(&mut self) {
        let (raw_total, enc_total): (u64, u64) = self.iter().fold((0, 0), |(r, e), elem| {
            (r + elem.len() as u64, e + list_lp_entry_bytes(elem))
        });
        self.lp_bytes = LIST_LP_OVERHEAD + enc_total;
        self.forced_quicklist = LIST_LP_OVERHEAD + raw_total > LIST_DEFAULT_BUDGET;
    }

    /// OBJECT ENCODING hint under the default byte budget: `true` when redis
    /// would report `quicklist`. Consulted only when `list_max_listpack_size`
    /// is the default `-2`. (frankenredis-rc49s)
    #[must_use]
    pub fn reports_quicklist_default(&self) -> bool {
        self.forced_quicklist
    }

    /// True when a grow-write has evaluated the sticky listpack→quicklist
    /// decision under a real `list-max-listpack-size` (vs a bulk-built list whose
    /// flag is only the stateless construction-time estimate). (frankenredis-a0p5p)
    #[must_use]
    pub fn encoding_decided_by_write(&self) -> bool {
        self.decided_by_write
    }

    /// The raw sticky listpack→quicklist flag (quicklist iff true), to be trusted
    /// only when `encoding_decided_by_write()`. (frankenredis-a0p5p)
    #[must_use]
    pub fn is_forced_quicklist(&self) -> bool {
        self.forced_quicklist
    }

    /// Exact `lpBytes` of this list as a single listpack (for tests/debug).
    #[must_use]
    pub fn listpack_byte_len(&self) -> u64 {
        self.lp_bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match &self.repr {
            ListRepr::Packed(p) => p.len(),
            ListRepr::Deque(d) => d.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&[u8]> {
        match &self.repr {
            ListRepr::Packed(p) => p.get(idx),
            ListRepr::Deque(d) => d.get(idx),
        }
    }

    fn promote(&mut self) {
        if let ListRepr::Packed(p) = &self.repr {
            let mut d: VecDeque<Vec<u8>> = VecDeque::with_capacity(p.len() + 1);
            for e in p.iter() {
                d.push_back(e.to_vec());
            }
            self.repr = ListRepr::Deque(Arc::new(ChunkedList::from(d)));
        }
    }

    fn maybe_promote(&mut self, added_len: usize) {
        if let ListRepr::Packed(p) = &self.repr
            && (p.len() >= PACKED_MAX_ENTRIES || added_len > PACKED_MAX_VALUE)
        {
            self.promote();
        }
    }

    pub fn push_back(&mut self, elem: Vec<u8>) {
        self.add_entry_bytes(&elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.push_back(&elem),
            ListRepr::Deque(d) => Arc::make_mut(d).push_back_with_fill(elem, self.fill),
        }
    }

    pub fn push_front(&mut self, elem: Vec<u8>) {
        self.add_entry_bytes(&elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.push_front(&elem),
            ListRepr::Deque(d) => Arc::make_mut(d).push_front_with_fill(elem, self.fill),
        }
    }

    /// (cc_fr) Borrowed siblings of [`Self::push_back`]/[`Self::push_front`]. The Packed repr
    /// (the common small-list case) copies STRAIGHT from the slice into its packed buffer, so
    /// LPUSH/RPUSH need not materialize an owned `Vec` per element — the old `push_*(bytes.to_vec())`
    /// alloc'd a temp Vec that was copied into the buffer then dropped. Only the Deque repr (large
    /// lists) needs an owned element (uncommon), where this `to_vec()`s exactly as before. Same
    /// `add_entry_bytes`/`maybe_promote`/repr dispatch ⇒ byte-identical to `push_*(elem.to_vec())`.
    pub fn push_back_borrowed(&mut self, elem: &[u8]) {
        self.add_entry_bytes(elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.push_back(elem),
            ListRepr::Deque(d) => Arc::make_mut(d).push_back_with_fill(elem.to_vec(), self.fill),
        }
    }

    pub fn push_front_borrowed(&mut self, elem: &[u8]) {
        self.add_entry_bytes(elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.push_front(elem),
            ListRepr::Deque(d) => Arc::make_mut(d).push_front_with_fill(elem.to_vec(), self.fill),
        }
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.pop_front(),
            ListRepr::Deque(d) => Arc::make_mut(d).pop_front(),
        };
        if let Some(ref r) = removed {
            self.on_remove_one(r);
        }
        removed
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.pop_back(),
            ListRepr::Deque(d) => Arc::make_mut(d).pop_back(),
        };
        if let Some(ref r) = removed {
            self.on_remove_one(r);
        }
        removed
    }

    /// (cc_fr) Batch [`Self::pop_front`] of up to `count` elements (LPOP count), returned in pop
    /// order (front first). On the Packed repr this is ONE `drain_front_n` shift instead of `count`
    /// per-element `buf.drain`s (`pop_front` × count is O(count·n) — quadratic); the Deque repr,
    /// already O(1)/element, keeps the exact per-`pop_front` sequence. Byte-identical observable
    /// state (contents, len, lp_bytes, encoding) to calling `pop_front` `count` times: on Packed,
    /// `shrink_hysteresis` is a no-op (a listpack is never `forced_quicklist`) apart from the
    /// empty→`lp_bytes = OVERHEAD` reset, which the per-element `on_remove_one` calls reproduce.
    pub fn pop_front_n(&mut self, count: usize) -> Vec<Vec<u8>> {
        let n = count.min(self.len());
        // Deque is already O(1)/element — preserve the exact pop_front sequence (incl per-pop
        // hysteresis / quicklist->listpack revert), no quadratic shift to eliminate.
        if matches!(self.repr, ListRepr::Deque(_)) {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                match self.pop_front() {
                    Some(v) => out.push(v),
                    None => break,
                }
            }
            return out;
        }
        let out = {
            let ListRepr::Packed(p) = &mut self.repr else {
                unreachable!("repr checked to be Packed")
            };
            p.drain_front_n(n)
        };
        for r in &out {
            self.on_remove_one(r);
        }
        out
    }

    /// (cc_fr) Batch [`Self::pop_back`] of up to `count` elements (RPOP count), returned in pop
    /// order (last element first). On the Packed repr this is ONE `drain_back_n` (scan + truncate)
    /// instead of `count` `pop_back`s that each re-scan from the front (`bounds(len-1)`), i.e.
    /// O(count·len). The Deque repr, already O(1)/element, keeps the exact per-`pop_back` sequence.
    /// Byte-identical observable state to calling `pop_back` `count` times (see `pop_front_n`).
    pub fn pop_back_n(&mut self, count: usize) -> Vec<Vec<u8>> {
        let n = count.min(self.len());
        if matches!(self.repr, ListRepr::Deque(_)) {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                match self.pop_back() {
                    Some(v) => out.push(v),
                    None => break,
                }
            }
            return out;
        }
        let out = {
            let ListRepr::Packed(p) = &mut self.repr else {
                unreachable!("repr checked to be Packed")
            };
            p.drain_back_n(n)
        };
        for r in &out {
            self.on_remove_one(r);
        }
        out
    }

    /// Replace the element at `idx` (LSET); false if out of range. This only
    /// updates the byte accounting and the contents; the listpack→quicklist
    /// conversion is the caller's responsibility via `note_lset_grow`, which
    /// upstream runs BEFORE the index range check. (frankenredis-rc49s/lsetql)
    pub fn set(&mut self, idx: usize, elem: Vec<u8>) -> bool {
        let old_entry_bytes = self.get(idx).map(list_lp_entry_bytes);
        let Some(old_entry_bytes) = old_entry_bytes else {
            return false;
        };
        let base = self.lp_bytes - old_entry_bytes;
        self.lp_bytes = base + list_lp_entry_bytes(&elem);
        match &mut self.repr {
            ListRepr::Packed(p) => p.set(idx, &elem),
            ListRepr::Deque(d) => Arc::make_mut(d).set(idx, elem),
        }
    }

    /// Insert before index `idx` (`idx >= len` appends), matching `VecDeque::insert`.
    /// The caller (LINSERT) makes the conversion decision via `note_command_grow`.
    pub fn insert(&mut self, idx: usize, elem: Vec<u8>) {
        self.add_entry_bytes(&elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.insert(idx, &elem),
            ListRepr::Deque(d) => Arc::make_mut(d).insert(idx, elem),
        }
    }

    pub fn remove(&mut self, idx: usize) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.remove(idx),
            ListRepr::Deque(d) => Arc::make_mut(d).remove(idx),
        };
        if let Some(ref r) = removed {
            self.on_remove_one(r);
        }
        removed
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let before = self.len();
        match &mut self.repr {
            ListRepr::Packed(p) => p.retain(&mut keep),
            ListRepr::Deque(d) => Arc::make_mut(d).retain(&mut keep),
        }
        if self.len() != before {
            self.on_remove_bulk();
        }
    }

    pub fn clear(&mut self) {
        *self = ListValue::default();
    }

    pub(crate) fn from_restored_quicklist2_nodes(nodes: Vec<RestoredListNode>) -> Self {
        // (frankenredis-10ovx) A multi-node QUICKLIST_2 payload WAS a quicklist:
        // redis only emits >1 node once a list crossed list-max-listpack-size, and
        // RESTORE/RDB-load/replica-sync preserve that encoding (they build what the
        // RDB says; they do NOT merge nodes back into a single listpack on load).
        // fr previously re-derived encoding from total content via
        // rebuild_growth_state, downgrading a crossed-then-shrunk quicklist (e.g.
        // 130→pop→127 @ cap=128, 2 nodes) to listpack — diverging from redis's
        // preserved `quicklist`. Preserve quicklist for multi-node payloads; a
        // single-node payload still re-derives (listpack iff it fits the configured
        // list-max-listpack-size, evaluated later in Store::object_encoding).
        let multi_node = nodes.len() > 1;
        // `from_restored_nodes` folds the growth-state totals during construction,
        // so we set them directly instead of paying `rebuild_growth_state`'s second
        // full walk over every restored element. The assignments below are exactly
        // what that fold would have written. (frankenredis-c92f6)
        let (chunks, raw_total, enc_total) = ChunkedList::from_restored_nodes(nodes);
        let mut list = ListValue {
            repr: ListRepr::Deque(Arc::new(chunks)),
            lp_bytes: LIST_LP_OVERHEAD + enc_total,
            forced_quicklist: LIST_LP_OVERHEAD + raw_total > LIST_DEFAULT_BUDGET,
            fill: -2,
            decided_by_write: false,
        };
        if multi_node {
            list.forced_quicklist = true;
            list.decided_by_write = true;
        }
        list
    }

    pub(crate) fn retained_listpack_chunks(&self) -> Option<Vec<RetainedListpackChunk<'_>>> {
        let ListRepr::Deque(list) = &self.repr else {
            return None;
        };
        let mut chunks = Vec::with_capacity(list.chunks.len());
        for chunk in &list.chunks {
            match chunk {
                ListChunk::Listpack { bytes, entries } if !entries.is_empty() => {
                    chunks.push(RetainedListpackChunk {
                        bytes: bytes.as_slice(),
                        entries: entries.as_slice(),
                    });
                }
                _ => return None,
            }
        }
        (!chunks.is_empty()).then_some(chunks)
    }

    pub(crate) fn quicklist_packed_nodes(&self, fill: i64) -> Option<Vec<QuicklistPackedNode<'_>>> {
        let ListRepr::Deque(list) = &self.repr else {
            return None;
        };
        let mut nodes = Vec::with_capacity(list.chunks.len());
        let mut previous: Option<(usize, u64)> = None;
        for chunk in &list.chunks {
            let (bytes, entries_len, first_len) = match chunk {
                ListChunk::Listpack { bytes, entries } if !entries.is_empty() => {
                    let first_len = entries.first()?.as_bytes(bytes).len();
                    (Cow::Borrowed(bytes.as_slice()), entries.len(), first_len)
                }
                ListChunk::Owned {
                    elems,
                    front_biased,
                    ..
                } if !elems.is_empty() => {
                    let slices: Vec<&[u8]> = if *front_biased {
                        elems.iter().rev().map(Vec::as_slice).collect()
                    } else {
                        elems.iter().map(Vec::as_slice).collect()
                    };
                    let blob = fr_persist::encode_listpack_strings_blob(&slices)?;
                    let first_len = if *front_biased {
                        elems.last()?.len()
                    } else {
                        elems.first()?.len()
                    };
                    (Cow::Owned(blob), elems.len(), first_len)
                }
                _ => return None,
            };

            let bytes_len = bytes.len() as u64;
            if list_node_exceeds_limit(fill, bytes_len, entries_len as u64) {
                return None;
            }
            if let Some((previous_count, previous_bytes)) = previous
                && quicklist_packed_node_accepts_local(
                    previous_count,
                    previous_bytes,
                    first_len,
                    fill,
                )
            {
                return None;
            }
            previous = Some((entries_len, bytes_len));
            nodes.push(QuicklistPackedNode { bytes });
        }
        (!nodes.is_empty()).then_some(nodes)
    }

    #[must_use]
    pub fn quicklist_packed_node_blobs(&self, fill: i64) -> Option<Vec<Vec<u8>>> {
        self.quicklist_packed_nodes(fill).map(|nodes| {
            nodes
                .into_iter()
                .map(|node| node.bytes.into_owned())
                .collect()
        })
    }

    #[must_use]
    pub fn iter(&self) -> ListValueIter<'_> {
        match &self.repr {
            ListRepr::Packed(p) => ListValueIter::Packed(p.iter()),
            ListRepr::Deque(d) => ListValueIter::Deque(d.iter()),
        }
    }

    /// Forward iterator starting at element index `start`, seeking at the chunk
    /// level for the large (quicklist) encoding so LRANGE with a deep start is
    /// O(start/chunk + count) not O(start). (frankenredis-3r9lz)
    pub fn iter_from(&self, start: usize) -> ListValueIter<'_> {
        match &self.repr {
            ListRepr::Packed(p) => ListValueIter::Packed(p.iter_from(start)),
            ListRepr::Deque(d) => ListValueIter::Deque(d.iter_from(start)),
        }
    }

    /// Back-to-front iterator. For the large (quicklist) encoding this is O(n)
    /// via the chunk reverse-iterator; a reverse scan with repeated `get(i)`
    /// would be O(n*chunks). The packed encoding is bounded small, so collecting
    /// its borrowed refs to reverse them is trivial. (frankenredis-gjyzr)
    pub fn iter_rev(&self) -> ListValueRevIter<'_> {
        match &self.repr {
            ListRepr::Packed(p) => {
                ListValueRevIter::Packed(p.iter().collect::<Vec<&[u8]>>().into_iter().rev())
            }
            ListRepr::Deque(d) => ListValueRevIter::Deque(d.iter_rev()),
        }
    }
}

/// Borrowing reverse iterator over list elements, back to front.
pub enum ListValueRevIter<'a> {
    Packed(std::iter::Rev<std::vec::IntoIter<&'a [u8]>>),
    Deque(ChunkedListRevIter<'a>),
}

impl<'a> Iterator for ListValueRevIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            ListValueRevIter::Packed(it) => it.next(),
            ListValueRevIter::Deque(it) => it.next(),
        }
    }
}

impl From<VecDeque<Vec<u8>>> for ListValue {
    fn from(d: VecDeque<Vec<u8>>) -> Self {
        let repr = if d.len() > PACKED_MAX_ENTRIES || d.iter().any(|e| e.len() > PACKED_MAX_VALUE) {
            ListRepr::Deque(Arc::new(ChunkedList::from(d)))
        } else {
            let mut p = PackedList::new();
            for e in &d {
                p.push_back(e);
            }
            ListRepr::Packed(p)
        };
        let mut list = ListValue {
            repr,
            lp_bytes: LIST_LP_OVERHEAD,
            forced_quicklist: false,
            fill: -2,
            decided_by_write: false,
        };
        list.rebuild_growth_state();
        list
    }
}

impl FromIterator<Vec<u8>> for ListValue {
    fn from_iter<I: IntoIterator<Item = Vec<u8>>>(iter: I) -> Self {
        let mut l = ListValue::default();
        for e in iter {
            l.push_back(e);
        }
        l
    }
}

/// Set-style equality is order-sensitive for lists (matches `VecDeque` eq).
impl PartialEq for ListValue {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}
impl Eq for ListValue {}

/// Borrowing iterator over list elements, front to back.
pub enum ListValueIter<'a> {
    Packed(PackedListIter<'a>),
    Deque(ChunkedListIter<'a>),
}

impl<'a> Iterator for ListValueIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            ListValueIter::Packed(it) => it.next(),
            ListValueIter::Deque(it) => it.next(),
        }
    }
}

// ───────────────────────── packed sorted set (for small zsets) ──────────────

/// Redis treats `+0.0` and `-0.0` as the same score (zslParseRange / score
/// comparisons). Mirror `Store::canonicalize_zero_score`.
fn canon_zero(score: f64) -> f64 {
    if score == 0.0 { 0.0 } else { score }
}

/// Total order on `(score, member)` matching `ScoreMember`'s `Ord`: by
/// canonical score (`total_cmp`), then member bytes ascending. A `PackedZSet`
/// kept in this order iterates identically to the `SortedSet.ordered` BTreeMap,
/// so ZRANGE/ZRANK output is byte-for-byte unchanged.
fn zset_cmp(score_a: f64, member_a: &[u8], score_b: f64, member_b: &[u8]) -> std::cmp::Ordering {
    canon_zero(score_a)
        .total_cmp(&canon_zero(score_b))
        .then_with(|| member_a.cmp(member_b))
}

/// Packed sorted set for SMALL zsets: a sequence of `[vint mlen][member][f64
/// score, 8 LE bytes]` records kept in `(score, member)` sorted order, one
/// allocation instead of a `BTreeMap` + member `HashMap` (+ lazy rank treap).
/// All zset reads (ZRANGE/ZRANK/ZSCORE/ZRANGEBYSCORE) become an O(n) walk of a
/// cache-resident buffer — the right trade below the zset-max-listpack threshold
/// and matching redis's listpack zset node. (frankenredis-9mh3o step 5)
///
/// Packed sorted-set storage for SMALL zsets: `(member, score)` records sorted
/// by Redis zset order in one contiguous buffer, promoting to the full
/// hash-map/tree representation when thresholds are crossed.
#[derive(Clone, Debug, Default)]
pub struct PackedZSet {
    buf: Vec<u8>,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedZSetInsertResult {
    Added,
    Updated,
    Unchanged,
}

#[allow(dead_code)]
impl PackedZSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }

    /// Build a packed zset from already de-duplicated `(member, score)` pairs.
    /// The output buffer is encoded once in final sorted order; this is the
    /// bulk-construction path for a missing-key `ZADD`.
    #[must_use]
    pub fn from_unique_pairs(mut pairs: Vec<(Vec<u8>, f64)>) -> Self {
        pairs.sort_by(|(am, ascore), (bm, bscore)| zset_cmp(*ascore, am, *bscore, bm));
        let cap = pairs
            .iter()
            .map(|(member, _)| member.len().saturating_add(10))
            .sum();
        let mut zset = Self {
            buf: Vec::with_capacity(cap),
            len: 0,
        };
        for (member, score) in pairs {
            write_varint(&mut zset.buf, member.len());
            zset.buf.extend_from_slice(&member);
            zset.buf.extend_from_slice(&canon_zero(score).to_le_bytes());
            zset.len += 1;
        }
        zset
    }

    /// Borrowed-input twin of [`Self::from_unique_pairs`] for RESTORE/RDB listpack
    /// decode. The packed representation owns one contiguous buffer either way,
    /// so borrowed inputs let the caller skip transient per-member `Vec<u8>`
    /// allocations and copy each member directly into the final packed buffer.
    #[must_use]
    pub fn from_unique_pairs_borrowed(mut pairs: Vec<(&[u8], f64)>) -> Self {
        pairs.sort_by(|(am, ascore), (bm, bscore)| zset_cmp(*ascore, am, *bscore, bm));
        let cap = pairs
            .iter()
            .map(|(member, _)| member.len().saturating_add(10))
            .sum();
        let mut zset = Self {
            buf: Vec::with_capacity(cap),
            len: 0,
        };
        for (member, score) in pairs {
            write_varint(&mut zset.buf, member.len());
            zset.buf.extend_from_slice(member);
            zset.buf.extend_from_slice(&canon_zero(score).to_le_bytes());
            zset.len += 1;
        }
        zset
    }

    #[must_use]
    pub fn from_single(member: Vec<u8>, score: f64) -> Self {
        let mut zset = Self {
            buf: Vec::with_capacity(member.len().saturating_add(10)),
            len: 1,
        };
        write_varint(&mut zset.buf, member.len());
        zset.buf.extend_from_slice(&member);
        zset.buf.extend_from_slice(&canon_zero(score).to_le_bytes());
        zset
    }

    /// Decode the record starting at `pos`: `(member, score, record_end)`.
    fn record_at(&self, pos: usize) -> (&[u8], f64, usize) {
        let (mlen, m_start) = read_varint(&self.buf, pos);
        let m_end = m_start + mlen;
        let mut score_bytes = [0; 8];
        score_bytes.copy_from_slice(&self.buf[m_end..m_end + 8]);
        let score = f64::from_le_bytes(score_bytes);
        (&self.buf[m_start..m_end], score, m_end + 8)
    }

    /// `(record_start, record_end, score)` for `member`, or None. Decodes the 8-byte score
    /// (a bounds-checked load) ONLY for the matching record; non-matching records are skipped by
    /// arithmetic past the member + fixed 8-byte score. `record_at` (used by the score-consuming
    /// scans: iter / insert_offset / index_slice) always decodes the score, so `locate`'s
    /// member-only search paid N score loads to use ONE — pure waste on the ZADD/ZSCORE/ZREM/ZRANK
    /// hot path. Byte-identical: the returned `(pos, end, score)` for the match is unchanged and
    /// non-matching scores were never read.
    fn locate(&self, member: &[u8]) -> Option<(usize, usize, f64)> {
        let mut pos = 0;
        while pos < self.buf.len() {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            let m_end = m_start + mlen;
            let end = m_end + 8;
            if self.buf[m_start..m_end] == *member {
                let mut score_bytes = [0; 8];
                score_bytes.copy_from_slice(&self.buf[m_end..end]);
                return Some((pos, end, f64::from_le_bytes(score_bytes)));
            }
            pos = end;
        }
        None
    }

    fn encode(member: &[u8], score: f64) -> Vec<u8> {
        let mut out = Vec::with_capacity(member.len() + 10);
        write_varint(&mut out, member.len());
        out.extend_from_slice(member);
        out.extend_from_slice(&score.to_le_bytes());
        out
    }

    /// Byte offset where a `(score, member)` record belongs to keep sort order.
    fn insert_offset(&self, member: &[u8], score: f64) -> usize {
        let mut pos = 0;
        while pos < self.buf.len() {
            let (m, s, end) = self.record_at(pos);
            if zset_cmp(score, member, s, m) == std::cmp::Ordering::Less {
                return pos;
            }
            pos = end;
        }
        self.buf.len()
    }

    #[must_use]
    pub fn get_score(&self, member: &[u8]) -> Option<f64> {
        self.locate(member).map(|(_, _, s)| s)
    }

    #[must_use]
    pub fn contains(&self, member: &[u8]) -> bool {
        self.locate(member).is_some()
    }

    /// ZADD a single member; returns true if it was newly added (false = score
    /// updated). Re-positions the member to keep `(score, member)` order.
    pub fn insert(&mut self, member: &[u8], score: f64) -> bool {
        matches!(
            self.insert_result(member, score),
            PackedZSetInsertResult::Added
        )
    }

    pub fn insert_result(&mut self, member: &[u8], score: f64) -> PackedZSetInsertResult {
        let score = canon_zero(score);
        let result = if let Some((rs, re, old_score)) = self.locate(member) {
            if old_score.total_cmp(&score).is_eq() {
                return PackedZSetInsertResult::Unchanged;
            }
            self.buf.drain(rs..re);
            self.len -= 1;
            PackedZSetInsertResult::Updated
        } else {
            PackedZSetInsertResult::Added
        };
        let off = self.insert_offset(member, score);
        self.buf.splice(off..off, Self::encode(member, score));
        self.len += 1;
        result
    }

    /// ZREM a member; returns true if it was present.
    pub fn remove(&mut self, member: &[u8]) -> bool {
        if let Some((rs, re, _)) = self.locate(member) {
            self.buf.drain(rs..re);
            self.len -= 1;
            true
        } else {
            false
        }
    }

    /// (cc_fr) Remove the `count` members at ascending ranks `[s_idx, s_idx+count)` in ONE drain,
    /// returning the number removed. The zset is stored in `(score, member)` rank order, so a rank
    /// range is a CONTIGUOUS byte span — this is O(len) (scan to the span, one shift) vs count× the
    /// O(len) `remove(member)` the generic path does (O(count·len)). No member decode/alloc (only the
    /// count is needed by ZREMRANGEBY{RANK,SCORE,LEX}). Byte-identical residual to count `remove`s of
    /// the same ascending slice.
    pub fn drain_rank_range(&mut self, s_idx: usize, count: usize) -> usize {
        if s_idx >= self.len || count == 0 {
            return 0;
        }
        let remove = count.min(self.len - s_idx);
        // Record layout (see record_at): varint(mlen) + member + 8-byte score.
        let mut pos = 0;
        for _ in 0..s_idx {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            pos = m_start + mlen + 8;
        }
        let start_off = pos;
        for _ in 0..remove {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            pos = m_start + mlen + 8;
        }
        self.buf.drain(start_off..pos);
        self.len -= remove;
        remove
    }

    /// (cc_fr) ZPOPMIN count: remove and return the `count.min(len)` lowest `(member, score)` in
    /// ascending order (lowest first — matching repeated `pop_min`). The lowest ranks are the front
    /// records, so this collects them then drains the front span in ONE shift — O(len) vs count× the
    /// O(len) `pop_min` (each drains the front). Byte-identical to count `pop_min`s.
    pub fn pop_min_n(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
        let n = count.min(self.len);
        let mut out = Vec::with_capacity(n);
        let mut pos = 0;
        for _ in 0..n {
            let (m, score, end) = self.record_at(pos);
            out.push((m.to_vec(), score));
            pos = end;
        }
        self.buf.drain(0..pos);
        self.len -= n;
        out
    }

    /// (cc_fr) ZPOPMAX count: remove and return the `count.min(len)` highest `(member, score)` in
    /// DESCENDING order (highest first — matching repeated `pop_max`). The highest ranks are the tail
    /// records, so this scans once to the split, collects the tail, `truncate`s, and reverses — O(len)
    /// vs count× the O(len) `pop_max` (each front-scans to the last record). Byte-identical to count
    /// `pop_max`s.
    pub fn pop_max_n(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
        let n = count.min(self.len);
        let keep = self.len - n;
        let mut pos = 0;
        for _ in 0..keep {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            pos = m_start + mlen + 8;
        }
        let start_off = pos;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let (m, score, end) = self.record_at(pos);
            out.push((m.to_vec(), score));
            pos = end;
        }
        self.buf.truncate(start_off);
        self.len -= n;
        out.reverse();
        out
    }

    /// 0-based rank of `member` in ascending `(score, member)` order (ZRANK).
    #[must_use]
    pub fn rank(&self, member: &[u8]) -> Option<usize> {
        self.rank_impl::<true>(member)
    }

    /// Shared candidate/reference body for same-binary proof. Plain rank only needs the member
    /// and record index, so production skips the fixed-width score bytes. `MEMBER_ONLY=false`
    /// retains the exact pre-change scan for the benchmark and differential test.
    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub(crate) fn rank_impl<const MEMBER_ONLY: bool>(&self, member: &[u8]) -> Option<usize> {
        let mut pos = 0;
        let mut idx = 0;
        if !MEMBER_ONLY {
            while pos < self.buf.len() {
                let (decoded_member, _score, end) = self.record_at(pos);
                if decoded_member == member {
                    return Some(idx);
                }
                idx += 1;
                pos = end;
            }
            return None;
        }
        while pos < self.buf.len() {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            let m_end = m_start + mlen;
            let end = m_end + 8;
            if self.buf[m_start..m_end] == *member {
                return Some(idx);
            }
            idx += 1;
            pos = end;
        }
        None
    }

    /// (CrimsonHawk) Rank + score in one scan for `ZRANK ... WITHSCORE` — the score is
    /// decoded only for the matching record, while returning it still avoids a second
    /// `get_score` pass.
    #[must_use]
    pub fn rank_with_score(&self, member: &[u8]) -> Option<(usize, f64)> {
        self.rank_with_score_impl::<true>(member)
    }

    /// Shared candidate/reference body for same-binary proof. Rank needs only each member until a
    /// match, so production skips every nonmatching fixed-width score. `MEMBER_ONLY=false` retains
    /// the exact pre-change `record_at` traversal.
    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub fn rank_with_score_impl<const MEMBER_ONLY: bool>(
        &self,
        member: &[u8],
    ) -> Option<(usize, f64)> {
        let mut pos = 0;
        let mut idx = 0;
        if !MEMBER_ONLY {
            while pos < self.buf.len() {
                let (decoded_member, score, end) = self.record_at(pos);
                if decoded_member == member {
                    return Some((idx, score));
                }
                idx += 1;
                pos = end;
            }
            return None;
        }
        while pos < self.buf.len() {
            let (mlen, m_start) = read_varint(&self.buf, pos);
            let m_end = m_start + mlen;
            let end = m_end + 8;
            if self.buf[m_start..m_end] == *member {
                let mut score_bytes = [0; 8];
                score_bytes.copy_from_slice(&self.buf[m_end..end]);
                return Some((idx, f64::from_le_bytes(score_bytes)));
            }
            idx += 1;
            pos = end;
        }
        None
    }

    /// Iterate `(member, score)` in ascending `(score, member)` order.
    #[must_use]
    pub fn iter(&self) -> PackedZSetIter<'_> {
        PackedZSetIter { zset: self, pos: 0 }
    }

    /// `(member, score)` pairs in DESCENDING order (mirrors SortedSet::iter_desc).
    pub fn iter_desc(&self) -> std::iter::Rev<std::vec::IntoIter<(&[u8], f64)>> {
        self.iter().collect::<Vec<_>>().into_iter().rev()
    }

    /// `count` (member, score) pairs starting at ascending index `start_idx`.
    #[must_use]
    pub fn index_slice_asc(&self, start_idx: usize, count: usize) -> Vec<(Vec<u8>, f64)> {
        self.iter()
            .skip(start_idx)
            .take(count)
            .map(|(m, s)| (m.to_vec(), s))
            .collect()
    }

    /// `count` (member, score) pairs starting at descending index `start_idx`
    /// (0 = highest), in descending order.
    #[must_use]
    pub fn index_slice_desc(&self, start_idx: usize, count: usize) -> Vec<(Vec<u8>, f64)> {
        self.index_slice_desc_impl::<true>(start_idx, count)
    }

    /// Shared candidate/reference body for same-binary proof. Production maps the requested
    /// descending ranks to one ascending packed window and reverses only that result; the
    /// `DIRECT=false` arm retains the exact pre-change full materialization.
    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub fn index_slice_desc_impl<const DIRECT: bool>(
        &self,
        start_idx: usize,
        count: usize,
    ) -> Vec<(Vec<u8>, f64)> {
        if !DIRECT {
            return self
                .iter_desc()
                .skip(start_idx)
                .take(count)
                .map(|(m, s)| (m.to_vec(), s))
                .collect();
        }
        if count == 0 || start_idx >= self.len {
            return Vec::new();
        }
        let take = count.min(self.len - start_idx);
        let asc_start = self.len - start_idx - take;
        let mut out = self.index_slice_asc(asc_start, take);
        out.reverse();
        out
    }

    /// Invoke `f(member, score)` for each member whose canonical score lies in
    /// the INCLUSIVE range `[lo, hi]`, ascending (mirrors
    /// SortedSet::for_each_in_score_range, which ranges
    /// `min_for_score(lo)..=max_for_score(hi)`).
    pub fn for_each_in_score_range(&self, lo: f64, hi: f64, f: impl FnMut(&[u8], f64)) {
        self.for_each_in_score_range_impl::<true>(lo, hi, f);
    }

    /// Shared candidate/reference body for same-binary proof. Packed records are sorted by
    /// canonical `(score, member)`, so production stops at the first score above `hi`; the
    /// `EARLY_BREAK=false` arm retains the exact pre-change full scan.
    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub fn for_each_in_score_range_impl<const EARLY_BREAK: bool>(
        &self,
        lo: f64,
        hi: f64,
        mut f: impl FnMut(&[u8], f64),
    ) {
        let (lo, hi) = (canon_zero(lo), canon_zero(hi));
        if !EARLY_BREAK {
            for (member, score) in self.iter() {
                let c = canon_zero(score);
                if c.total_cmp(&lo) != std::cmp::Ordering::Less
                    && c.total_cmp(&hi) != std::cmp::Ordering::Greater
                {
                    f(member, score);
                }
            }
            return;
        }
        for (member, score) in self.iter() {
            let c = canon_zero(score);
            if c.total_cmp(&hi) == std::cmp::Ordering::Greater {
                break;
            }
            if c.total_cmp(&lo) != std::cmp::Ordering::Less {
                f(member, score);
            }
        }
    }

    /// Remove and return the lowest-ranked `(member, score)` (ZPOPMIN).
    pub fn pop_min(&mut self) -> Option<(Vec<u8>, f64)> {
        if self.buf.is_empty() {
            return None;
        }
        let (m, score, end) = self.record_at(0);
        let out = (m.to_vec(), score);
        self.buf.drain(0..end);
        self.len -= 1;
        Some(out)
    }

    /// Remove and return the highest-ranked `(member, score)` (ZPOPMAX).
    pub fn pop_max(&mut self) -> Option<(Vec<u8>, f64)> {
        self.pop_max_impl::<true>()
    }

    /// Shared candidate/reference body for same-binary proof. Finding the final record needs only
    /// record boundaries; production skips every discarded member/score decode, while
    /// `MEMBER_ONLY=false` retains the exact pre-change traversal.
    #[cfg_attr(feature = "bench-reference", inline(never))]
    pub fn pop_max_impl<const MEMBER_ONLY: bool>(&mut self) -> Option<(Vec<u8>, f64)> {
        if self.buf.is_empty() {
            return None;
        }
        // Walk to the last record's start.
        let mut pos = 0;
        let mut last_start = 0;
        if MEMBER_ONLY {
            while pos < self.buf.len() {
                last_start = pos;
                let (mlen, m_start) = read_varint(&self.buf, pos);
                pos = m_start + mlen + 8;
            }
        } else {
            while pos < self.buf.len() {
                last_start = pos;
                let (_member, _score, end) = self.record_at(pos);
                pos = end;
            }
        }
        let (m, score, _end) = self.record_at(last_start);
        let out = (m.to_vec(), score);
        self.buf.truncate(last_start);
        self.len -= 1;
        Some(out)
    }
}

/// Borrowing iterator over `(member, score)` in ascending order.
pub struct PackedZSetIter<'a> {
    zset: &'a PackedZSet,
    pos: usize,
}

impl<'a> Iterator for PackedZSetIter<'a> {
    type Item = (&'a [u8], f64);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.zset.buf.len() {
            return None;
        }
        let (m, s, end) = self.zset.record_at(self.pos);
        self.pos = end;
        Some((m, s))
    }
}

/// (frankenredis-ym6ih) Pre-optimization delete path, kept ONLY for the A/B
/// micro-bench `swap_remove_perf_legacy_vs_new_ym6ih`. This is the original
/// `swap_remove`: it re-probes the index by field bytes twice (tombstone +
/// repoint) and allocates the moved field's bytes — exactly the per-delete work
/// the slot back-pointer + `lookup_slot` change eliminates.
#[cfg(test)]
impl CompactFieldMap {
    fn tombstone_slot_legacy(&mut self, field: &[u8]) {
        let mask = self.slots.len() - 1;
        let mut slot = (self.hash(field) as usize) & mask;
        loop {
            let s = self.slots[slot];
            if s >= 2 {
                let pos = (s - 2) as usize;
                let (fr, _) = cfm_decode(&self.buf, self.order[pos]);
                if &self.buf[fr] == field {
                    self.slots[slot] = CFM_TOMB;
                    self.tombs += 1;
                    return;
                }
            }
            slot = (slot + 1) & mask;
        }
    }

    fn repoint_slot_legacy(&mut self, field: &[u8], pos: usize) {
        let mask = self.slots.len() - 1;
        let mut slot = (self.hash(field) as usize) & mask;
        loop {
            let s = self.slots[slot];
            if s >= 2 {
                let cur = (s - 2) as usize;
                let (fr, _) = cfm_decode(&self.buf, self.order[cur]);
                if &self.buf[fr] == field {
                    self.slots[slot] = (pos as u32) + 2;
                    return;
                }
            }
            slot = (slot + 1) & mask;
        }
    }

    fn swap_remove_legacy(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        let pos = self.lookup(field)?;
        let off = self.order[pos];
        let (_, vr) = cfm_decode(&self.buf, off);
        let value = self.buf[vr].to_vec();
        self.dead += self.entry_size(off);
        self.tombstone_slot_legacy(field);
        let last = self.order.len() - 1;
        if pos != last {
            let moved_off = self.order[last];
            self.order[pos] = moved_off;
            let (mfr, _) = cfm_decode(&self.buf, moved_off);
            let mfield = self.buf[mfr].to_vec();
            self.repoint_slot_legacy(&mfield, pos);
        }
        self.order.pop();
        // Keep `slot_of` length consistent with `order` so `rehash`/`maybe_compact`
        // stay sound for repeated legacy deletes (legacy never reads `slot_of`).
        self.slot_of.pop();
        self.maybe_compact();
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChunkedList, CompactFieldMap, CompactStrSet, LIST_CHUNK_TARGET, ListChunk, ListRepr,
        ListValue, PACKED_MAX_ENTRIES, PACKED_STREAM_NODE_MAX_ENTRIES, PackedList, PackedStrMap,
        PackedStrSet, PackedStreamFields, PackedStreamLog, PackedZSet, zset_cmp,
    };

    #[test]
    fn list_lp_int_canonical_probe_matches_roundtrip_without_alloc_bssrh() {
        fn old_roundtrip_int(entry: &[u8]) -> Option<i64> {
            if entry.is_empty() || entry.len() >= 21 {
                return None;
            }
            let value: i64 = std::str::from_utf8(entry).ok()?.parse().ok()?;
            if value.to_string().as_bytes() == entry {
                Some(value)
            } else {
                None
            }
        }

        fn old_entry_bytes(elem: &[u8]) -> u64 {
            let data_len: u64 = if let Some(v) = old_roundtrip_int(elem) {
                if (0..=127).contains(&v) {
                    1
                } else if (-4096..=4095).contains(&v) {
                    2
                } else if i16::try_from(v).is_ok() {
                    3
                } else if (-8_388_608..=8_388_607).contains(&v) {
                    4
                } else if i32::try_from(v).is_ok() {
                    5
                } else {
                    9
                }
            } else {
                let header = if elem.len() < 64 {
                    1
                } else if elem.len() < 4096 {
                    2
                } else {
                    5
                };
                header + elem.len() as u64
            };
            data_len + super::list_lp_backlen_bytes(data_len)
        }

        let cases: &[&[u8]] = &[
            b"",
            b"0",
            b"-0",
            b"00",
            b"007",
            b"+1",
            b"1",
            b"-1",
            b"127",
            b"128",
            b"-4096",
            b"4095",
            b"32767",
            b"32768",
            b"-8388608",
            b"8388607",
            b"2147483647",
            b"2147483648",
            b"-9223372036854775808",
            b"9223372036854775807",
            b"9223372036854775808",
            b"-9223372036854775809",
            b"18446744073709551615",
            b"1a",
            b"-",
            b"123456789012345678901",
        ];

        for case in cases {
            assert_eq!(
                super::list_lp_int(case),
                old_roundtrip_int(case),
                "canonical integer parse mismatch for {:?}",
                std::str::from_utf8(case).unwrap_or("<non-utf8>")
            );
            assert_eq!(
                super::list_lp_entry_bytes(case),
                old_entry_bytes(case),
                "listpack byte sizing changed for {:?}",
                std::str::from_utf8(case).unwrap_or("<non-utf8>")
            );
        }

        assert_eq!(super::list_lp_int(b"0"), Some(0));
        assert_eq!(super::list_lp_int(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(super::list_lp_int(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(super::list_lp_int(b"-0"), None);
        assert_eq!(super::list_lp_int(b"+1"), None);
        assert_eq!(super::list_lp_int(b"9223372036854775808"), None);
    }

    #[test]
    fn packed_stream_fields_round_trips_p8wd1() {
        // PackedStreamFields must losslessly round-trip an ORDERED list of
        // (field, value) pairs (incl. empty, binary, duplicate field names),
        // matching the former Vec<(Vec<u8>,Vec<u8>)> exactly.
        let cases: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![
            vec![],
            vec![(b"f".to_vec(), b"v".to_vec())],
            vec![
                (b"field_a".to_vec(), b"value_data_1".to_vec()),
                (b"field_b".to_vec(), b"".to_vec()),
                (b"field_a".to_vec(), b"dup_field_name".to_vec()),
                (b"\x00\xff".to_vec(), b"\r\n\x00bin".to_vec()),
            ],
            (0..200)
                .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                .collect(),
        ];
        for pairs in cases {
            let packed = PackedStreamFields::from_pairs(&pairs);
            assert_eq!(packed.len(), pairs.len());
            assert_eq!(packed.is_empty(), pairs.is_empty());
            assert_eq!(packed.to_pairs(), pairs, "to_pairs round-trip");
            let iter: Vec<(Vec<u8>, Vec<u8>)> = packed
                .iter()
                .map(|(f, v)| (f.to_vec(), v.to_vec()))
                .collect();
            assert_eq!(iter, pairs, "iter order/content");
            // Rebuilding from a borrowed-pair slice matches.
            let refs: Vec<(&[u8], &[u8])> = pairs
                .iter()
                .map(|(f, v)| (f.as_slice(), v.as_slice()))
                .collect();
            assert_eq!(PackedStreamFields::from_pairs(&refs), packed);
        }
    }
    #[test]
    fn packed_stream_log_matches_btreemap_oracle_p8wd1() {
        use std::collections::BTreeMap;
        // PackedStreamLog must be a drop-in for BTreeMap<StreamId,
        // PackedStreamFields>: same get/range/iter/last/first results and the
        // SAME packed bytes per entry, across insert (monotonic XADD), overwrite,
        // remove (XDEL), and front-trim (XTRIM) — including compaction churn.
        type Pairs = Vec<(Vec<u8>, Vec<u8>)>;
        let mk = |i: u64| -> Pairs {
            vec![
                (b"field_a".to_vec(), format!("value_{i}").into_bytes()),
                (b"field_b".to_vec(), format!("more_{i}").into_bytes()),
                (b"seq".to_vec(), i.to_string().into_bytes()),
            ]
        };
        let mut log = PackedStreamLog::new();
        let mut oracle: BTreeMap<(u64, u64), Pairs> = BTreeMap::new();
        // Monotonic XADD of 1000 entries.
        for i in 0..1000u64 {
            let id = (i, 0);
            let pairs = mk(i);
            assert_eq!(
                log.insert(id, &pairs),
                oracle.insert(id, pairs.clone()).is_some()
            );
        }
        // Overwrite a few ids (XSETID/replace semantics).
        for i in [10u64, 500, 999] {
            let pairs = mk(i + 10_000);
            assert!(log.insert((i, 0), &pairs)); // existed
            oracle.insert((i, 0), pairs);
        }
        // XDEL a scattered third (forces dead bytes + eventual compaction).
        for i in (0..1000u64).step_by(3) {
            assert_eq!(log.remove((i, 0)), oracle.remove(&(i, 0)).is_some());
        }
        // XTRIM-style front trim of the oldest 100 surviving ids.
        let trim: Vec<(u64, u64)> = oracle.keys().take(100).copied().collect();
        for id in trim {
            assert!(log.remove(id));
            oracle.remove(&id);
        }
        // Equivalence: len, get (incl. exact decoded pairs), full iter, ranges.
        assert_eq!(log.len(), oracle.len());
        assert_eq!(log.first_id(), oracle.keys().next().copied());
        assert_eq!(log.last_id(), oracle.keys().next_back().copied());
        assert!(
            log.nodes.len() + usize::from(log.tail.is_some()) < oracle.len(),
            "stream entries are grouped into nodes"
        );
        assert!(
            log.nodes
                .values()
                .chain(log.tail.iter())
                .all(|node| node.entries.len() <= PACKED_STREAM_NODE_MAX_ENTRIES),
            "each stream node obeys Redis's default stream-node-max-entries cap"
        );
        for (id, want) in &oracle {
            let got = log.get(*id).expect("present");
            // SAMEFIELDS encodes field NAMES once in the dict + a per-entry
            // index, so the raw arena bytes differ from the old per-entry-name
            // layout; the DECODED pairs (what DUMP/XRANGE/DIGEST observe) must be
            // identical.
            assert_eq!(&got.to_pairs(), want, "fields for {id:?}");
            assert_eq!(got.len(), want.len());
        }
        assert!(log.get((1, 0)).is_none()); // trimmed
        // Full iteration matches the oracle order/content.
        let log_iter: Vec<((u64, u64), Pairs)> =
            log.iter().map(|(id, f)| (*id, f.to_pairs())).collect();
        let oracle_iter: Vec<((u64, u64), Pairs)> =
            oracle.iter().map(|(id, p)| (*id, p.clone())).collect();
        assert_eq!(log_iter, oracle_iter, "iter equivalence");
        // XRANGE/XREVRANGE boundary shapes, including ranges entirely outside
        // the stream and bounds that start in an inter-entry gap.
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let range_cases = [
            ("unbounded", (Unbounded, Unbounded)),
            ("below-first", (Included((0, 0)), Excluded((1, 0)))),
            ("included", (Included((300, 0)), Included((700, 0)))),
            ("excluded-gap", (Excluded((300, 0)), Excluded((700, 0)))),
            ("gap-start", (Included((350, 1)), Included((700, 0)))),
            ("above-last", (Excluded((2_000, 0)), Unbounded)),
        ];
        for (label, bounds) in range_cases {
            let log_range: Vec<(u64, u64)> = log.range(bounds).map(|(id, _)| *id).collect();
            let oracle_range: Vec<(u64, u64)> = oracle.range(bounds).map(|(id, _)| *id).collect();
            assert_eq!(log_range, oracle_range, "{label} forward range");
            assert_eq!(
                log.range(bounds)
                    .rev()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                oracle_range.into_iter().rev().collect::<Vec<_>>(),
                "{label} reversed range"
            );
        }
        // Arena did not leak unbounded dead bytes after all the churn.
        assert!(
            log.arena.len()
                <= (log.dead
                    + oracle_iter
                        .iter()
                        .map(|(_, p)| { PackedStreamFields::from_pairs(p).buf.len() })
                        .sum::<usize>())
                    + 1
        );
    }

    #[test]
    fn packed_stream_monotonic_append_matches_fallback_he1yu() {
        type Pairs = Vec<(Vec<u8>, Vec<u8>)>;
        let pairs =
            |i: u64| -> Pairs { vec![(b"field".to_vec(), format!("value:{i}").into_bytes())] };
        let assert_same = |candidate: &PackedStreamLog, fallback: &PackedStreamLog| {
            assert_eq!(candidate.arena, fallback.arena);
            assert_eq!(candidate.field_dict, fallback.field_dict);
            assert_eq!(candidate.dead, fallback.dead);
            assert_eq!(candidate.len, fallback.len);
            let contents = |log: &PackedStreamLog| {
                log.iter()
                    .map(|(id, fields)| (*id, fields.to_pairs()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(contents(candidate), contents(fallback));
            assert_eq!(candidate.bench_node_layout(), fallback.bench_node_layout());
        };

        let mut candidate = PackedStreamLog::new();
        let mut fallback = PackedStreamLog::new();
        // Cross 99/100/101 and a second full-node boundary.
        for i in 1..=201_u64 {
            let fields = pairs(i);
            assert_eq!(
                candidate.insert((i, 0), &fields),
                fallback.bench_insert_fallback((i, 0), &fields)
            );
        }
        assert_same(&candidate, &fallback);

        // Equal-ID overwrite plus front and full-middle insertions retain the
        // exact B-tree lookup/split fallback and node boundaries.
        let overwritten = pairs(10_000);
        assert_eq!(
            candidate.insert((100, 0), &overwritten),
            fallback.bench_insert_fallback((100, 0), &overwritten)
        );
        let front = pairs(10_001);
        assert_eq!(
            candidate.insert((0, 0), &front),
            fallback.bench_insert_fallback((0, 0), &front)
        );
        let out_of_order = pairs(10_001);
        assert_eq!(
            candidate.insert((150, 1), &out_of_order),
            fallback.bench_insert_fallback((150, 1), &out_of_order)
        );
        assert_same(&candidate, &fallback);

        // Removing every entry leaves an empty node map; the next append must rebuild the exact
        // first-node layout rather than assuming a surviving tail node.
        let ids: Vec<(u64, u64)> = candidate.iter().map(|(id, _)| *id).collect();
        for id in ids {
            assert_eq!(candidate.remove(id), fallback.remove(id));
        }
        assert!(candidate.is_empty());
        let after_empty = pairs(20_000);
        assert_eq!(
            candidate.insert((500, 0), &after_empty),
            fallback.bench_insert_fallback((500, 0), &after_empty)
        );
        assert_same(&candidate, &fallback);
    }

    use indexmap::{IndexMap, IndexSet};
    use proptest::prelude::*;
    use std::collections::VecDeque;

    /// (frankenredis-ym6ih) A/B micro-bench isolating the per-delete work that the
    /// slot back-pointer + `lookup_slot` + value-free `delete` removed. Builds an
    /// identical large hashtable-range map, then deletes every field two ways:
    /// the pre-optimization `swap_remove_legacy` (3 probes + 2 allocs/delete) vs
    /// the new `delete` (1 probe, 0 owned allocs). Both share the same
    /// `maybe_compact`, so the wall-clock delta is pure per-op savings. Ignored by
    /// default (timing); run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn swap_remove_perf_legacy_vs_new_ym6ih() {
        use std::time::Instant;
        const N: usize = 300_000;
        let build = || {
            let mut m = CompactFieldMap::new();
            for i in 0..N {
                let f = format!("field:{i:012}");
                m.insert(f.as_bytes(), b"v");
            }
            m
        };
        // Distinct, shuffled-ish delete order (every field hit once, present).
        let order: Vec<Vec<u8>> = (0..N)
            .map(|i| format!("field:{:012}", (i.wrapping_mul(2_654_435_761)) % N).into_bytes())
            .collect();
        // Dedup so each field is deleted exactly once (multiplicative hash collisions).
        let mut seen = std::collections::HashSet::new();
        let dels: Vec<&[u8]> = order
            .iter()
            .filter(|f| seen.insert((*f).clone()))
            .map(|f| f.as_slice())
            .collect();

        let mut legacy = build();
        let t0 = Instant::now();
        let mut lc = 0u64;
        for f in &dels {
            if legacy.swap_remove_legacy(f).is_some() {
                lc += 1;
            }
        }
        let legacy_ns = t0.elapsed().as_nanos();

        let mut newm = build();
        let t1 = Instant::now();
        let mut nc = 0u64;
        for f in &dels {
            if newm.delete(f) {
                nc += 1;
            }
        }
        let new_ns = t1.elapsed().as_nanos();

        assert_eq!(lc, nc, "both paths must remove the same field count");
        assert_eq!(legacy.len(), newm.len(), "same residual size");
        let speedup = legacy_ns as f64 / new_ns as f64;
        eprintln!(
            "[ym6ih] CompactFieldMap delete {} fields: legacy={:.2}ms new={:.2}ms  speedup={:.3}x  ({:.0}ns vs {:.0}ns per delete)",
            nc,
            legacy_ns as f64 / 1e6,
            new_ns as f64 / 1e6,
            speedup,
            legacy_ns as f64 / nc as f64,
            new_ns as f64 / nc as f64,
        );
        assert!(
            new_ns as f64 <= legacy_ns as f64 * 0.95,
            "new delete must be at least 5% faster than legacy (got {speedup:.3}x)"
        );
    }

    #[test]
    fn compact_str_set_matches_indexset_under_random_ops_ideww() {
        // CompactStrSet must be a byte-for-byte drop-in for the IndexSet<member>
        // backing GenericSet::Hash: same returns + insertion-order iteration +
        // positional access across insert/contains/get_index/shift_remove
        // [order-preserving] / swap_remove [unordered] / swap_remove_index / retain.
        let mut rng: u64 = 0xD1B54A32D192ED03;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let key = |n: u64| format!("member_{}", n % 50).into_bytes();
        let mut c = CompactStrSet::new();
        let mut o: IndexSet<Vec<u8>, foldhash::quality::RandomState> =
            IndexSet::with_hasher(foldhash::quality::RandomState::default());
        let check = |c: &CompactStrSet, o: &IndexSet<Vec<u8>, _>| {
            assert_eq!(c.len(), o.len());
            let ci: Vec<Vec<u8>> = c.iter().map(<[u8]>::to_vec).collect();
            let oi: Vec<Vec<u8>> = o.iter().cloned().collect();
            assert_eq!(ci, oi, "iteration order");
            for i in 0..o.len() {
                assert_eq!(c.get_index(i).map(<[u8]>::to_vec), o.get_index(i).cloned());
            }
        };
        for _ in 0..20_000 {
            let r = next();
            let m = key(r);
            match r % 12 {
                0..=4 => assert_eq!(c.insert(&m), o.insert(m.clone()), "insert"),
                5 => assert_eq!(c.contains(&m), o.contains(&m[..]), "contains"),
                6 => assert_eq!(c.shift_remove(&m), o.shift_remove(&m[..]), "shift_remove"),
                7 => assert_eq!(c.swap_remove(&m), o.swap_remove(&m[..]), "swap_remove"),
                8 => {
                    let idx = (next() as usize) % (o.len() + 1);
                    assert_eq!(
                        c.swap_remove_index(idx),
                        o.swap_remove_index(idx),
                        "swap_remove_index"
                    );
                }
                9 => {
                    let keep = next() % 3;
                    c.retain(|x| x.last().copied().unwrap_or(0) as u64 % 3 != keep);
                    o.retain(|x| x.last().copied().unwrap_or(0) as u64 % 3 != keep);
                }
                _ => {
                    let idx = (next() as usize) % (o.len() + 1);
                    assert_eq!(
                        c.get_index(idx).map(<[u8]>::to_vec),
                        o.get_index(idx).cloned()
                    );
                }
            }
            check(&c, &o);
        }
    }

    #[test]
    fn compact_field_map_matches_indexmap_under_random_ops_ideww() {
        // CompactFieldMap must be a byte-for-byte drop-in for the
        // IndexMap<field,value> backing HashFieldMap::Hash: same returns, same
        // insertion-order iteration, same positional access, across a long
        // randomized op stream (insert incl. updates, get, contains, get_index,
        // shift_remove [order-preserving], swap_remove [unordered]).
        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        // Small key space to force collisions / updates / re-inserts.
        let key = |n: u64| format!("field_{}", n % 40).into_bytes();
        let val = |n: u64| format!("value_data_{}", n).into_bytes();

        let mut c = CompactFieldMap::new();
        let mut o: IndexMap<Vec<u8>, Vec<u8>, foldhash::quality::RandomState> =
            IndexMap::with_hasher(foldhash::quality::RandomState::default());

        let check = |c: &CompactFieldMap, o: &IndexMap<Vec<u8>, Vec<u8>, _>| {
            assert_eq!(c.len(), o.len(), "len");
            let ci: Vec<(Vec<u8>, Vec<u8>)> =
                c.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
            let oi: Vec<(Vec<u8>, Vec<u8>)> =
                o.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(ci, oi, "iteration order/content");
            for i in 0..o.len() {
                let cp = c.get_index(i).map(|(k, v)| (k.to_vec(), v.to_vec()));
                let op = o.get_index(i).map(|(k, v)| (k.clone(), v.clone()));
                assert_eq!(cp, op, "get_index({i})");
            }
        };

        for _ in 0..20_000 {
            let r = next();
            let k = key(r);
            match r % 11 {
                0..=4 => {
                    let v = val(next());
                    assert_eq!(c.insert(&k, &v), o.insert(k.clone(), v.clone()), "insert");
                }
                5 => assert_eq!(c.get(&k).map(<[u8]>::to_vec), o.get(&k).cloned(), "get"),
                6 => assert_eq!(c.contains_key(&k), o.contains_key(&k), "contains"),
                7 => assert_eq!(c.shift_remove(&k), o.shift_remove(&k), "shift_remove"),
                8 => assert_eq!(c.swap_remove(&k), o.swap_remove(&k), "swap_remove"),
                9 => {
                    let v = val(next());
                    let was_new = !o.contains_key(&k);
                    assert_eq!(c.insert_borrowed(&k, &v), was_new, "insert_borrowed");
                    o.insert(k.clone(), v);
                }
                _ => {
                    let idx = (next() as usize) % (o.len() + 1);
                    let cp = c.get_index(idx).map(|(k, v)| (k.to_vec(), v.to_vec()));
                    let op = o.get_index(idx).map(|(k, v)| (k.clone(), v.clone()));
                    assert_eq!(cp, op, "get_index");
                }
            }
            check(&c, &o);
        }
        assert!(!c.is_empty(), "expected a non-trivial residual map");
    }

    #[test]
    fn compact_field_map_borrowed_overwrite_reuses_same_size_slot_ohsk5() {
        let mut map = CompactFieldMap::new();
        assert_eq!(map.insert(b"field", b"aaaa"), None);
        let one_record_len = map.buf.len();

        assert!(!map.insert_borrowed(b"field", b"bbbb"));
        assert_eq!(map.get(b"field"), Some(&b"bbbb"[..]));
        assert_eq!(map.len(), 1);
        assert_eq!(map.buf.len(), one_record_len);
        assert_eq!(map.dead, 0);

        assert_eq!(map.insert(b"field", b"cccc"), Some(b"bbbb".to_vec()));
        assert_eq!(map.get(b"field"), Some(&b"cccc"[..]));
        assert_eq!(map.buf.len(), one_record_len);
        assert_eq!(map.dead, 0);

        assert!(!map.insert_borrowed(b"field", b"longer-value"));
        assert_eq!(map.get(b"field"), Some(&b"longer-value"[..]));
        assert_eq!(map.len(), 1);
        assert!(map.buf.len() > one_record_len);

        assert!(map.insert_borrowed(b"other", b"zzzz"));
        let got: Vec<(Vec<u8>, Vec<u8>)> =
            map.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        assert_eq!(
            got,
            vec![
                (b"field".to_vec(), b"longer-value".to_vec()),
                (b"other".to_vec(), b"zzzz".to_vec()),
            ]
        );
    }

    #[test]
    fn insert_dedup_order_contains() {
        let mut s = PackedStrSet::new();
        assert!(s.insert(b"alpha"));
        assert!(s.insert(b"beta"));
        assert!(s.insert(b"gamma"));
        assert!(!s.insert(b"beta")); // dup
        assert_eq!(s.len(), 3);
        let got: Vec<&[u8]> = s.iter().collect();
        assert_eq!(got, vec![&b"alpha"[..], b"beta", b"gamma"]);
        assert!(s.contains(b"alpha"));
        assert!(!s.contains(b"delta"));
    }

    #[test]
    fn remove_preserves_order() {
        let mut s = PackedStrSet::new();
        for m in [&b"a"[..], b"b", b"c", b"d"] {
            s.insert(m);
        }
        assert!(s.remove(b"b"));
        assert!(!s.remove(b"b")); // already gone
        assert_eq!(s.len(), 3);
        let got: Vec<&[u8]> = s.iter().collect();
        assert_eq!(got, vec![&b"a"[..], b"c", b"d"]);
        assert!(s.remove(b"a")); // remove head
        assert!(s.remove(b"d")); // remove tail
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![&b"c"[..]]);
    }

    #[test]
    fn empty_member_and_varint_boundaries() {
        let mut s = PackedStrSet::new();
        // empty member, and lengths straddling the 1-byte varint boundary (128)
        let big127 = vec![b'x'; 127];
        let big128 = vec![b'y'; 128];
        let big1000 = vec![b'z'; 1000];
        assert!(s.insert(b""));
        assert!(s.insert(&big127));
        assert!(s.insert(&big128));
        assert!(s.insert(&big1000));
        assert_eq!(s.len(), 4);
        assert!(s.contains(b""));
        assert!(s.contains(&big127));
        assert!(s.contains(&big128));
        assert!(s.contains(&big1000));
        let got: Vec<&[u8]> = s.iter().collect();
        assert_eq!(got, vec![&b""[..], &big127, &big128, &big1000]);
    }

    #[test]
    fn generic_hash_set_inline_members_preserve_indexset_semantics() {
        let mut s = super::GenericSet::with_capacity_and_hasher(
            PACKED_MAX_ENTRIES + 1,
            foldhash::quality::RandomState::default(),
        );
        let long = b"abcdefghijklmnopqrstuvwxyz0123456789".to_vec();

        assert!(s.insert_borrowed(b"alpha"));
        assert!(s.insert_borrowed(b"beta"));
        assert!(s.insert_borrowed(&long));
        assert!(!s.insert_borrowed(b"alpha"));
        assert_eq!(s.len(), 3);
        assert!(s.contains(b"alpha"));
        assert!(s.contains(&long));
        assert!(!s.contains(b"delta"));
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            vec![&b"alpha"[..], &b"beta"[..], long.as_slice()]
        );
        assert_eq!(s.get_index(1), Some(&b"beta"[..]));

        assert!(s.shift_remove(b"beta"));
        assert!(!s.shift_remove(b"beta"));
        assert_eq!(
            s.clone().into_iter().collect::<Vec<_>>(),
            vec![b"alpha".to_vec(), long.clone()]
        );
        assert_eq!(s.pop_index(0), Some(b"alpha".to_vec()));
        assert_eq!(s.into_iter().collect::<Vec<_>>(), vec![long]);
    }

    #[test]
    fn hash_field_map_from_unique_pairs_matches_insert_loop_qxfmr() {
        use super::{HashFieldMap, PACKED_MAX_VALUE};
        let big = vec![b'x'; PACKED_MAX_VALUE + 1];
        let atcap = vec![b'y'; PACKED_MAX_VALUE];
        let cases: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![
            vec![],
            (0..1)
                .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                .collect(),
            // Packed boundary: exactly PACKED_MAX_ENTRIES stays Packed.
            (0..PACKED_MAX_ENTRIES)
                .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                .collect(),
            // One past the boundary promotes to Hash.
            (0..=PACKED_MAX_ENTRIES)
                .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                .collect(),
            (0..300)
                .map(|i| (format!("f{i}").into_bytes(), format!("v{i}").into_bytes()))
                .collect(),
            // Value == PACKED_MAX_VALUE stays Packed; one over promotes.
            vec![
                (b"f".to_vec(), atcap.clone()),
                (b"g".to_vec(), b"y".to_vec()),
            ],
            vec![(b"f".to_vec(), big.clone()), (b"g".to_vec(), b"y".to_vec())],
            // Oversize field promotes.
            vec![(big.clone(), b"v".to_vec())],
            // Binary field/value with NUL, CR/LF, high bytes.
            vec![
                (b"f\x00\xff".to_vec(), b"v\r\n\x00".to_vec()),
                (b"g".to_vec(), b"\xfe".to_vec()),
            ],
        ];
        for pairs in cases {
            let mut loop_map = HashFieldMap::default();
            for (f, v) in &pairs {
                loop_map.insert(f.clone(), v.clone());
            }
            let bulk_map = HashFieldMap::from_unique_pairs(pairs.clone());
            // Same encoding variant (Packed vs Hash) — observable via OBJECT
            // ENCODING and the internal repr the incremental path would reach.
            assert_eq!(
                std::mem::discriminant(&loop_map),
                std::mem::discriminant(&bulk_map),
                "variant mismatch for {} pairs",
                pairs.len()
            );
            // Same length and same INSERTION-ORDER iteration (HGETALL/HKEYS).
            assert_eq!(loop_map.len(), bulk_map.len());
            let loop_iter: Vec<(Vec<u8>, Vec<u8>)> = loop_map
                .iter()
                .map(|(f, v)| (f.to_vec(), v.to_vec()))
                .collect();
            let bulk_iter: Vec<(Vec<u8>, Vec<u8>)> = bulk_map
                .iter()
                .map(|(f, v)| (f.to_vec(), v.to_vec()))
                .collect();
            assert_eq!(loop_iter, bulk_iter, "iteration mismatch");
            // Every field resolves to its value.
            for (f, v) in &pairs {
                assert_eq!(bulk_map.get(f), Some(v.as_slice()));
            }
        }
    }

    #[test]
    fn generic_set_from_unique_str_members_matches_insert_loop_saddbulk() {
        use super::{GenericSet, PACKED_MAX_VALUE};
        let big = vec![b'x'; PACKED_MAX_VALUE + 1];
        let atcap = vec![b'y'; PACKED_MAX_VALUE];
        let cases: Vec<Vec<Vec<u8>>> = vec![
            vec![b"a".to_vec()],
            // Packed boundary: exactly PACKED_MAX_ENTRIES stays Packed.
            (0..PACKED_MAX_ENTRIES)
                .map(|i| format!("m{i}").into_bytes())
                .collect(),
            // One past the boundary promotes to Hash.
            (0..=PACKED_MAX_ENTRIES)
                .map(|i| format!("m{i}").into_bytes())
                .collect(),
            (0..400).map(|i| format!("m{i}").into_bytes()).collect(),
            // Member == PACKED_MAX_VALUE stays Packed; one over promotes to Hash.
            vec![atcap.clone(), b"z".to_vec()],
            vec![big.clone(), b"z".to_vec()],
            // Binary members with NUL, CR/LF, high bytes.
            vec![b"m\x00\xff".to_vec(), b"n\r\n".to_vec(), b"\xfe".to_vec()],
        ];
        for members in cases {
            // Reference: incremental borrowed inserts from an empty generic set,
            // exactly what SADD's fresh-key loop reaches once it is generic.
            let mut loop_set = GenericSet::default();
            for m in &members {
                loop_set.insert_borrowed(m);
            }
            let bulk_set = GenericSet::from_unique_str_members(&members);
            assert_eq!(
                std::mem::discriminant(&loop_set),
                std::mem::discriminant(&bulk_set),
                "variant mismatch for {} members",
                members.len()
            );
            assert_eq!(loop_set.len(), bulk_set.len());
            let loop_iter: Vec<Vec<u8>> = loop_set.iter().map(<[u8]>::to_vec).collect();
            let bulk_iter: Vec<Vec<u8>> = bulk_set.iter().map(<[u8]>::to_vec).collect();
            assert_eq!(loop_iter, bulk_iter, "iteration mismatch");
            for m in &members {
                assert!(bulk_set.contains(m));
            }
        }
    }

    #[test]
    fn hash_field_map_inline_hash_bytes_preserve_indexmap_semantics() {
        let mut map = super::HashFieldMap::Hash(super::CompactFieldMap::new());
        let mut oracle: IndexMap<Vec<u8>, Vec<u8>> = IndexMap::new();
        let long_field = b"abcdefghijklmnopqrstuvwxyz0123456789".to_vec();
        let long_value = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".to_vec();

        for (field, value) in [
            (b"alpha".to_vec(), b"one".to_vec()),
            (b"beta".to_vec(), b"two".to_vec()),
            (long_field.clone(), long_value.clone()),
        ] {
            assert_eq!(
                map.insert(field.clone(), value.clone()),
                oracle.insert(field, value)
            );
        }

        assert!(!map.insert_borrowed(b"alpha", b"uno".to_vec()));
        oracle.insert(b"alpha".to_vec(), b"uno".to_vec());
        assert!(map.insert_borrowed(b"gamma", b"three".to_vec()));
        oracle.insert(b"gamma".to_vec(), b"three".to_vec());

        for (field, value) in &oracle {
            assert_eq!(map.get(field), Some(value.as_slice()));
            assert!(map.contains_key(field));
        }
        assert_eq!(map.get(b"missing"), None);
        assert!(!map.contains_key(b"missing"));
        assert_eq!(map.get_index(1), Some((&b"beta"[..], &b"two"[..])));

        let map_items: Vec<(Vec<u8>, Vec<u8>)> = map
            .iter()
            .map(|(field, value)| (field.to_vec(), value.to_vec()))
            .collect();
        let oracle_items: Vec<(Vec<u8>, Vec<u8>)> = oracle
            .iter()
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect();
        assert_eq!(map_items, oracle_items);

        assert_eq!(map.shift_remove(b"beta"), oracle.shift_remove(&b"beta"[..]));
        assert_eq!(
            map.swap_remove(&long_field),
            oracle.swap_remove(&long_field)
        );
        let map_items: Vec<(Vec<u8>, Vec<u8>)> = map
            .iter()
            .map(|(field, value)| (field.to_vec(), value.to_vec()))
            .collect();
        let oracle_items: Vec<(Vec<u8>, Vec<u8>)> = oracle
            .iter()
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect();
        assert_eq!(map_items, oracle_items);
    }

    proptest! {
        /// PackedStrSet must behave EXACTLY like an insertion-ordered IndexSet
        /// under an arbitrary op stream: same membership, same length, same
        /// iteration order. This is the isomorphism the SetValue wiring relies on.
        #[test]
        fn equivalent_to_indexset(ops in proptest::collection::vec(
            (any::<bool>(), proptest::collection::vec(0u8..4, 0..5)), 0..300)) {
            let mut packed = PackedStrSet::new();
            let mut oracle: IndexSet<Vec<u8>> = IndexSet::new();
            for (is_insert, member) in ops {
                if is_insert {
                    let a = packed.insert(&member);
                    let b = oracle.insert(member.clone());
                    prop_assert_eq!(a, b);
                } else {
                    let a = packed.remove(&member);
                    let b = oracle.shift_remove(&member);
                    prop_assert_eq!(a, b);
                }
                prop_assert_eq!(packed.len(), oracle.len());
                prop_assert_eq!(packed.contains(&member), oracle.contains(&member[..]));
                let p: Vec<&[u8]> = packed.iter().collect();
                let o: Vec<&[u8]> = oracle.iter().map(|v| v.as_slice()).collect();
                prop_assert_eq!(p, o);
            }
        }

        /// PackedStrMap must behave EXACTLY like an insertion-ordered IndexMap:
        /// insert returns the previous value AND keeps the field's position on
        /// update, get/contains/len/shift_remove match, and iteration order is
        /// identical. The isomorphism the HashFieldMap wiring relies on.
        #[test]
        fn map_equivalent_to_indexmap(ops in proptest::collection::vec(
            (0u8..4, proptest::collection::vec(0u8..3, 0..4), proptest::collection::vec(0u8..9, 0..4)),
            0..300)) {
            let mut packed = PackedStrMap::new();
            let mut oracle: IndexMap<Vec<u8>, Vec<u8>> = IndexMap::new();
            for (op, field, value) in ops {
                match op {
                    0 => {
                        let a = packed.insert(field.clone(), value.clone());
                        let b = oracle.insert(field.clone(), value.clone());
                        prop_assert_eq!(a, b);
                    }
                    1 => {
                        let a = packed.shift_remove(&field);
                        let b = oracle.shift_remove(&field);
                        prop_assert_eq!(a, b);
                    }
                    2 => {
                        prop_assert_eq!(packed.get(&field), oracle.get(&field[..]).map(|v| v.as_slice()));
                        prop_assert_eq!(packed.contains_key(&field), oracle.contains_key(&field[..]));
                    }
                    _ => {
                        let a = packed.insert_borrowed(&field, value.clone());
                        let b = !oracle.contains_key(&field[..]);
                        oracle.insert(field.clone(), value.clone());
                        prop_assert_eq!(a, b);
                    }
                }
                prop_assert_eq!(packed.get(&field), oracle.get(&field[..]).map(|v| v.as_slice()));
                prop_assert_eq!(packed.contains_key(&field), oracle.contains_key(&field[..]));
                prop_assert_eq!(packed.len(), oracle.len());
                let p: Vec<(&[u8], &[u8])> = packed.iter().collect();
                let o: Vec<(&[u8], &[u8])> =
                    oracle.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
                prop_assert_eq!(p, o);
            }
        }
    }

    #[test]
    fn list_basic_ops_and_order() {
        let mut l = PackedList::new();
        l.push_back(b"b");
        l.push_back(b"c");
        l.push_front(b"a");
        assert_eq!(l.iter().collect::<Vec<_>>(), vec![&b"a"[..], b"b", b"c"]);
        assert_eq!(l.get(1), Some(&b"b"[..]));
        assert!(l.set(1, b"BBBBB")); // value-length change
        assert_eq!(l.get(1), Some(&b"BBBBB"[..]));
        l.insert(1, b"x"); // before index 1
        assert_eq!(
            l.iter().collect::<Vec<_>>(),
            vec![&b"a"[..], b"x", b"BBBBB", b"c"]
        );
        assert_eq!(l.remove(0), Some(b"a".to_vec()));
        assert_eq!(l.pop_back(), Some(b"c".to_vec()));
        assert_eq!(l.pop_front(), Some(b"x".to_vec()));
        assert_eq!(l.iter().collect::<Vec<_>>(), vec![&b"BBBBB"[..]]);
        assert_eq!(l.len(), 1);
    }

    fn list_test_value(idx: usize) -> Vec<u8> {
        idx.to_ne_bytes().to_vec()
    }

    #[test]
    fn list_value_clone_shares_large_deque_until_mutation() {
        let mut source = ListValue::default();
        for idx in 0..2_000 {
            source.push_back(list_test_value(idx));
        }

        let mut copy = source.clone();
        let (source_deque, copy_deque) = match (&source.repr, &copy.repr) {
            (ListRepr::Deque(source_deque), ListRepr::Deque(copy_deque)) => {
                (source_deque, copy_deque)
            }
            _ => {
                unreachable!("large list must promote to deque storage");
            }
        };
        assert!(std::sync::Arc::ptr_eq(source_deque, copy_deque));

        assert!(copy.set(0, b"changed".to_vec()));
        let zero = list_test_value(0);
        assert_eq!(source.get(0), Some(zero.as_slice()));
        assert_eq!(copy.get(0), Some(&b"changed"[..]));

        let (source_deque, copy_deque) = match (&source.repr, &copy.repr) {
            (ListRepr::Deque(source_deque), ListRepr::Deque(copy_deque)) => {
                (source_deque, copy_deque)
            }
            _ => {
                unreachable!("large list must stay in deque storage");
            }
        };
        assert!(!std::sync::Arc::ptr_eq(source_deque, copy_deque));
        match (
            source_deque.chunks.front(),
            copy_deque.chunks.front(),
            source_deque.chunks.get(1),
            copy_deque.chunks.get(1),
        ) {
            (Some(source_front), Some(copy_front), Some(source_tail), Some(copy_tail)) => {
                // (frankenredis-99fwc) The first chunk crossed Redis's quicklist
                // node boundary and was sealed when the second chunk started, so
                // the untouched source front is `Listpack`; `copy.set(0)`
                // mutated element 0, re-materializing the copy's front back to
                // `Owned`. The mutated chunk thus diverged.
                assert!(matches!(source_front, ListChunk::Listpack { .. }));
                let ListChunk::Owned {
                    elems: _copy_front, ..
                } = copy_front
                else {
                    unreachable!("the mutated copy's front chunk re-materializes to owned");
                };
                // The untouched tail remains shared in whichever representation
                // quicklist-boundary sealing chose for that chunk.
                match (source_tail, copy_tail) {
                    (
                        ListChunk::Owned {
                            elems: source_tail, ..
                        },
                        ListChunk::Owned {
                            elems: copy_tail, ..
                        },
                    ) => assert!(std::sync::Arc::ptr_eq(source_tail, copy_tail)),
                    (
                        ListChunk::Listpack {
                            bytes: source_bytes,
                            entries: source_entries,
                        },
                        ListChunk::Listpack {
                            bytes: copy_bytes,
                            entries: copy_entries,
                        },
                    ) => {
                        assert!(std::sync::Arc::ptr_eq(source_bytes, copy_bytes));
                        assert!(std::sync::Arc::ptr_eq(source_entries, copy_entries));
                    }
                    _ => {
                        unreachable!("untouched tail chunks must retain shared representation");
                    }
                }
            }
            _ => {
                unreachable!("large deque must split on Redis quicklist node boundaries");
            }
        }
    }

    #[test]
    fn list_value_cow_mutations_preserve_independent_order() {
        let mut left = ListValue::default();
        let original_len = PACKED_MAX_ENTRIES + 1;
        for idx in 0..original_len {
            left.push_back(list_test_value(idx));
        }
        let mut right = left.clone();

        assert_eq!(left.pop_front(), Some(list_test_value(0)));
        right.push_front(b"prefix__".to_vec());
        right.push_back(b"suffix__".to_vec());

        let one = list_test_value(1);
        assert_eq!(left.get(0), Some(one.as_slice()));
        assert_eq!(right.get(0), Some(&b"prefix__"[..]));
        assert_eq!(right.get(right.len() - 1), Some(&b"suffix__"[..]));
        assert_eq!(left.len(), original_len - 1);
        assert_eq!(right.len(), original_len + 2);
    }

    proptest! {
        /// PackedList must behave EXACTLY like a VecDeque<Vec<u8>> under an
        /// arbitrary op stream: same elements in the same order after every
        /// push/pop (both ends), get, set, insert, remove, and retain. This is
        /// the isomorphism the eventual Value::List wiring relies on.
        #[test]
        fn list_equivalent_to_vecdeque(ops in proptest::collection::vec(
            (0u8..7, proptest::collection::vec(0u8..4, 0..4), any::<u8>()), 0..300)) {
            let mut packed = PackedList::new();
            let mut oracle: VecDeque<Vec<u8>> = VecDeque::new();
            for (op, elem, raw_idx) in ops {
                let n = oracle.len();
                let idx = if n == 0 { 0 } else { raw_idx as usize % n };
                match op {
                    0 => { packed.push_back(&elem); oracle.push_back(elem.clone()); }
                    1 => { packed.push_front(&elem); oracle.push_front(elem.clone()); }
                    2 => { prop_assert_eq!(packed.pop_back(), oracle.pop_back()); }
                    3 => { prop_assert_eq!(packed.pop_front(), oracle.pop_front()); }
                    4 => {
                        if n > 0 {
                            prop_assert_eq!(packed.set(idx, &elem), true);
                            oracle[idx] = elem.clone();
                        }
                    }
                    5 => {
                        let ins = if n == 0 { 0 } else { raw_idx as usize % (n + 1) };
                        packed.insert(ins, &elem);
                        oracle.insert(ins, elem.clone());
                    }
                    _ => {
                        if n > 0 {
                            prop_assert_eq!(packed.remove(idx), Some(oracle.remove(idx).unwrap()));
                        }
                    }
                }
                prop_assert_eq!(packed.len(), oracle.len());
                let p: Vec<&[u8]> = packed.iter().collect();
                let o: Vec<&[u8]> = oracle.iter().map(|v| v.as_slice()).collect();
                prop_assert_eq!(p, o);
            }
        }

        /// ListValue's promoted quicklist representation must remain a
        /// front-to-back VecDeque isomorphism after the chunked COW change.
        #[test]
        fn list_value_deque_equivalent_to_vecdeque_after_promotion(ops in proptest::collection::vec(
            (0u8..8, proptest::collection::vec(0u8..4, 0..4), any::<u8>()), 0..240)) {
            let mut list = ListValue::default();
            let mut oracle: VecDeque<Vec<u8>> = VecDeque::new();
            for idx in 0..=PACKED_MAX_ENTRIES {
                let elem = list_test_value(idx);
                list.push_back(elem.clone());
                oracle.push_back(elem);
            }
            prop_assert!(matches!(list.repr, ListRepr::Deque(_)));

            for (op, elem, raw_idx) in ops {
                let n = oracle.len();
                let idx = if n == 0 { 0 } else { raw_idx as usize % n };
                match op {
                    0 => { list.push_back(elem.clone()); oracle.push_back(elem.clone()); }
                    1 => { list.push_front(elem.clone()); oracle.push_front(elem.clone()); }
                    2 => { prop_assert_eq!(list.pop_back(), oracle.pop_back()); }
                    3 => { prop_assert_eq!(list.pop_front(), oracle.pop_front()); }
                    4 => {
                        if n > 0 {
                            prop_assert!(list.set(idx, elem.clone()));
                            oracle[idx] = elem.clone();
                        }
                    }
                    5 => {
                        let ins = if n == 0 { 0 } else { raw_idx as usize % (n + 1) };
                        list.insert(ins, elem.clone());
                        oracle.insert(ins, elem.clone());
                    }
                    6 => {
                        if n > 0 {
                            prop_assert_eq!(list.remove(idx), oracle.remove(idx));
                        }
                    }
                    _ => {
                        let keep_parity = raw_idx & 1;
                        list.retain(|value| {
                            value.first().copied().unwrap_or_default() & 1 == keep_parity
                        });
                        oracle.retain(|value| {
                            value.first().copied().unwrap_or_default() & 1 == keep_parity
                        });
                    }
                }
                prop_assert_eq!(list.len(), oracle.len());
                for check_idx in [0, idx, oracle.len().saturating_sub(1)] {
                    prop_assert_eq!(
                        list.get(check_idx),
                        oracle.get(check_idx).map(Vec::as_slice)
                    );
                }
                let got: Vec<&[u8]> = list.iter().collect();
                let want: Vec<&[u8]> = oracle.iter().map(Vec::as_slice).collect();
                prop_assert_eq!(got, want);
            }
        }
    }

    #[test]
    fn zset_basic_order_score_rank() {
        let mut z = PackedZSet::new();
        assert!(z.insert(b"b", 2.0));
        assert!(z.insert(b"a", 1.0));
        assert!(z.insert(b"c", 2.0)); // tie with b -> ordered by member
        assert!(!z.insert(b"b", 0.5)); // update score, not new; repositions to front
        let pairs: Vec<(&[u8], f64)> = z.iter().collect();
        assert_eq!(pairs, vec![(&b"b"[..], 0.5), (b"a", 1.0), (b"c", 2.0)]);
        assert_eq!(z.get_score(b"a"), Some(1.0));
        assert_eq!(z.get_score(b"zzz"), None);
        assert_eq!(z.rank(b"b"), Some(0));
        assert_eq!(z.rank(b"c"), Some(2));
        assert_eq!(z.rank(b"zzz"), None);
        for member in [b"b".as_slice(), b"a", b"c", b"zzz"] {
            assert_eq!(
                z.rank(member),
                z.rank_impl::<false>(member),
                "member-only rank diverged from score-decoding reference"
            );
            assert_eq!(
                z.rank_with_score(member),
                z.rank_with_score_impl::<false>(member),
                "member-only rank-with-score diverged from score-decoding reference"
            );
        }
        assert!(z.remove(b"a"));
        assert!(!z.remove(b"a"));
        assert_eq!(z.len(), 2);
        // +0.0 and -0.0 are the same score (member tiebreak only).
        let mut z2 = PackedZSet::new();
        z2.insert(b"y", -0.0);
        z2.insert(b"x", 0.0);
        assert_eq!(
            z2.iter().collect::<Vec<_>>(),
            vec![(&b"x"[..], 0.0), (b"y", -0.0)]
        );
    }

    #[test]
    fn packed_zset_score_range_early_break_matches_total_order_reference() {
        let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
        let positive_nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let zset = PackedZSet::from_unique_pairs(vec![
            (b"negative-nan".to_vec(), negative_nan),
            (b"negative-infinity".to_vec(), f64::NEG_INFINITY),
            (b"negative-zero".to_vec(), -0.0),
            (b"positive-zero".to_vec(), 0.0),
            (b"hi-a".to_vec(), 15.0),
            (b"hi-b".to_vec(), 15.0),
            (b"positive-infinity".to_vec(), f64::INFINITY),
            (b"positive-nan".to_vec(), positive_nan),
        ]);

        for (lo, hi) in [
            (negative_nan, positive_nan),
            (f64::NEG_INFINITY, f64::INFINITY),
            (-0.0, 0.0),
            (15.0, 15.0),
            (f64::INFINITY, f64::INFINITY),
            (1.0, -1.0),
            (negative_nan, negative_nan),
            (positive_nan, positive_nan),
        ] {
            let mut candidate = Vec::new();
            zset.for_each_in_score_range(lo, hi, |member, score| {
                candidate.push((member.to_vec(), score.to_bits()));
            });
            let mut reference = Vec::new();
            zset.for_each_in_score_range_impl::<false>(lo, hi, |member, score| {
                reference.push((member.to_vec(), score.to_bits()));
            });
            assert_eq!(candidate, reference, "range [{lo:?}, {hi:?}] diverged");
        }
    }

    #[test]
    fn packed_zset_pop_max_member_only_matches_score_decoding_reference() {
        let pairs: Vec<(Vec<u8>, f64)> = (0_i32..120)
            .map(|index| {
                (
                    format!("member:{index:04}").into_bytes(),
                    f64::from((index % 17) - 8) + f64::from(index % 3) * 0.25,
                )
            })
            .collect();
        let mut candidate = PackedZSet::from_unique_pairs(pairs.clone());
        let mut reference = PackedZSet::from_unique_pairs(pairs);
        loop {
            let candidate_result = candidate.pop_max();
            let reference_result = reference.pop_max_impl::<false>();
            assert_eq!(candidate_result, reference_result);
            assert_eq!(candidate.len(), reference.len());
            assert_eq!(
                candidate.iter().collect::<Vec<_>>(),
                reference.iter().collect::<Vec<_>>()
            );
            if candidate_result.is_none() {
                break;
            }
        }
    }

    proptest! {
        /// PackedZSet must keep `(score, member)` sorted order and match ZADD/
        /// ZREM/ZSCORE/ZRANK against a reference unique-member set sorted by the
        /// SAME comparator (ScoreMember's order). The isomorphism the SortedSet
        /// wiring relies on.
        #[test]
        fn zset_equivalent_to_sorted_reference(ops in proptest::collection::vec(
            (0u8..3, proptest::collection::vec(0u8..3, 0..3), -3i8..4), 0..300)) {
            let mut packed = PackedZSet::new();
            let mut oracle: Vec<(Vec<u8>, f64)> = Vec::new();
            for (op, member, raw_score) in ops {
                let score = f64::from(raw_score);
                match op {
                    0 => {
                        let was_new = !oracle.iter().any(|(m, _)| m == &member);
                        if let Some(e) = oracle.iter_mut().find(|(m, _)| m == &member) {
                            e.1 = score;
                        } else {
                            oracle.push((member.clone(), score));
                        }
                        prop_assert_eq!(packed.insert(&member, score), was_new);
                    }
                    1 => {
                        let existed = oracle.iter().any(|(m, _)| m == &member);
                        oracle.retain(|(m, _)| m != &member);
                        prop_assert_eq!(packed.remove(&member), existed);
                    }
                    _ => {
                        let os = oracle.iter().find(|(m, _)| m == &member).map(|(_, s)| *s);
                        prop_assert_eq!(packed.get_score(&member), os);
                    }
                }
                prop_assert_eq!(packed.len(), oracle.len());
                let mut sorted = oracle.clone();
                sorted.sort_by(|a, b| zset_cmp(a.1, &a.0, b.1, &b.0));
                let got: Vec<(Vec<u8>, f64)> =
                    packed.iter().map(|(m, s)| (m.to_vec(), s)).collect();
                prop_assert_eq!(&got, &sorted);
                // rank == index in the sorted reference
                for (i, (m, _)) in sorted.iter().enumerate() {
                    prop_assert_eq!(packed.rank(m), Some(i));
                    prop_assert_eq!(packed.rank(m), packed.rank_impl::<false>(m));
                    prop_assert_eq!(
                        packed.rank_with_score(m),
                        packed.rank_with_score_impl::<false>(m)
                    );
                }
                prop_assert_eq!(packed.rank(b"missing"), packed.rank_impl::<false>(b"missing"));
                prop_assert_eq!(
                    packed.rank_with_score(b"missing"),
                    packed.rank_with_score_impl::<false>(b"missing")
                );
                // iter_desc == reversed sorted
                let desc: Vec<(Vec<u8>, f64)> =
                    packed.iter_desc().map(|(m, s)| (m.to_vec(), s)).collect();
                let mut sorted_rev = sorted.clone();
                sorted_rev.reverse();
                prop_assert_eq!(&desc, &sorted_rev);
                // index_slice_asc / _desc == sorted/reversed skip+take
                for (start, count) in [(0usize, 2usize), (1, 3), (0, 100), (5, 1)] {
                    let asc_want: Vec<(Vec<u8>, f64)> =
                        sorted.iter().skip(start).take(count).cloned().collect();
                    prop_assert_eq!(packed.index_slice_asc(start, count), asc_want);
                    let desc_want: Vec<(Vec<u8>, f64)> =
                        sorted_rev.iter().skip(start).take(count).cloned().collect();
                    prop_assert_eq!(packed.index_slice_desc(start, count), desc_want);
                    prop_assert_eq!(
                        packed.index_slice_desc(start, count),
                        packed.index_slice_desc_impl::<false>(start, count)
                    );
                }
                // for_each_in_score_range == sorted filtered to [lo, hi]
                for (lo, hi) in [
                    (f64::NEG_INFINITY, f64::INFINITY),
                    (-2.0, 2.0),
                    (-0.0, 0.0),
                    (1.0, 3.0),
                    (3.0, 1.0),
                ] {
                    let mut got_range: Vec<(Vec<u8>, f64)> = Vec::new();
                    packed.for_each_in_score_range(lo, hi, |m, s| got_range.push((m.to_vec(), s)));
                    let mut old_range: Vec<(Vec<u8>, f64)> = Vec::new();
                    packed.for_each_in_score_range_impl::<false>(lo, hi, |m, s| {
                        old_range.push((m.to_vec(), s));
                    });
                    let want_range: Vec<(Vec<u8>, f64)> = sorted
                        .iter()
                        .filter(|(_, s)| *s >= lo && *s <= hi)
                        .cloned()
                        .collect();
                    prop_assert_eq!(&got_range, &old_range);
                    prop_assert_eq!(&got_range, &want_range);
                }
            }
        }

        /// pop_min/pop_max drain the ends in sorted order (ZPOPMIN/ZPOPMAX).
        #[test]
        fn zset_pop_min_max(members in proptest::collection::vec(
            (proptest::collection::vec(0u8..4, 1..3), -3i8..4), 0..20)) {
            let mut packed = PackedZSet::new();
            let mut oracle: Vec<(Vec<u8>, f64)> = Vec::new();
            for (m, raw) in members {
                let s = f64::from(raw);
                if let Some(e) = oracle.iter_mut().find(|(om, _)| om == &m) {
                    e.1 = s;
                } else {
                    oracle.push((m.clone(), s));
                }
                packed.insert(&m, s);
            }
            oracle.sort_by(|a, b| zset_cmp(a.1, &a.0, b.1, &b.0));
            // pop from both ends, alternating, comparing to the reference deque.
            let mut deque: std::collections::VecDeque<(Vec<u8>, f64)> = oracle.into();
            let mut take_min = true;
            while !deque.is_empty() {
                if take_min {
                    prop_assert_eq!(packed.pop_min(), deque.pop_front());
                } else {
                    prop_assert_eq!(packed.pop_max(), deque.pop_back());
                }
                take_min = !take_min;
            }
            prop_assert_eq!(packed.pop_min(), None);
            prop_assert_eq!(packed.pop_max(), None);
            prop_assert_eq!(packed.len(), 0);
        }
    }

    #[test]
    fn packed_zset_desc_slice_matches_full_materialization_reference() {
        let negative_nan = f64::from_bits(0xfff8_0000_0000_0001);
        let positive_nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let zset = PackedZSet::from_unique_pairs(vec![
            (b"negative-nan".to_vec(), negative_nan),
            (b"negative-infinity".to_vec(), f64::NEG_INFINITY),
            (b"negative-zero".to_vec(), -0.0),
            (b"positive-zero".to_vec(), 0.0),
            (b"tie-a".to_vec(), 15.0),
            (b"tie-b".to_vec(), 15.0),
            (b"positive-infinity".to_vec(), f64::INFINITY),
            (b"positive-nan".to_vec(), positive_nan),
        ]);

        for (start, count) in [
            (0, 0),
            (0, 1),
            (0, usize::MAX),
            (1, 3),
            (zset.len() - 1, 2),
            (zset.len(), 1),
            (zset.len() + 1, 1),
            (usize::MAX, usize::MAX),
        ] {
            let candidate: Vec<_> = zset
                .index_slice_desc(start, count)
                .into_iter()
                .map(|(member, score)| (member, score.to_bits()))
                .collect();
            let reference: Vec<_> = zset
                .index_slice_desc_impl::<false>(start, count)
                .into_iter()
                .map(|(member, score)| (member, score.to_bits()))
                .collect();
            assert_eq!(candidate, reference, "slice ({start}, {count}) diverged");
        }
    }

    #[test]
    fn map_insert_update_keeps_position() {
        let mut m = PackedStrMap::new();
        assert_eq!(m.insert(b"a".to_vec(), b"1".to_vec()), None);
        assert_eq!(m.insert(b"b".to_vec(), b"2".to_vec()), None);
        assert_eq!(m.insert(b"c".to_vec(), b"3".to_vec()), None);
        // updating an existing field keeps its position, returns the old value,
        // and handles a value-length change (1 -> 5 bytes).
        assert_eq!(
            m.insert(b"b".to_vec(), b"22222".to_vec()),
            Some(b"2".to_vec())
        );
        let pairs: Vec<(&[u8], &[u8])> = m.iter().collect();
        assert_eq!(
            pairs,
            vec![(&b"a"[..], &b"1"[..]), (b"b", b"22222"), (b"c", b"3")]
        );
        assert_eq!(m.get(b"b"), Some(&b"22222"[..]));
        assert_eq!(m.shift_remove(b"a"), Some(b"1".to_vec()));
        assert_eq!(m.get_index(0), Some((&b"b"[..], &b"22222"[..])));
        assert_eq!(m.len(), 2);
    }

    // (frankenredis-vizeb) ChunkedList::locate now walks from the nearer end.
    // It MUST return exactly the same (chunk_idx, local_idx) as a front-only walk
    // for every index, and ListValue::get must return the right element from both
    // halves. A/B: deep-tail locate goes O(num_chunks) -> O(1).
    #[test]
    fn chunked_list_locate_nearer_end_isomorphic_and_faster_listidx() {
        // Front-only reference == the pre-listidx implementation.
        fn front_only_locate(cl: &ChunkedList, idx: usize) -> Option<(usize, usize)> {
            if idx >= cl.len {
                return None;
            }
            let mut base = 0usize;
            for (chunk_idx, chunk) in cl.chunks.iter().enumerate() {
                let next = base + chunk.len();
                if idx < next {
                    return Some((chunk_idx, idx - base));
                }
                base = next;
            }
            None
        }

        // Several lengths straddling chunk boundaries (LIST_CHUNK_TARGET=128).
        for &n in &[1usize, 127, 128, 129, 1000, 4096] {
            let d: VecDeque<Vec<u8>> = (0..n).map(|i| format!("e{i}").into_bytes()).collect();
            let cl = ChunkedList::from(d);
            assert_eq!(cl.len(), n);
            for idx in 0..n {
                assert_eq!(
                    cl.locate(idx),
                    front_only_locate(&cl, idx),
                    "n={n} idx={idx}: nearer-end locate diverged from front-only"
                );
                // And the located element is the right one.
                assert_eq!(cl.get(idx), Some(format!("e{idx}").into_bytes().as_slice()));
            }
            assert_eq!(cl.locate(n), None);
        }

        // A/B: deep-tail locate. Old front-walk is O(num_chunks); new is O(1).
        let n = 400_000usize;
        let d: VecDeque<Vec<u8>> = (0..n).map(|i| format!("e{i}").into_bytes()).collect();
        let cl = ChunkedList::from(d);
        let tail = n - 1;
        let reps = 200_000usize;

        let mut acc = 0usize;
        let t0 = std::time::Instant::now();
        for _ in 0..reps {
            acc += front_only_locate(std::hint::black_box(&cl), std::hint::black_box(tail))
                .map_or(0, |(c, _)| c);
        }
        let old_ns = t0.elapsed().as_nanos().max(1);
        std::hint::black_box(acc);

        let mut acc2 = 0usize;
        let t1 = std::time::Instant::now();
        for _ in 0..reps {
            acc2 += cl.locate(std::hint::black_box(tail)).map_or(0, |(c, _)| c);
        }
        let new_ns = t1.elapsed().as_nanos().max(1);
        std::hint::black_box(acc2);

        let chunks = n.div_ceil(LIST_CHUNK_TARGET);
        println!(
            "ChunkedList tail-locate A/B (n={n}, {chunks} chunks, x{reps}): front-walk={old_ns}ns nearer-end={new_ns}ns ratio={:.1}x",
            old_ns as f64 / new_ns as f64
        );
        assert!(
            (old_ns as f64 / new_ns as f64) > 2.0 || cfg!(debug_assertions),
            "expected >2x, got {:.1}x",
            old_ns as f64 / new_ns as f64
        );
    }

    // (frankenredis-c92f6) `from_restored_quicklist2_nodes` now folds the
    // growth-state totals during construction instead of calling
    // `rebuild_growth_state`, which re-walked every restored element. Pin the
    // equivalence: building the value and THEN running the old fold must not
    // change `lp_bytes` / `forced_quicklist` (nor `len`), for every node shape.
    //
    // The non-canonical case is the load-bearing one: a listpack whose entries
    // are STRING-encoded even though their bytes parse as integers. Deriving
    // `enc_total` from the listpack header's `total_bytes` would report the
    // on-wire size and silently change OBJECT ENCODING; summing
    // `list_lp_entry_bytes` per element (as both the fold and the fused pass do)
    // reports the canonical re-encoded size. These must stay equal.
    fn string_encoded_listpack(entries: &[&[u8]]) -> Vec<u8> {
        let mut encoded = Vec::new();
        for entry in entries {
            let start = encoded.len();
            assert!(entry.len() < 64, "test helper only emits 6-bit-len strings");
            encoded.push(0x80 | entry.len() as u8);
            encoded.extend_from_slice(entry);
            let data_len = encoded.len() - start;
            crate::encode_listpack_backlen(&mut encoded, data_len);
        }
        crate::finish_listpack_entries(encoded, entries.len()).expect("listpack fits")
    }

    fn listpack_node(bytes: Vec<u8>) -> super::RestoredListNode {
        let entries =
            fr_persist::listpack::decode_value_spans(&bytes).expect("test listpack must decode");
        super::RestoredListNode::Listpack { bytes, entries }
    }

    fn assert_fused_totals_match_rewalk(
        label: &str,
        make: impl Fn() -> Vec<super::RestoredListNode>,
        expect_len: usize,
    ) {
        let mut fused = ListValue::from_restored_quicklist2_nodes(make());
        let (lp_bytes, forced, decided) = (
            fused.lp_bytes,
            fused.forced_quicklist,
            fused.decided_by_write,
        );
        assert_eq!(fused.len(), expect_len, "{label}: element count");

        // Re-run the exact fold the old code performed.
        fused.rebuild_growth_state();
        assert_eq!(fused.lp_bytes, lp_bytes, "{label}: lp_bytes drifted");
        // multi-node payloads pin forced_quicklist AFTER the fold, so compare the
        // fold's own verdict only where the constructor did not override it.
        if !decided {
            assert_eq!(
                fused.forced_quicklist, forced,
                "{label}: forced_quicklist drifted"
            );
        }
    }

    #[test]
    fn restored_quicklist2_fused_growth_totals_match_rebuild_walk_c92f6() {
        // canonical: mixed strings + integer-encoded entries
        let canonical: Vec<&[u8]> = vec![b"member:0001", b"42", b"-9999", b"x"];
        let canonical_lp = crate::encode_listpack_strings(&canonical).expect("lp");
        assert_fused_totals_match_rewalk(
            "canonical single node",
            || vec![listpack_node(canonical_lp.clone())],
            canonical.len(),
        );

        // NON-CANONICAL: int-looking values stored as listpack STRINGS.
        let noncanon: Vec<&[u8]> =
            vec![b"123", b"4096", b"-1", b"0", b"00", b"9223372036854775807"];
        let noncanon_lp = string_encoded_listpack(&noncanon);
        assert_fused_totals_match_rewalk(
            "non-canonical string-encoded ints",
            || vec![listpack_node(noncanon_lp.clone())],
            noncanon.len(),
        );

        // plain nodes only (large elements)
        assert_fused_totals_match_rewalk(
            "plain nodes",
            || {
                vec![
                    super::RestoredListNode::Plain(vec![b'a'; 100]),
                    super::RestoredListNode::Plain(b"77".to_vec()),
                ]
            },
            2,
        );

        // mixed multi-node: plain + listpack interleaved (also exercises the
        // plain-chunk flush between listpack nodes)
        assert_fused_totals_match_rewalk(
            "mixed multi-node",
            || {
                vec![
                    super::RestoredListNode::Plain(vec![b'z'; 70]),
                    listpack_node(canonical_lp.clone()),
                    super::RestoredListNode::Plain(b"5".to_vec()),
                    listpack_node(noncanon_lp.clone()),
                ]
            },
            2 + canonical.len() + noncanon.len(),
        );

        // budget boundary: enough raw bytes to trip forced_quicklist
        assert_fused_totals_match_rewalk(
            "over LIST_DEFAULT_BUDGET",
            || {
                (0..200)
                    .map(|i| super::RestoredListNode::Plain(vec![b'q'; 50 + (i % 3)]))
                    .collect()
            },
            200,
        );
    }

    #[test]
    fn pop_front_n_matches_pop_front_loop_cc() {
        // (cc_fr) ListValue::pop_front_n(count) MUST be byte-identical (returned values in pop
        // order, residual contents, len, lp_bytes) to calling pop_front() count times — across the
        // Packed repr (the O(n)-drain fast path) and the Deque repr (>128 elems, per-pop loop), for
        // count < / == / > len, incl. emptying the list.
        for &(total, popn) in &[
            (0usize, 3usize),
            (1, 1),
            (5, 3),
            (10, 10),
            (10, 25),
            (128, 64),
            (128, 128),
            (200, 50),
            (200, 200),
        ] {
            let build = || {
                let mut l = ListValue::default();
                for i in 0..total {
                    l.push_back(format!("elem:{i:05}").into_bytes());
                }
                l
            };
            let mut a = build();
            let mut b = build();
            let mut want = Vec::new();
            for _ in 0..popn {
                match b.pop_front() {
                    Some(v) => want.push(v),
                    None => break,
                }
            }
            let got = a.pop_front_n(popn);
            assert_eq!(got, want, "returned @ total={total} popn={popn}");
            assert_eq!(a.len(), b.len(), "len @ total={total} popn={popn}");
            assert_eq!(
                a.iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                b.iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                "residual @ total={total} popn={popn}"
            );
            assert_eq!(
                a.listpack_byte_len(),
                b.listpack_byte_len(),
                "lp_bytes @ total={total} popn={popn}"
            );
        }
    }

    #[test]
    fn pop_back_n_matches_pop_back_loop_cc() {
        // (cc_fr) ListValue::pop_back_n(count) MUST be byte-identical (returned values in pop order
        // = LAST element first, residual, len, lp_bytes) to calling pop_back() count times — across
        // Packed (O(len) scan+truncate) and Deque (per-pop loop), count < / == / > len incl. empty.
        for &(total, popn) in &[
            (0usize, 3usize),
            (1, 1),
            (5, 3),
            (10, 10),
            (10, 25),
            (128, 64),
            (128, 128),
            (200, 50),
            (200, 200),
        ] {
            let build = || {
                let mut l = ListValue::default();
                for i in 0..total {
                    l.push_back(format!("elem:{i:05}").into_bytes());
                }
                l
            };
            let mut a = build();
            let mut b = build();
            let mut want = Vec::new();
            for _ in 0..popn {
                match b.pop_back() {
                    Some(v) => want.push(v),
                    None => break,
                }
            }
            let got = a.pop_back_n(popn);
            assert_eq!(got, want, "returned @ total={total} popn={popn}");
            assert_eq!(a.len(), b.len(), "len @ total={total} popn={popn}");
            assert_eq!(
                a.iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                b.iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                "residual @ total={total} popn={popn}"
            );
            assert_eq!(
                a.listpack_byte_len(),
                b.listpack_byte_len(),
                "lp_bytes @ total={total} popn={popn}"
            );
        }
    }
}
