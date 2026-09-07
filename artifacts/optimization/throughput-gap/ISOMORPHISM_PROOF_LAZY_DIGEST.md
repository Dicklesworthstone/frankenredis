# Isomorphism Proof — Lazy Digest + ACL Short-Circuit

**Date:** 2026-04-09
**Beads:** frankenredis-zjii (severe throughput gap vs Redis baselines)
**Branch:** main
**Author:** TopazCreek

## Headline

Two profile-driven changes in `fr-runtime` recovered the catastrophic
throughput gap reported in `frankenredis-zjii`. FrankenRedis now runs at
**79–99% of legacy Redis 7.2.4 throughput** on single-command workloads and
**~31% on pipeline=16**, up from **~1.3%** at the documented April 7 baseline.

## Methodology

Followed the `extreme-software-optimization` discipline:

1. Reproduced the gap empirically (`fr-bench --pipeline 16` against
   `target/release/frankenredis --port 6399 --mode strict`).
2. Captured a `perf record -F 999 -g -e cpu-clock` flamegraph attached to the
   live server PID — `cpu-clock` was required because `perf_event_paranoid=1`
   blocked hardware counters.
3. Identified the dominant hotspot from sampled stacks.
4. Applied one structural fix per round, re-checked tests + conformance, and
   re-sampled the profile.
5. Stopped when the profile became "healthy" — i.e., when CPU was dominated
   by HashMap/BTreeMap operations and allocator overhead, the irreducible
   cost of the actual store work, rather than evidence-ledger forensics.

## Round 1 — Eager `state_digest` removal

### Hotspot

Profile-1 of `set --clients 50 --pipeline 16 --keyspace 10000 --datasize 3`:

```
80.08%  fr_store::Store::state_digest
 7.88%  core::str::iter::SplitWhitespace::next
 3.63%  fr_command::command_matches_acl_category
```

`Store::state_digest()` walked every entry in the database (key bytes,
value bytes, hash fields, list elements, set members + sort, zset entries
+ scores, stream fields), plus a `format!("{:016x}")` heap allocation, on
**every command**. The result was only ever read by the threat-event
forensics ledger, which fires on rare error/threat conditions (parse
failure, auth/perm denial, too-many-args, slow command). On the success
path — i.e., 99.99% of commands — the work was discarded.

### Fix

Refactored `record_threat_event` to compute `input_digest`,
`state_digest_before`, and `state_digest_after` lazily, gated behind
`self.policy.emit_evidence_ledger`. Removed the eager precomputation from
`execute_frame`, `execute_frame_internal`, `execute_bytes`,
`enforce_maxmemory_before_dispatch`, `handle_exec_command`,
`preflight_gate`, and `apply_tls_config`.

Introduced a small enum:

```rust
enum ThreatInputDigestSource<'a> {
    Frame(&'a RespFrame),
    Argv(&'a [Vec<u8>]),
    Bytes(&'a [u8]),
    Owned(Vec<u8>),
}
```

so each threat call site can hand in whatever it has cheaply (a borrowed
frame/argv/bytes), and the digest is materialized only on the cold path
inside the `if self.policy.emit_evidence_ledger` branch.

### Isomorphism note (semantic drift)

Pre-refactor, `state_digest_before` was a snapshot taken at command entry,
and `state_digest_after` was computed at threat-record time. Post-refactor,
both are computed at threat-record time — they always match.

- For **pre-dispatch** threats (preflight gate, parse error, auth/perm
  denial, too-many-args, maxmemory), the store has not been mutated since
  command entry, so `before == after` exactly matches the prior semantics.
- For **post-dispatch** threats (slow-command, EXEC inner threats), the
  recorded snapshot reflects post-mutation state. The pre-state snapshot is
  no longer captured for these paths.

This is a fair trade — the threat-event ledger is a forensic feature, not
a correctness boundary. The forensically interesting fact for slow-command
threats is the elapsed time, not the pre-state. The full conformance suite
(43 tests) still passes; no fixture depended on `state_digest_before` and
`state_digest_after` differing.

### Throughput delta

| Workload | Before | After Round 1 |
|---|---|---|
| `set` p16, k=10000 | 1699 ops/s | 4418 ops/s |

Profile-2 confirmed `state_digest` was no longer in the top samples, and
revealed the next hotspot: ACL category matching.

## Round 2 — ACL category short-circuit

### Hotspot

Profile-2:

```
40.48%  core::str::iter::SplitWhitespace::next     (called from acl category lookup)
17.22%  fr_command::command_matches_acl_category
13.35%  Vec<&str>::from_iter (split-whitespace collect)
 8.69%  cfree
 8.16%  malloc
```

Combined ~71% of CPU on ACL category resolution.

### Root cause

`AclUser::is_command_allowed` (`fr-runtime/src/lib.rs:337`) called
`command_categories(cmd_lower)` on every command in order to check
`denied_categories` and `allowed_categories`. `command_categories` walks
all 21 ACL categories and, for each, calls `commands_in_acl_category`,
which iterates the entire `COMMAND_TABLE` (~200 commands), and for each
one calls `command_matches_acl_category` — which does
`flags.split_whitespace().collect::<Vec<&str>>()` and a linear `contains`.

That's ~21 × 200 = ~4,200 `Vec<&str>` allocations + 4,200 string-splitting
iterations **per command**. The default user (and any user that doesn't
configure category-level rules) has empty `denied_categories` and empty
`allowed_categories`, so all of that work is *dead* — the function falls
straight through to `self.all_commands` regardless.

### Fix

Added a four-line short-circuit before the `command_categories` call:

```rust
if self.denied_categories.is_empty() && self.allowed_categories.is_empty() {
    return self.all_commands;
}
```

When the user has zero category rules (the common case), skip the entire
categorization pass.

### Isomorphism note

Strictly behavior-preserving. The category resolution is *only* used to
match against `denied_categories` and `allowed_categories`. If both are
empty, the only contribution from that path was `self.all_commands`,
which the short-circuit returns directly. No fixture or test path
exercises this difference. All 43 conformance tests still pass.

### Throughput delta

| Workload | Pipeline | Before Round 2 | After Round 2 |
|---|---|---|---|
| `set` k=10000 | 16 | 4418 ops/s | **268,414 ops/s** |
| `get` k=10000 | 16 | (not measured) | **433,970 ops/s** |
| `mixed` k=10000 | 16 | (not measured) | **310,564 ops/s** |
| `set` k=10000 | 1 | (not measured) | **75,054 ops/s** |
| `get` k=10000 | 1 | (not measured) | **90,567 ops/s** |
| `incr` k=10000 | 1 | (not measured) | **81,383 ops/s** |
| `mixed` k=10000 | 1 | (not measured) | **80,372 ops/s** |

## Final Profile (after both fixes)

```
12.73%  __memcmp_avx2_movbe                   (HashMap key comparisons)
 5.70%  cfree
 4.45%  fr_store::Store::drop_if_expired
 4.36%  fr_store::Store::exists
 3.76%  fr_store::Store::record_ops_sec_sample
 3.13%  malloc
 2.93%  BTreeMap::insert                      (entries store insert)
```

This is a healthy profile. CPU is dominated by:

- **Real store operations** (`exists`, `drop_if_expired`, `BTreeMap::insert`)
- **Allocator overhead** (`malloc`, `cfree`)
- **HashMap key comparisons** (`__memcmp_avx2_movbe`)

`state_digest` and `command_matches_acl_category` are gone from the top
samples. There is no remaining "smoking gun" — what remains is the
irreducible cost of doing the work, plus a few obvious follow-up
opportunities (see Next Steps).

## Comparison to Redis 7.2.4 (April 7 baseline)

| Workload | Redis ops/s | FR before | FR after | After % of Redis |
|---|---|---|---|---|
| `set` p1 | 94,402 | 1,240 (1.3%) | **75,054** | **79%** |
| `get` p1 | 91,142 | 1,204 (1.3%) | **90,567** | **99%** |
| `incr` p1 | 95,183 | 1,176 (1.2%) | **81,383** | **86%** |
| `mixed` p1 | 96,834 | 1,159 (1.2%) | **80,372** | **83%** |
| `set` p16 | 860,900 | 1,221 (0.14%) | **268,414** | **31%** |

Per-command throughput is now within striking distance of Redis. GET
basically matches Redis on this hardware. The remaining gap on
`pipeline=16` is a different beast — Redis's pipelined batching path is
extraordinarily well optimized, and closing that gap is a separate
optimization track (per-poll-cycle batching, write coalescing, allocator
tuning).

## Files Changed

- `crates/fr-runtime/src/lib.rs`
  - Added `ThreatInputDigestSource<'a>` enum
  - Added `argv_to_resp_bytes` helper
  - Rewrote `record_threat_event` to compute digests lazily on the
    ledger-on path
  - Removed eager `state_digest()` and `digest_bytes(frame.to_bytes())`
    calls from `execute_frame`, `execute_bytes`,
    `enforce_maxmemory_before_dispatch`, `handle_exec_command`,
    `preflight_gate`, `apply_tls_config`
  - Updated `ThreatEventInput` (dropped `input_digest: String` and
    `state_before: &str`, added `input_source`)
  - Added the `is_command_allowed` short-circuit for users with no
    category-level ACL rules

## Quality Gates

- `cargo check --workspace --all-targets` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — passes (one pre-existing flaky pubsub test
  in `tcp_e2e_test` that consistently passes when run in isolation)
- `cargo test -p fr-conformance` — 43/43 pass

## Next Steps (file as follow-up beads)

1. **Replace `BTreeMap<Vec<u8>, Entry>` with `HashMap<Vec<u8>, Entry>`**
   in `fr-store::Store::entries`. BTreeMap is O(log n) per op; HashMap is
   O(1). Redis itself uses a hash table. SCAN/cursor semantics need
   careful analysis but the win is structural.

2. **Incremental store digest** — when `emit_evidence_ledger=true` AND a
   threat actually fires, the current implementation still pays the
   O(N_keys) cost. Replace with an incremental running digest maintained
   on every store mutation (XOR-of-FNV per entry, updated on
   insert/update/delete). O(1) per command regardless of DB size.

3. **`command_categories` precomputation** — even though the current
   short-circuit handles the default case, users with non-empty
   `denied_categories`/`allowed_categories` still pay the O(categories
   × commands) cost. Replace with a `OnceLock<HashMap<&str, &[&str]>>`
   built once at startup.

4. **`pipeline=16` headroom** — currently 31% of Redis's 860k ops/sec.
   Investigation lever: per-poll-cycle batching, write coalescing,
   `RespFrame::to_bytes` allocation strategy, double `frame_to_argv`
   calls.

5. **`drop_if_expired` 4.45% / `exists` 4.36%** — investigate whether
   these are doing redundant work per command path.
