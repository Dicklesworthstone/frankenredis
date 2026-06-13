# frankenredis-63c8x rejection report

## Target

Post-cms7p hz1 profile at `f818387cc` on LPUSH P16/C50/n3M showed:

- `Store::lpush::<&[u8]>`: 7.27% self.
- `fr_store::estimate_value_memory_usage_bytes`: 5.43% self.
- `_mi_page_malloc_zero`: 2.22% self.
- `ListValue::push_front`: 0.49% self.

Candidate lever: update the global memory-usage cache incrementally for LPUSH
and RPUSH when the cache was exact at command entry, instead of letting the next
sample trigger a full keyspace memory recompute.

## Behavior proof

- Focused store test passed through RCH:
  `cargo test -p fr-store --lib list_push_updates_current_memory_cache_by_delta -- --nocapture`.
- RCH `cargo check -p fr-store --lib` passed.
- RCH release build for `fr-server` and `fr-bench` passed.
- Raw TCP golden covered list order, integer replies, wrongtype, expired-key
  rewrite, and per-key `MEMORY USAGE`.
- Golden input SHA256:
  `459126abe043c0d638b75cdf8d443f2e01f687dd0dbc6947d8f8d3e80a098aa0`.
- Baseline and candidate raw output SHA256 both:
  `8169647db39fbfe1951fc2291fd7332a77911becbf0427640146b796adb343e1`.
- Isomorphism notes: the source hunk touched only LPUSH/RPUSH memory-cache
  bookkeeping after the existing lazy-expiry gate. It did not alter list
  insertion order, integer replies, wrongtype branch, LFU touch rules, dirty
  increment, RNG, tie-breaking, or floating-point behavior.

## Benchmark evidence

Baseline LPUSH P16/C50/n1M fresh-state:

- Hyperfine mean: `1.352877851s +/- 0.078441799s`.
- Last report: `737628.26 ops/sec`, p50 `1019us`, p95 `1180us`, p99 `3457us`.

Candidate independent n1M:

- Hyperfine mean: `1.408017261s +/- 0.035614048s`.
- Last report: `696440.54 ops/sec`, p50 `1108us`, p95 `1340us`, p99 `1600us`.
- Result: slower than baseline.

Paired n1M, baseline first:

- Baseline: `1.516180348s +/- 0.094964329s`.
- Candidate: `1.284500303s +/- 0.039710181s`.
- Result: candidate `1.18x` faster, but order-sensitive.

Paired n1M, candidate first:

- Candidate: `1.373832662s +/- 0.054700426s`.
- Baseline: `1.460182377s +/- 0.125185125s`.
- Result: candidate `1.06x` faster, still high variance.

Paired n5M, baseline first:

- Baseline: `8.818347644s +/- 1.190698312s`.
- Candidate: `8.491984543s +/- 1.247146869s`.
- Mean ratio: candidate `1.04x` faster with overlapping variance.
- Last report regressed throughput: baseline `534694.16 ops/sec`, candidate
  `515682.75 ops/sec`.

## Decision

Rejected. The candidate does not clear Score >= 2.0:

- Impact: `1.0` for an unstable `1.04x` long-run mean and regressed last-run
  throughput.
- Confidence: `0.6` because n1M was order-sensitive and n5M variance overlaps.
- Effort: `1.0`.
- Score: `1.0 * 0.6 / 1.0 = 0.6`.

The production source hunk was removed before commit. Evidence is retained so
the campaign does not repeat this lever.

## Next route

Do not repeat per-command memory-cache delta accounting. The profile still
points at LPUSH userspace work, but this lever moves cost inside every command
and does not reliably improve throughput. Re-profile and attack a structurally
different primitive: list payload allocation/layout, listpack entry construction,
or memory sampling architecture that removes repeated full-keyspace scans without
adding per-command list estimator work.
