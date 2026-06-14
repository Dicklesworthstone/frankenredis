# q5ody — list DUMP/MIGRATE O(n·node_size) → O(n) (75x), byte-exact

**Bead:** frankenredis-list-dump-quicklist-reencode-q5ody
**Status:** PROVEN in worktree, ready to apply. fr-store is exclusively reserved
by CoralOx (their WIP does NOT touch these fns), so this is a handoff, not a
direct commit. `fix.patch` applies cleanly to fr-store at HEAD 866a14b39 / e61640aa7.

## Bug
`fr-store::encode_dump_quicklist2` (the DUMP/MIGRATE list serializer) built
quicklist PACKED nodes by calling `quicklist_packed_node_allows_append(&packed,
item, fill)` **per item**, which did `current.to_vec()` + `encode_listpack_strings(trial)`
— i.e. it **re-encoded the entire accumulated node listpack on every append**.
Per node of k entries: Σ O(i) = O(k²); total O(n·node_size).

## Fix
Track the current packed node's listpack byte size **incrementally**. New
`listpack_entry_encoded_len(item)` returns the exact encoded length of one
listpack entry in O(1) (integer-band vs string-header + payload + backlen);
`quicklist_packed_node_accepts(count, bytes, next_size, fill)` reproduces the
exact `quicklist_packed_node_fits` predicate (positive fill = entry-count +
8192 SIZE_SAFETY_LIMIT; negative fill = byte budget) from the running size. Node
boundaries are identical → byte-exact output. Removed the now-dead
`quicklist_packed_node_allows_append`.

## Proof
- **Helper exactness:** unit test `listpack_entry_encoded_len_matches_real_encoder`
  asserts the O(1) length == `encode_listpack_strings([e]).len()-7` across every
  integer band + string-header/backlen band + non-canonical-integer strings.
- **Byte-exact end-to-end:** DUMP output identical (sha256) between the clean-HEAD
  baseline binary and the patched binary across 8 list shapes spanning 1..many
  nodes, mixed int/string, near-64B headers, PLAIN-node (>budget element), and a
  20000-element boundary list. All BYTE-EXACT.
- **Perf (list/10000 DUMP, non-pipelined median µs/op):**
  baseline 62488 → **patched 831 = 75.1x**; redis 160 (patched/redis 0.19x — the
  catastrophic gap is gone; the residual ~5x is a separate, smaller lever:
  ChunkedList iteration + per-node listpack alloc).
- fr-store dump/list/quicklist tests pass; clippy clean.

## Score
Impact 5 (75x on a real command, also MIGRATE of large lists) × Confidence 5
(byte-exact proven) / Effort 2 ≈ 12.5 — well above 2.0.

Apply: `git apply artifacts/optimization/blackthrush-q5ody-list-dump/fix.patch`
(from repo root, when fr-store is in a buildable state).
