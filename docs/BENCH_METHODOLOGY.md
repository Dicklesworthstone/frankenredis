# Bench & Build Methodology — frankenredis

Rules that are **not optional**. Each one exists because breaking it produced confident,
low-variance, *wrong* numbers that were nearly written into the ledger.

Scope: this file is repo-local. Do **not** copy it into `/data/projects/AGENTS.md`, which is the
operator's shared global file.

---

## 1. The only sanctioned build/bench/test form

```sh
RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec -- cargo <subcommand> ...
```

Use it for **every** `cargo build`, `cargo test`, and `cargo bench`. No exceptions.

**Why `RCH_REQUIRE_REMOTE=1`:** `rch` defaults to *"Strict remote: off"*. When it cannot reserve a
remote slot (`rch diagnose` will show `worker assigned, slots remaining 0, queue policy
queue_when_busy`), it **silently falls back to building LOCALLY**. All 12 workers can be healthy and
an obedient-looking `rch exec -- cargo bench` still becomes a local build. On this host a local
`cargo bench` drains roughly **73 GB/hour**. `RCH_REQUIRE_REMOTE=1` makes `rch` **fail closed**: it
errors instead of quietly building locally.

**If `rch` errors with no remote slot, that is a BLOCKER.** Wait and retry, or do analysis-only work
(ledger-grep, read already-captured profiles, write the ranked frame table, design the lever, save
the patch under `tests/artifacts/perf/` and park it). **Never** fall back to a local build.

**Never run `maturin build` at all** — it fails open to a local build.

## 2. `env -u CARGO_TARGET_DIR` is mandatory

`~/.zshrc` globally exports `CARGO_TARGET_DIR=/data/tmp/cargo-target`. `rch exec` inherits it,
switches into custom-target-dir mode, and bench/build artifact retrieval comes back as
~0 bytes (`Custom CARGO_TARGET_DIR artifacts retrieved: 2 files, 769 bytes`). Unsetting it per
invocation is required; do not merely hope the shell is clean.

Corollary, learned the hard way: **`rch` does not return the linked binary.** Anything that needs a
real `fr-server` (a live-server A/B, `perf record` on the server, a `fr-bench` row) cannot be done
through `rch` today. Profiling an **already-existing** binary is fine and needs no Cargo at all.

## 3. A/B substrate

**An A/B split across two `rch` invocations is INVALID.** `rch exec` has no `--worker` flag, picks
workers non-deterministically, and the ORIG/CAND ratio is **not worker-invariant**. Discard any ratio
measured that way.

Instead:

- Bench **both arms in ONE binary and ONE invocation.** Keep the pre-change implementation as a
  **bench-only reference fn** (ideally in the crate that owns the code, behind
  `#[cfg(any(test, feature = "bench-reference"))]`), so it stays faithful to what shipped.
- **Interleave the arms WITHIN a single measured routine.** Criterion group members run
  **sequentially** — `group.bench_with_input` does *not* alternate — so registering two ids in one
  group does **not** cancel drift. Alternate arms round-robin (A,B / B,A) inside one measured loop
  and take min-of-N (the repo's existing convention: "min-of-41 interleaved", "median of 9
  interleaved trials").
- **`black_box` both the inputs and the results.** A reference arm written as
  `match (None::<&()>, from_utf8(l), from_utf8(r))` has a statically-unreachable guarded arm, so LLVM
  is free to **delete the exact work under test**. Pass the discriminating value (e.g. the collator)
  through `black_box` so it is a runtime value, and consume the result.
- Never use the "stash ORIG, bench, pop, bench NEW" recipe: it assumes one machine, and
  `git stash` is forbidden here.

Prefer `instructions:u` over wall-clock. Workers and this host are shared with other agents; wall
time is noise, retired instructions are not.

## 4. Ledger integrity

**Before trusting *or* writing any REJECT: profile-verify that the benchmark actually executes the
function under test, with non-zero self-time attributable to it. Record that self-time in the entry.**

A row whose bench never reached the code is measuring dead code and is **INVALID** — say so in the
ledger, reopen the lever, and re-measure on an input where the function is hot.

Two real instances from this repo:

- Manifest levers **#2/#3** are `fr-persist` RDB-save code but were ranked with a **DUMP-command**
  blast that never calls them. The "`encode_intset` 4.47% self" cited for #3 was
  `fr_store::encode_intset` — a **homonym in a different crate**. Re-measured on `SAVE`, their real
  ceiling is ≤ 0.35% self.
- The **SORT** REJECT row said outright *"needs a profile I couldn't get — `perf record -p` returned
  no samples"*, then asserted a root cause. Profiled properly, the top frame is
  `core::str::converts::from_utf8` at **35.43% self** (20.35% under P16) — a frame nobody had named.

Distinguish two cases when you write the verdict: **never called** on that input (a dead-code
measurement ⇒ INVALID) versus **called but not hot** (a real, small ceiling ⇒ REJECT stands, with the
number recorded).

## 5. Binary provenance — prove the arm you measured contains the hunk

Separate from dead code: the bench can run live code that is **not the code the row is about**. As of
2026-07-10 our ledgers hold **70 REJECT rows; only 3 record a binary `sha256` and only 10 record any
self-time.** Both provenance failures have actually happened here:

- **Measured a copy.** A bench-only ORIG comparator, `#[inline(never)]` and taking the real runtime
  `Option<&CollatorBorrowed>`, still had its `from_utf8` eliminated by LLVM because with `None` the
  results were unobservable. The profile gate caught it (0% `from_utf8` self, 17 samples). The
  repaired harness — symmetric `black_box` barriers on **both** arms — then measured a real 51.82%
  instruction win.
- **Measured identical arms.** A `git status`-clean HEAD worktree sharing `CARGO_TARGET_DIR` with the
  main tree linked the *candidate's* rlib into the "control". Both arms ran the same code and the
  guard read `1.0000`.

So every A/B row must record:

1. a distinct `sha256` for each arm's binary; **and**
2. a symbol- or frame-level check that the candidate actually contains the hunk —
   `strings -a <bin> | grep -x <fn>` (use `-x`/`-w`: `grep -c zset_score_listpack_entry`
   false-positives on `encode_zset_score_listpack_entry` from another crate), or the changed function
   appearing in the candidate's own profile.

A heading that says "rejected **and reverted**" must state whether the revert preceded the
measurement.

## Worker facts (verify, don't assume)

- **Not all rch workers are equal.** `hz2` has no `perf` executable; `ovh-a` runs
  `perf_event_paranoid = 4` (no counters, no sampling); `hz1` completed a full profile + PMU A/B in
  one fail-closed invocation. A perf failure on one worker is **not** a fleet-wide blocker — retry,
  and record which worker produced the number.
- `rch` does **not** return a linked binary. An **in-crate bench target** (`cargo bench -p <crate>`)
  runs entirely inside the worker's own process and therefore works; a **server A/B** (two
  `fr-server` binaries under `perf stat`) does not, because the binaries never come back. This is the
  line between what can and cannot be measured today.
- This host has `perf_event_paranoid = 1`, so profiling an **already-existing** binary is always
  available and needs no Cargo at all.

---

## Profiling recipe that actually works here

Needs no Cargo and no binary retrieval — profile an existing, symbol-verified binary.

1. **Symbol-verify every binary before benching it.**
   `strings -a <bin> | grep -x <new_fn>` (expect 1 for cand, 0 for ctl) and the inverse for the old
   symbol. Use `grep -x`/`-w`: `grep -c "zset_score_listpack_entry"` false-positives on
   `encode_zset_score_listpack_entry` from another crate. A `git status`-clean worktree sharing a
   `CARGO_TARGET_DIR` once linked the **candidate's** rlib into the "control" — both arms ran
   identical code, and the guard shape read a clean `1.0000`.
2. **Quiesce before attaching perf.** The first perf window after seeding charges **~130.7M
   instructions with zero commands issued** (dict rehash / cron) versus ~551K once quiet — about 16%
   of a DUMP pass, varying with machine load. Sleep ~3 s after seeding, *before* `perf record`
   attaches. This alone took cv from 10–22% down to ≤0.03% and dissolved a phantom "+10.6%
   regression" (the *same* control binary measured 123,622 then 136,700 instr/op across runs).
3. **Attach ≥0.6 s before the measured loop starts**, or the loop begins unmeasured and silently
   drops instructions — an undercount that looks like a win for whichever arm lost the race.
4. **Pipeline the client heavily.** A single unpipelined Python client cannot saturate fr; that is
   why the original SORT row got *"`perf record -p` returned no samples"* and shipped a REJECT with
   no profile at all.
5. **Always include a guard shape** whose code path both arms share. It must land at ~1.000. If it
   does not, the harness is lying, not the lever.
6. Pin the server to one core and the client to others (`taskset -c 2` / `-c 6,7`).

## Workload traps

- **Compact-zset `DUMP` is memoized** (`Store::dump_payload_cache`, keyed by
  `(key, modification_count, zset_max_listpack_*)`). A repeat-DUMP blast is ~49.7x cheaper and
  measures `Vec::clone`, **not the encoder**. Reseed and DUMP each key exactly once.
  `FLUSHALL` *does* clear the cache, so reseed-then-DUMP-once is a valid cold measurement.
  Set / list / hash DUMP are **not** memoized.
- The **shared working tree** may hold a peer's uncommitted WIP. `git add <file>` stages *all* hunks
  in that file, including theirs. Check `git status` immediately before staging, and never straddle a
  peer's dirty file across the two arms of an A/B.
