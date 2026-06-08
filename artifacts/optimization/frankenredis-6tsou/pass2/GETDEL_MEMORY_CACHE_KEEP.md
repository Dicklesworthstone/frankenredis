# frankenredis-6tsou GETDEL Memory-Cache Keep

## Scope

- Bead: `frankenredis-6tsou`
- Lever: after `GETDEL` removes an entry, decrement an exact cached memory-usage
  total instead of forcing the next runtime sample to rescan the store.
- Production file: `crates/fr-store/src/lib.rs`
- Candidate build: `/tmp/codex-fr-6tsou-pass2-candidate-target/release-perf/frankenredis`
- Baseline build: `/tmp/codex-fr-6tsou-pass2-base-target/release-perf/frankenredis`

## Baseline and Profile

Baseline was built via `rch` with `release-perf` for `fr-server` and `fr-bench`,
then measured with the GETDEL hit workload:

- Initial baseline GETDEL P16/300k: `6.150s +/- 0.239`.
- Paired baseline GETDEL P16/300k: `5.123s +/- 0.093`.
- Reversed baseline GETDEL P16/300k: `5.120s +/- 0.101`.

The profile-backed target was `fr_store::estimate_entry_memory_usage_bytes`,
which was the largest server self frame in the baseline GETDEL profile:

- Baseline: `3.31%` self in
  `baseline-getdel-hit-perf-report-nochildren.txt`.
- Adjacent store/runtime rows included `Store::getdel`,
  `Store::internal_entries_remove`, and
  `Runtime::refresh_store_runtime_info_context`.

## Isomorphism

The existing Redis-observable behavior is unchanged:

- `GETDEL` still performs the same remove, type check, dirty increment, and
  returned payload conversion.
- Key ordering and hash iteration order are unchanged; the lever only updates a
  scalar cache after the entry is already removed.
- Tie-breaking does not apply.
- Floating-point and RNG state are not touched.
- Wrong-type behavior is unchanged because adjustment happens only after a
  successful entry remove.
- The cached memory value is adjusted only when the cache was exact immediately
  before this mutation. If the cache is stale or absent, the existing full-scan
  path remains responsible for refreshing it.

Golden transcript:

- Harness: `getdel_golden.py`
- SHA-256:
  `88522d7770f2995f05572dc89f71a662c9d1ce7084b2af2f2559e5023dc29b5d`
- Covered commands: `FLUSHALL`, `SET`, `GETDEL` hit, `GET` after delete,
  `GETDEL` miss, list wrongtype via `RPUSH`/`GETDEL`/`GET`.

Focused unit coverage:

- `getdel_keeps_exact_memory_cache_incremental_after_remove`
- Existing `GETDEL` hit/miss/wrongtype tests

## Benchmarks

Same harness, paired order:

- Baseline: `5.123s +/- 0.093`
- Candidate: `4.980s +/- 0.129`
- Ratio: candidate `1.03x +/- 0.03`

Same harness, reversed order:

- Candidate: `4.999s +/- 0.083`
- Baseline: `5.120s +/- 0.101`
- Ratio: candidate `1.02x +/- 0.03`

Post-change profile:

- Candidate profiled run: `150834.0 ops/sec`, p50/p95/p99
  `4142us / 11501us / 15951us`.
- `estimate_entry_memory_usage_bytes` fell from `3.31%` self to `1.74%` self in
  the no-children report.

## Score

`4.0 = Impact 2 x Confidence 2 / Effort 1`

Decision: keep. The gain is modest but repeatable in both benchmark orders, the
target was the top server self frame in the GETDEL profile, and the proof
surface is narrow.

## Next Profile Route

The shifted profile is no longer a pure command-specific borrowed-write target.
The next pass should attack the shared runtime/observability refresh path or a
deeper output/framing primitive only with fresh same-worker baseline evidence.
