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
| 6 | 921d21913 | zset listpack DUMP direct-emit (drop per-member copy + 2 Vecs) | fr-store | DUMP-loop on large listpack zsets; BGSAVE of many zsets | **N member-copies eliminated/zset** (largest of the set) — **CONFIRMED KEEP 2026-07-10 (cc_fr): −6.7…−11.6% `instructions:u`, byte-exact** |

## Status (cc_fr, 2026-07-10)

**#6 CONFIRMED KEEP.** Benched by a factorial A/B of three `release-perf` binaries built from
ONE source dir, differing only in `dump_key`'s zset-listpack branch:
`ctlA` = pre-`921d21913` `Vec<Vec<u8>>` pairs materialization · `ctlB` = direct-emit with
string scores · `cand` = HEAD (direct-emit + the later `9ce4b42ac` int-score shortcut).
`ctlA/ctlB` isolates #6; `ctlB/cand` isolates the int-score shortcut.

Server-side `perf stat -e instructions:u`, cold dump-payload cache (each key DUMPed exactly
once; see caveat below), 18,000 zsets × 100 members × 32-byte members, median of 9 trials,
all cv ≤ 4.92%:

| shape | ctlA/ctlB (**lever #6**) | ctlB/cand (int-score lever) | ctlA/cand |
|---|---:|---:|---:|
| integer scores | **1.131** (−11.6% instr) | 1.203 (−16.9%) | 1.360 |
| fractional scores | **1.076** (−7.1% instr) | **0.975 (+2.6% instr — regression)** | 1.049 |

Reproduced across three independent runs (#6 = 1.104 / 1.111 / 1.131 int; 1.072 / 1.121 /
1.076 frac). DUMP payloads byte-exact vs vendored redis 7.2.4 for all three binaries and both
shapes. 14/15 DUMP/RESTORE differential harnesses PASS (`quicklist_dump_boundary_differ` fails
identically on the control ⇒ pre-existing, bead `frankenredis-s36di`).

**BENCH CAVEAT (bank this):** compact-zset DUMP is memoized in `Store::dump_payload_cache`,
keyed by `(key, modification_count, zset_max_listpack_*)` and cleared by `flushdb`. A
repeated-DUMP blast measures `Vec::clone` of the cache, **not the encoder**. Any DUMP bench
here must reseed and DUMP each key exactly once.

**NEW FINDING (not part of #6):** the int-score shortcut `9ce4b42ac` is a large win on integer
scores but costs **+2.6…+2.9% instructions on fractional-score zsets** (reproducible in all
three runs) — it pays a failed `zset_score_listpack_integer` probe per member, then
`encode_listpack_entry` re-probes the formatted string via `parse_listpack_integer`. This
reconciles the apparent contradiction between ledger row "zset DUMP integer-score listpack
shortcut … mixed then rejected (0.9559x)" and `9ce4b42ac`'s "+37%": **both are correct** — the
lever wins on integer scores and loses slightly on fractional ones. Filed as a follow-up.

Remaining unverified: #1, #2, #3, #5 (and #4, whose zset half is already folded into #6's
`Vec::with_capacity`). Note #1/#2/#3/#5 sit on RDB-save / intset / set paths that a
DUMP-command loop over listpack zsets does **not** exercise — each needs its own shape.

Ranked profile of the DUMP-command path (perf, flat self%, HEAD, 30k zsets × 100 members,
cold cache): `lzf_compress` **32.5%**, `Store::dump_key` 12.7%, `crc64_redis` 5.3%,
`encode_listpack_entry` 5.1%, `memmove` 3.8%, `foldhash` 3.4% (the dump-cache insert),
`encode_listpack_backlen` 3.2%, `parse_listpack_integer` 2.2%. fr/redis DUMP throughput is
0.37–0.53x, so the DUMP-command gap is dominated by LZF + the cache-insert clone, not by the
listpack encoder the manifest levers target.

Notes:
- Conformance: all byte-identical; the full DUMP-gate family (hash/set/zset/intset/quicklist/
  stream) + scan_invariant + string_encoding PASS on the final binary (8/8).
- The biggest expected win is #6 (zset direct-emit) — it removes a `member.to_vec()` per member,
  bringing the DUMP-command side to parity with the RDB-save side which already direct-emits.
- Hash DUMP-command was deliberately NOT direct-emitted: its `pairs` is already `Vec<&[u8]>`
  (references, no member copies), so direct-emit would only save one pointer-Vec (micro-lever).
- These do NOT target the realistic GET/SET hot path (already parity-or-faster; contended +
  un-benchable under cargo-check-only). They target persistence/MIGRATE throughput.
