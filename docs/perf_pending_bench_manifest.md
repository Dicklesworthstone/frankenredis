# Pending-Bench Manifest — cc code-first perf levers (cod-walled session, 2026-06-18)

These levers were written **code-first** under the cargo-check-only constraint (no release
build, no rch), so their *magnitude* is unverified — each is an algorithmically-certain,
**byte-identical** change (verified vs redis 7.2.4 by the DUMP-gate family; all 8 gates PASS
collectively on the final binary). This manifest tells the batch/rch run exactly **what to
bench** to confirm or reject each. All are on the RDB-save (`rdbSaveObject`) + DUMP-command +
MIGRATE-payload encode paths — i.e. persistence/migration throughput, NOT the GET/SET hot path.

Bench harness (release build, via rch): build `frankenredis` + `fr-bench` in `--release`, then
for each lever run the **DUMP workload** on the relevant value shape and the **DEBUG RELOAD /
BGSAVE timing** on a DB seeded with many such keys. Confirm wins with the comprehensive-bench
matrix; gate against `.bench-history`. Reject (move ledger row to "rejected") if a release A/B
shows no stable win.

| # | Commit | Lever | Crate | Bench target (release A/B) | Expected |
|---|--------|-------|-------|---------------------------|----------|
| 1 | 71a908f75 | presize collection listpack-blob builders | fr-persist | BGSAVE/DEBUG RELOAD of a DB of many near-threshold (≈8 KiB) hashes/sets/zsets; DUMP-loop on same | fewer reallocs/key on bulk save |
| 2 | c83e5e926 | presize quicklist node listpack buffer (both encoders) | fr-persist + fr-store | DUMP + DEBUG RELOAD of multi-node (>8 KiB) lists | fewer reallocs/node |
| 3 | 78fff02e8 | intset encode sorts owned values in place (drop to_vec) | fr-persist | DUMP + BGSAVE of large int-sets (≤512 ints) | one fewer Vec<i64> alloc+copy/key |
| 4 | ca61b6ca4 | presize DUMP-command listpack entry buffers (hash/zset/set) | fr-store | DUMP-loop on listpack hash/zset/set | fewer reallocs/DUMP |
| 5 | bae131f7e | lazy set-DUMP integer view (only intset branch) | fr-store | DUMP of >512-member all-int (hashtable) sets + FORCE-flagged sets | skips a full parse+Vec<i64>/key |
| 6 | 921d21913 | zset listpack DUMP direct-emit (drop per-member copy + 2 Vecs) | fr-store | DUMP-loop on large listpack zsets; BGSAVE of many zsets | **N member-copies eliminated/zset** (largest of the set) |

Notes:
- Conformance: all byte-identical; the full DUMP-gate family (hash/set/zset/intset/quicklist/
  stream) + scan_invariant + string_encoding PASS on the final binary (8/8).
- The biggest expected win is #6 (zset direct-emit) — it removes a `member.to_vec()` per member,
  bringing the DUMP-command side to parity with the RDB-save side which already direct-emits.
- Hash DUMP-command was deliberately NOT direct-emitted: its `pairs` is already `Vec<&[u8]>`
  (references, no member copies), so direct-emit would only save one pointer-Vec (micro-lever).
- These do NOT target the realistic GET/SET hot path (already parity-or-faster; contended +
  un-benchable under cargo-check-only). They target persistence/MIGRATE throughput.
