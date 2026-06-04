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
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
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
        let survivors: Vec<Vec<u8>> = self.iter().filter(|e| keep(e)).map(<[u8]>::to_vec).collect();
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
pub enum ListValue {
    Packed(PackedList),
    Deque(VecDeque<Vec<u8>>),
}

impl Default for ListValue {
    fn default() -> Self {
        ListValue::Packed(PackedList::new())
    }
}

impl ListValue {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            ListValue::Packed(p) => p.len(),
            ListValue::Deque(d) => d.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&[u8]> {
        match self {
            ListValue::Packed(p) => p.get(idx),
            ListValue::Deque(d) => d.get(idx).map(|v| v.as_slice()),
        }
    }

    fn promote(&mut self) {
        if let ListValue::Packed(p) = self {
            let mut d: VecDeque<Vec<u8>> = VecDeque::with_capacity(p.len() + 1);
            for e in p.iter() {
                d.push_back(e.to_vec());
            }
            *self = ListValue::Deque(d);
        }
    }

    fn maybe_promote(&mut self, added_len: usize) {
        if let ListValue::Packed(p) = self
            && (p.len() >= PACKED_MAX_ENTRIES || added_len > PACKED_MAX_VALUE)
        {
            self.promote();
        }
    }

    pub fn push_back(&mut self, elem: Vec<u8>) {
        self.maybe_promote(elem.len());
        match self {
            ListValue::Packed(p) => p.push_back(&elem),
            ListValue::Deque(d) => d.push_back(elem),
        }
    }

    pub fn push_front(&mut self, elem: Vec<u8>) {
        self.maybe_promote(elem.len());
        match self {
            ListValue::Packed(p) => p.push_front(&elem),
            ListValue::Deque(d) => d.push_front(elem),
        }
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        match self {
            ListValue::Packed(p) => p.pop_front(),
            ListValue::Deque(d) => d.pop_front(),
        }
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        match self {
            ListValue::Packed(p) => p.pop_back(),
            ListValue::Deque(d) => d.pop_back(),
        }
    }

    /// Replace the element at `idx` (LSET); false if out of range.
    pub fn set(&mut self, idx: usize, elem: Vec<u8>) -> bool {
        match self {
            ListValue::Packed(p) => p.set(idx, &elem),
            ListValue::Deque(d) => {
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
    pub fn insert(&mut self, idx: usize, elem: Vec<u8>) {
        self.maybe_promote(elem.len());
        match self {
            ListValue::Packed(p) => p.insert(idx, &elem),
            ListValue::Deque(d) => d.insert(idx, elem),
        }
    }

    pub fn remove(&mut self, idx: usize) -> Option<Vec<u8>> {
        match self {
            ListValue::Packed(p) => p.remove(idx),
            ListValue::Deque(d) => d.remove(idx),
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        match self {
            ListValue::Packed(p) => p.retain(&mut keep),
            ListValue::Deque(d) => d.retain(|v| keep(v)),
        }
    }

    pub fn clear(&mut self) {
        *self = ListValue::default();
    }

    #[must_use]
    pub fn iter(&self) -> ListValueIter<'_> {
        match self {
            ListValue::Packed(p) => ListValueIter::Packed(p.iter()),
            ListValue::Deque(d) => ListValueIter::Deque(d.iter()),
        }
    }
}

impl From<VecDeque<Vec<u8>>> for ListValue {
    fn from(d: VecDeque<Vec<u8>>) -> Self {
        if d.len() > PACKED_MAX_ENTRIES || d.iter().any(|e| e.len() > PACKED_MAX_VALUE) {
            ListValue::Deque(d)
        } else {
            let mut p = PackedList::new();
            for e in &d {
                p.push_back(e);
            }
            ListValue::Packed(p)
        }
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

#[cfg(test)]
mod tests {
    use super::{PackedList, PackedStrMap, PackedStrSet};
    use indexmap::{IndexMap, IndexSet};
    use std::collections::VecDeque;
    use proptest::prelude::*;

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
        assert_eq!(l.iter().collect::<Vec<_>>(), vec![&b"a"[..], b"x", b"BBBBB", b"c"]);
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
    fn map_insert_update_keeps_position() {
        let mut m = PackedStrMap::new();
        assert_eq!(m.insert(b"a".to_vec(), b"1".to_vec()), None);
        assert_eq!(m.insert(b"b".to_vec(), b"2".to_vec()), None);
        assert_eq!(m.insert(b"c".to_vec(), b"3".to_vec()), None);
        // updating an existing field keeps its position, returns the old value,
        // and handles a value-length change (1 -> 5 bytes).
        assert_eq!(m.insert(b"b".to_vec(), b"22222".to_vec()), Some(b"2".to_vec()));
        let pairs: Vec<(&[u8], &[u8])> = m.iter().collect();
        assert_eq!(pairs, vec![(&b"a"[..], &b"1"[..]), (b"b", b"22222"), (b"c", b"3")]);
        assert_eq!(m.get(b"b"), Some(&b"22222"[..]));
        assert_eq!(m.shift_remove(b"a"), Some(b"1".to_vec()));
        assert_eq!(m.get_index(0), Some((&b"b"[..], &b"22222"[..])));
        assert_eq!(m.len(), 2);
    }
}
