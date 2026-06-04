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

use indexmap::IndexSet;

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

#[cfg(test)]
mod tests {
    use super::{GenericSet, PackedStrSet};
    use indexmap::IndexSet;
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
    }
}
