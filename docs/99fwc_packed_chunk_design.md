# 99fwc — Packed-append mutable ChunkedList chunk (LPUSH/RPUSH 0.72–0.75x → parity)

**Status: design blueprint, PENDING-BENCH (authored under DISK-CRITICAL build-freeze by
CobaltCove/cc; implement with iterative build+test on disk recovery).** Owner: cod-a
(`99fwc` bead). Source: `crates/fr-store/src/packed_set.rs`.

## Problem (confirmed at source)
The mutable active chunk is `ListChunk::Owned { elems: Arc<Vec<Vec<u8>>>, lp_bytes, front_biased }`.
Every `push_back_owned`/`push_front_owned` stores the element as a **separate `Vec<u8>`
heap allocation**; the store loop also does `bytes.to_vec()` per element. Redis appends
into one packed listpack buffer per quicklist node (no per-element alloc). That per-element
alloc is the measured LPUSH/RPUSH gap (arity sweep: fr per-element work loses only at the
tail; redis-benchmark LPUSH/RPUSH 1-elem is worst case). Sealed interior chunks already use
the packed `ListChunk::Listpack { bytes, entries }` variant — only the ACTIVE mutable chunk
pays per-element Vec cost.

## Lever
Give the mutable active chunk a **packed-append** representation: a growing `Vec<u8>`
listpack-style buffer + a `Vec<ListpackValueSpan>` (offset/len) index, so appends are
contiguous byte writes (amortized, no per-element alloc), matching redis's node append.

## Implementation plan (per method, preserve byte-exactness)
Add a 3rd variant (or repurpose Owned) — proposed `ListChunk::PackedMut { buf: Vec<u8>, spans: Vec<ListpackValueSpan>, fill_bytes_cap, front_biased }`:
1. **append (push_back_owned / push_front_owned):** encode elem into `buf` with the SAME
   listpack entry encoding `owned_listpack_bytes`/the Listpack variant uses (reuse the
   existing encoder so DUMP bytes stay identical); push its span. Front-bias: keep the
   reversed-physical-order trick (append at buf tail, mark front_biased) so repeated LPUSH
   is amortized — mirror current `front_biased` semantics.
2. **accepts_append(elem, fill):** same quicklist node-boundary check as today but against
   the running `buf.len()` + encoded-elem-len instead of recomputing `owned_listpack_bytes`
   each call (this also removes the lp_bytes-recompute cost). Respect `fill` (-2 = 8KB, >0 =
   entry count) exactly as `note_command_grow`/current logic.
3. **seal:** PackedMut → `Listpack { bytes: Arc::new(buf), entries: Arc::new(spans) }` is
   nearly free (move, no re-encode) — this is the big win vs today's Owned→re-encode seal.
4. **make_mut / COW:** on a shared (refcount>1) chunk needing mutation, clone buf+spans
   (cheap memcpy) instead of cloning Vec<Vec<u8>>. For random mutation (set/insert/remove)
   convert PackedMut→Owned (unpack) OR operate in-place on buf+spans; simplest correct
   first cut: unpack to a temporary Vec for set/insert/remove (rare ops) and re-pack, OR
   keep Owned for lists that hit those ops. Keep iter reading spans→&buf[span].
5. **pop_front/pop_back:** drop first/last span and (lazily) the dead bytes; reuse the
   `dead`-bytes/compaction pattern already in CompactFieldMap (packed_set.rs:1059) so popped
   bytes are reclaimed on a threshold, not per-pop.
6. **iter / rev-iter:** yield `&buf[span.start..span.end]` — same element bytes/order.
7. **get_index / locate / len:** spans.len() and spans[i].

## Byte-exactness invariants (MUST hold; verify on recovery)
- List element ORDER unchanged (LPUSH reversal, RPUSH order, front_biased).
- OBJECT ENCODING (listpack vs quicklist) unchanged — the node-boundary `fill` logic is
  identical, so encoding transitions fire at the same cardinality/bytes.
- DUMP/DEBUG bytes identical — reuse the EXACT listpack entry encoder already used by the
  `Listpack` variant; do not introduce a second encoding.
- LINSERT/LSET/LREM/LRANGE/LPOS results identical.

## Test plan (on disk recovery, before commit-to-bench)
- `cargo test -p fr-store list` + the list DUMP byte-equality gate + `fr-conformance`
  core_list (incl live-redis) green.
- fr-OLD vs fr-NEW differential over LPUSH/RPUSH/LPOP/LINSERT/LSET/LREM/LRANGE/LMPOP across
  small (listpack) + large (quicklist) lists + front/back bias, 0 diffs.
- A/B redis-benchmark P16 lpush/rpush taskset-pinned; KEEP only if >1.0x (else revert,
  like the already-rejected VecDeque variant which measured 0.53x slower).

## Why not implemented this turn
DISK-CRITICAL build-freeze: a multi-surface state-machine rewrite cannot be written "well"
with zero compiler/test feedback — latent byte-exactness bugs would not surface until
builds resume. This blueprint lets it be implemented correctly and fast on recovery.
