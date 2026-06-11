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

use indexmap::{IndexMap, IndexSet};

/// Hashtable storage for a large generic set (the former `GenericSet` alias).
pub type SetHashTable = IndexSet<Vec<u8>, foldhash::quality::RandomState>;

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
    Hash(SetHashTable),
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
            GenericSet::Hash(IndexSet::with_capacity_and_hasher(
                n,
                foldhash::quality::RandomState::default(),
            ))
        } else {
            GenericSet::Packed(PackedStrSet::with_capacity(n.saturating_mul(8)))
        }
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
            GenericSet::Hash(h) => h.get_index(idx).map(|v| v.as_slice()),
        }
    }

    fn promote(&mut self) {
        if let GenericSet::Packed(p) = self {
            let mut h: SetHashTable = IndexSet::with_capacity_and_hasher(
                p.len() + 1,
                foldhash::quality::RandomState::default(),
            );
            for m in p.iter() {
                h.insert(m.to_vec());
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
            GenericSet::Hash(h) => h.insert(member),
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
            GenericSet::Hash(h) => h.retain(|m| keep(m)),
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
            GenericSet::Hash(h) => h.into_iter().collect(),
        };
        owned.into_iter()
    }
}

/// Borrowing iterator over a `GenericSet`'s members in insertion order.
pub enum GenericSetIter<'a> {
    Packed(PackedStrSetIter<'a>),
    Hash(indexmap::set::Iter<'a, Vec<u8>>),
}

impl<'a> Iterator for GenericSetIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            GenericSetIter::Packed(it) => it.next(),
            GenericSetIter::Hash(it) => it.next().map(|v| v.as_slice()),
        }
    }
}

/// Hashtable storage for a large hash (the former `HashFieldMap` alias).
pub type FieldHashTable = IndexMap<Vec<u8>, Vec<u8>, foldhash::quality::RandomState>;

/// Storage for a hash's field→value map: a packed listpack-style buffer while
/// small, promoting to an `IndexMap` hashtable past the threshold. Drop-in for
/// the former `IndexMap` alias — same insertion-ordered iteration and identical
/// get/insert/contains/remove semantics, so HGETALL/HKEYS/HVALS/HSCAN output is
/// byte-for-byte unchanged. (frankenredis-9mh3o step 3)
#[derive(Clone, Debug)]
pub enum HashFieldMap {
    Packed(PackedStrMap),
    Hash(FieldHashTable),
}

impl Default for HashFieldMap {
    fn default() -> Self {
        HashFieldMap::Packed(PackedStrMap::new())
    }
}

impl HashFieldMap {
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
            HashFieldMap::Hash(h) => h.get(field).map(|v| v.as_slice()),
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
            HashFieldMap::Hash(h) => h.get_index(idx).map(|(k, v)| (k.as_slice(), v.as_slice())),
        }
    }

    fn promote(&mut self) {
        if let HashFieldMap::Packed(p) = self {
            let mut h: FieldHashTable = IndexMap::with_capacity_and_hasher(
                p.len() + 1,
                foldhash::quality::RandomState::default(),
            );
            for (k, v) in p.iter() {
                h.insert(k.to_vec(), v.to_vec());
            }
            *self = HashFieldMap::Hash(h);
        }
    }

    /// Insert/overwrite, returning the previous value (matches `IndexMap::insert`).
    pub fn insert(&mut self, field: Vec<u8>, value: Vec<u8>) -> Option<Vec<u8>> {
        if let HashFieldMap::Packed(p) = self
            && !p.contains_key(&field)
            && (p.len() >= PACKED_MAX_ENTRIES
                || field.len() > PACKED_MAX_VALUE
                || value.len() > PACKED_MAX_VALUE)
        {
            self.promote();
        }
        match self {
            HashFieldMap::Packed(p) => p.insert(field, value),
            HashFieldMap::Hash(h) => h.insert(field, value),
        }
    }

    pub fn shift_remove(&mut self, field: &[u8]) -> Option<Vec<u8>> {
        match self {
            HashFieldMap::Packed(p) => p.shift_remove(field),
            HashFieldMap::Hash(h) => h.shift_remove(field),
        }
    }

    #[must_use]
    pub fn iter(&self) -> HashFieldMapIter<'_> {
        match self {
            HashFieldMap::Packed(p) => HashFieldMapIter::Packed(p.iter()),
            HashFieldMap::Hash(h) => HashFieldMapIter::Hash(h.iter()),
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.iter().map(|(k, _)| k)
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

/// Borrowing iterator over a `HashFieldMap`'s (field, value) pairs.
pub enum HashFieldMapIter<'a> {
    Packed(PackedStrMapIter<'a>),
    Hash(indexmap::map::Iter<'a, Vec<u8>, Vec<u8>>),
}

impl<'a> Iterator for HashFieldMapIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            HashFieldMapIter::Packed(it) => it.next(),
            HashFieldMapIter::Hash(it) => it.next().map(|(k, v)| (k.as_slice(), v.as_slice())),
        }
    }
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

use std::collections::VecDeque;

/// Storage for a list: a packed buffer while small, promoting to a
/// `VecDeque<Vec<u8>>` (which keeps O(1) ends for large lists, redis's quicklist
/// regime) past the threshold. Drop-in for the former `VecDeque` — same
/// front-to-back order and identical push/pop/get/insert/remove/retain
/// semantics, so LRANGE/LINDEX/LPOP/etc. output is byte-for-byte unchanged.
/// (frankenredis-9mh3o step 4)
#[derive(Clone, Debug)]
enum ListRepr {
    Packed(PackedList),
    Deque(VecDeque<Vec<u8>>),
}

impl Default for ListRepr {
    fn default() -> Self {
        ListRepr::Packed(PackedList::new())
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
    let value: i64 = std::str::from_utf8(entry).ok()?.parse().ok()?;
    if value.to_string().as_bytes() == entry {
        Some(value)
    } else {
        None
    }
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
}

impl Default for ListValue {
    fn default() -> Self {
        ListValue {
            repr: ListRepr::default(),
            lp_bytes: LIST_LP_OVERHEAD,
            forced_quicklist: false,
            fill: -2,
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
            ListRepr::Deque(d) => d.get(idx).map(|v| v.as_slice()),
        }
    }

    fn promote(&mut self) {
        if let ListRepr::Packed(p) = &self.repr {
            let mut d: VecDeque<Vec<u8>> = VecDeque::with_capacity(p.len() + 1);
            for e in p.iter() {
                d.push_back(e.to_vec());
            }
            self.repr = ListRepr::Deque(d);
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
            ListRepr::Deque(d) => d.push_back(elem),
        }
    }

    pub fn push_front(&mut self, elem: Vec<u8>) {
        self.add_entry_bytes(&elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.push_front(&elem),
            ListRepr::Deque(d) => d.push_front(elem),
        }
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.pop_front(),
            ListRepr::Deque(d) => d.pop_front(),
        };
        if let Some(ref r) = removed {
            self.on_remove_one(r);
        }
        removed
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.pop_back(),
            ListRepr::Deque(d) => d.pop_back(),
        };
        if let Some(ref r) = removed {
            self.on_remove_one(r);
        }
        removed
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
            ListRepr::Deque(d) => {
                if let Some(slot) = d.get_mut(idx) {
                    *slot = elem;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Insert before index `idx` (`idx >= len` appends), matching `VecDeque::insert`.
    /// The caller (LINSERT) makes the conversion decision via `note_command_grow`.
    pub fn insert(&mut self, idx: usize, elem: Vec<u8>) {
        self.add_entry_bytes(&elem);
        self.maybe_promote(elem.len());
        match &mut self.repr {
            ListRepr::Packed(p) => p.insert(idx, &elem),
            ListRepr::Deque(d) => d.insert(idx, elem),
        }
    }

    pub fn remove(&mut self, idx: usize) -> Option<Vec<u8>> {
        let removed = match &mut self.repr {
            ListRepr::Packed(p) => p.remove(idx),
            ListRepr::Deque(d) => d.remove(idx),
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
            ListRepr::Deque(d) => d.retain(|v| keep(v)),
        }
        if self.len() != before {
            self.on_remove_bulk();
        }
    }

    pub fn clear(&mut self) {
        *self = ListValue::default();
    }

    #[must_use]
    pub fn iter(&self) -> ListValueIter<'_> {
        match &self.repr {
            ListRepr::Packed(p) => ListValueIter::Packed(p.iter()),
            ListRepr::Deque(d) => ListValueIter::Deque(d.iter()),
        }
    }
}

impl From<VecDeque<Vec<u8>>> for ListValue {
    fn from(d: VecDeque<Vec<u8>>) -> Self {
        let repr = if d.len() > PACKED_MAX_ENTRIES || d.iter().any(|e| e.len() > PACKED_MAX_VALUE) {
            ListRepr::Deque(d)
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
    Deque(std::collections::vec_deque::Iter<'a, Vec<u8>>),
}

impl<'a> Iterator for ListValueIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        match self {
            ListValueIter::Packed(it) => it.next(),
            ListValueIter::Deque(it) => it.next().map(|v| v.as_slice()),
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

    /// Decode the record starting at `pos`: `(member, score, record_end)`.
    fn record_at(&self, pos: usize) -> (&[u8], f64, usize) {
        let (mlen, m_start) = read_varint(&self.buf, pos);
        let m_end = m_start + mlen;
        let mut score_bytes = [0; 8];
        score_bytes.copy_from_slice(&self.buf[m_end..m_end + 8]);
        let score = f64::from_le_bytes(score_bytes);
        (&self.buf[m_start..m_end], score, m_end + 8)
    }

    /// `(record_start, record_end, score)` for `member`, or None.
    fn locate(&self, member: &[u8]) -> Option<(usize, usize, f64)> {
        let mut pos = 0;
        while pos < self.buf.len() {
            let (m, score, end) = self.record_at(pos);
            if m == member {
                return Some((pos, end, score));
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
        let existed = if let Some((rs, re, _)) = self.locate(member) {
            self.buf.drain(rs..re);
            self.len -= 1;
            true
        } else {
            false
        };
        let off = self.insert_offset(member, score);
        self.buf.splice(off..off, Self::encode(member, score));
        self.len += 1;
        !existed
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

    /// 0-based rank of `member` in ascending `(score, member)` order (ZRANK).
    #[must_use]
    pub fn rank(&self, member: &[u8]) -> Option<usize> {
        let mut pos = 0;
        let mut idx = 0;
        while pos < self.buf.len() {
            let (m, _s, end) = self.record_at(pos);
            if m == member {
                return Some(idx);
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
        self.iter_desc()
            .skip(start_idx)
            .take(count)
            .map(|(m, s)| (m.to_vec(), s))
            .collect()
    }

    /// Invoke `f(member, score)` for each member whose canonical score lies in
    /// the INCLUSIVE range `[lo, hi]`, ascending (mirrors
    /// SortedSet::for_each_in_score_range, which ranges
    /// `min_for_score(lo)..=max_for_score(hi)`).
    pub fn for_each_in_score_range(&self, lo: f64, hi: f64, mut f: impl FnMut(&[u8], f64)) {
        let (lo, hi) = (canon_zero(lo), canon_zero(hi));
        for (member, score) in self.iter() {
            let c = canon_zero(score);
            if c.total_cmp(&lo) != std::cmp::Ordering::Less
                && c.total_cmp(&hi) != std::cmp::Ordering::Greater
            {
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
        if self.buf.is_empty() {
            return None;
        }
        // Walk to the last record's start.
        let mut pos = 0;
        let mut last_start = 0;
        while pos < self.buf.len() {
            last_start = pos;
            let (_m, _s, end) = self.record_at(pos);
            pos = end;
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

#[cfg(test)]
mod tests {
    use super::{PackedList, PackedStrMap, PackedStrSet, PackedZSet, zset_cmp};
    use indexmap::{IndexMap, IndexSet};
    use proptest::prelude::*;
    use std::collections::VecDeque;

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
            (0u8..3, proptest::collection::vec(0u8..3, 0..4), proptest::collection::vec(0u8..9, 0..4)),
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
                    _ => {
                        prop_assert_eq!(packed.get(&field), oracle.get(&field[..]).map(|v| v.as_slice()));
                        prop_assert_eq!(packed.contains_key(&field), oracle.contains_key(&field[..]));
                    }
                }
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
                }
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
                }
                // for_each_in_score_range == sorted filtered to [lo, hi]
                for (lo, hi) in [(-2.0, 2.0), (0.0, 0.0), (1.0, 3.0)] {
                    let mut got_range: Vec<(Vec<u8>, f64)> = Vec::new();
                    packed.for_each_in_score_range(lo, hi, |m, s| got_range.push((m.to_vec(), s)));
                    let want_range: Vec<(Vec<u8>, f64)> = sorted
                        .iter()
                        .filter(|(_, s)| *s >= lo && *s <= hi)
                        .cloned()
                        .collect();
                    prop_assert_eq!(got_range, want_range);
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
}
