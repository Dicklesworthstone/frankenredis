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
| 5 | bae131f7e | lazy set-DUMP integer view (only intset branch) | fr-store | DUMP of >512-member all-int (hashtable) sets + FORCE-flagged sets | skips a full parse+Vec<i64>/key — **CONFIRMED KEEP 2026-07-10 (cc_fr): −29.8% (hashtable) / −24.7% (listpack) `instructions:u`; biggest of the six** |
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

> **FOLLOW-UP DISCHARGED 2026-07-10 (cc_fr).** Fixed by deciding the listpack score entry
> straight from the f64 (`zset_score_listpack_entry -> {Int|Str|Reparse}`, mirroring
> `d2string`'s own branch) instead of formatting-then-re-parsing. Measured `instructions:u`,
> cv ≤ 0.02%: **fractional 1.0280 (−2.7%, the regression above, recovered)**, integral
> `>1e18 ≤2^62` **2.0389 (−51.0%)**, plain-integer guard 1.0035. Byte-exact: 20/20 score bands
> vs live vendored 7.2.4, DUMP gate PASS, fr-conformance 194/194.
>
> ⚠️ **The fix sketched below ("let the failed probe select a known-non-integer string encode
> instead of re-probing") is UNSOUND — do NOT implement it.** The probe's `None` domain is NOT
> confined to scores whose render fails `parse_listpack_integer`. `double2ll`'s window is
> `±(LLONG_MAX/2)` = **±2^62** (NOT ±2^52 — `format_redis_double`'s doc comment is wrong), and
> *above* that window grisu2 still emits a plain canonical decimal for some integral doubles:
> upstream renders `6917529027641081856` as `"6917529027641082000"` and **int-encodes** it.
> Those must still be re-parsed — hence the `Reparse` arm, pinned by the unit test
> `zset_score_reparse_arm_is_load_bearing`. The old `±1e18` gate was conservative, not wrong.

**#5 CONFIRMED KEEP — the biggest of the six.** `ctl` = verbatim pre-`bae131f7e` (the
`Option<Vec<i64>>` integer view built EAGERLY before the encoding branch) vs `cand` = HEAD
(view built only inside the intset branch that consumes it). Same source dir, only
`fr-store/src/lib.rs` differs. `perf stat -e instructions:u`, 300 sets × 40 DUMP reps, median
of 9 interleaved trials:

| shape | encoding | ctl instr/dump | cand instr/dump | ctl/cand | cv | verdict |
|---|---|---:|---:|---:|---:|---|
| 1000 int members | hashtable | 788,149 | 553,265 | **1.4245** | 0.05 / 0.07% | **−29.8% instr** |
| 99 int + 1 trailing string | listpack | 95,600 | 71,980 | **1.3281** | 0.00 / 0.27% | **−24.7% instr** |
| 400 int members | intset | 113,065 | 111,998 | 1.0095 | 1.42 / 2.41% | GUARD ok (view *is* consumed) |
| 100 string members | listpack | 45,997 | 45,861 | 1.0030 | 0.21 / 0.24% | GUARD ok (collect short-circuits) |

The two guard shapes are the point: on the intset path the view is genuinely used, and on an
all-string set the eager `collect()` short-circuits at the first non-integer member — both stay
flat, so the win is exactly the *wasted* parse. DUMP payloads identical between `ctl` and
`cand` on all four shapes. fr-conformance 105 passed / 0 failed; 14/15 DUMP/RESTORE differs PASS
(`quicklist_dump_boundary_differ` fails identically on the control ⇒ pre-existing `s36di`).

**Shape correction worth banking:** an all-integer set with ≤ `set-max-intset-entries` (512)
members encodes as **intset**, never listpack — so "a small all-int listpack set" does not exist.
The listpack path is only reachable with ≥1 non-integer member; the eager view is wasted there
only when the non-integer member is *late* in iteration order (an early one short-circuits the
`collect()`).

Ranked profile of the remaining levers' own target shapes (perf flat self%, HEAD, repeated DUMP;
set/list/hash DUMP is **not** memoized — only compact zsets are — so a repeat blast is honest):

| lever | shape | fr/redis DUMP | lever-attributable cost |
|---|---|---:|---|
| **#5** | 1000-int hashtable set | 1.457x | eager `parse_i64` sweep + `Vec<i64>`; ~31% allocator traffic — **taken, confirmed** |
| #3 | 400-int intset | 1.021x | `encode_intset` 4.47% self — ceiling is small; `lzf_compress` 63.9% dominates |
| #2 | multi-node quicklist | 1.192x | **no realloc symbols in profile** (`lzf_compress` 64.2%, `common_prefix_len` 11.4%) ⇒ presize already lean |
| #4 | listpack hash | 0.637x | **no realloc symbols** (`lzf_compress` 42.8%, `PackedStrMapIter` 11.8%, `encode_listpack_entry` 8.7%) ⇒ presize already lean |
| #1 | BGSAVE / DEBUG RELOAD | not profiled | needs its own bulk-save shape; not exercised by a DUMP loop |

So #3/#2/#4 have a small measured ceiling on the DUMP-command path and should not be chased
without a fresh bulk-save (BGSAVE / DEBUG RELOAD) profile that names them. Only #1 remains
genuinely unprofiled.

> **⚠️ THE TABLE ABOVE IS INVALID FOR #2 AND #3 — corrected 2026-07-10 (cc_fr).** Both are
> `fr-persist` RDB-save levers, and a **DUMP-command blast never calls them**:
> `encode_intset_blob` (#3) and `encode_compact_list_quicklist2` (#2) each have exactly one
> caller, inside fr-persist's RDB-save path. The "`encode_intset` 4.47% self" cited for #3 is
> actually **`fr_store::encode_intset`**, a homonym in a different crate reached from
> `dump_key`. A DUMP profile shows the only `fr_persist` symbols present are `lzf_compress`
> and `crc64_redis`. Those two rows ranked code that never executed.
>
> **Re-measured on `SAVE` (the real bulk-save path)**, 1,200 each of near-threshold hashes /
> 400-int intsets / multi-node quicklists / listpack zsets, 4.8 MB RDB, `perf -F 997`, flat self%:
>
> | frame | self% |
> |---|---:|
> | `lzf_compress` | 13.70% |
> | `__memmove_avx_unaligned_erms` | 10.35% |
> | `encode_rdb_internal` | 7.43% |
> | Rust `format!("{score}")` (grisu + float_to_decimal + format_inner) | **7.46%** |
> | `encode_listpack_entry` | 1.24% |
> | **#1** `encode_listpack_strings_blob` | **0.35%** |
> | **#3** `encode_intset_blob` | **0 samples** (called, never hot) |
> | **#2** `encode_compact_list_quicklist2` | **0 samples** (called, never hot) |
>
> ⇒ **#1, #2, #3 are CLOSED on evidence**: combined ceiling ≤ 0.35% self on the path they
> actually run on. The removable mass was the 7.46% Rust float formatter — a *correctness*
> bug (RDB scores rendered with Rust `Display`, silently truncated to `1.5e+126` / `0` by a
> real redis's 128-byte `zzlStrtod` buffer), fixed in `59fe5dc40`. #4 is `fr-store`
> DUMP-command code and was ranked on an input that reaches it, so its row stands.
>
> **MANIFEST STATUS after this correction — the sentence that used to read "Remaining unverified:
> #1, #2, #3, #4" is obsolete:**
>
> | lever | status |
> |---|---|
> | #5 lazy set-DUMP integer view | **CONFIRMED KEEP** (−29.8% / −24.7% `instructions:u`) |
> | #6 zset listpack DUMP direct-emit | **CONFIRMED KEEP** (−6.7…−11.6% `instructions:u`, byte-exact) |
> | #1 listpack-blob builders | **CLOSED on evidence** — 0.35% self on `SAVE` |
> | #2 quicklist node presize | **CLOSED on evidence** — 0 samples on `SAVE` (called, never hot) |
> | #3 intset encode in place | **CLOSED on evidence** — 0 samples on `SAVE` (called, never hot) |
> | #4 DUMP-command entry presize | ranked on a reaching input (no realloc symbols); **verification BLOCKED** |
>
> **Why #4 cannot be finished right now, and why cod_fr's SORT bench does not unblock it.** Verifying a
> manifest lever means A/B-ing two *server* binaries (pre-hunk vs HEAD) under `perf stat`. `rch` does
> not return a linked binary — only worker-scoped artifacts — and a local build is forbidden. cod_fr's
> `51.82%` SORT comparator win was measured by an **in-crate bench target** that runs entirely inside
> the remote worker's own process (`cargo bench -p fr-command`), so it never needed a binary shipped
> back. No such in-crate harness exists for a DUMP-command server A/B. Unblock = retrieve the built
> `fr-server`, or authorize one local `release-perf` build.

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
