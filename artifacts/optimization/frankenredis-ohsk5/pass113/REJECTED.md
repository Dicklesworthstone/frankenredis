# Pass 113 Reject: Nonnumeric SET Canonicalization Bypass

## Target

- Bead: `frankenredis-ohsk5.11`
- Baseline: current `origin/main` `46b3a9b3a`, release-perf `fr-server` + `fr-bench`.
- Baseline SET/P16/C50/1M hyperfine: `906.2 ms +/- 34.2 ms`.
- Baseline SET/P16/C50/3M profile: `1311240.5925841355 ops/sec`, p50 `569us`, p95 `884us`, p99 `1216us`, 3008 perf samples, 0 lost.
- Top relevant profile rows:
  - `fr_store::canonical_string_value_from_slice`: `10.46%` self
  - `Runtime::execute_plain_set_borrowed`: `0.88%` self / `4.73%` children
  - `Store::set_plain_borrowed`: `1.68%` self / `2.54%` children
  - `process_buffered_frames`: `0.95%` self / `1.94%` children
  - `plain_borrowed_default_key_write_allows`: `1.03%` self / `1.70%` children
  - writer threads `__send`: `19.76%` and `18.66%` children

## Decision

Rejected without a source hunk.

The named bead targeted a nonnumeric-value bypass before Redis integer
canonicalization. The fresh profile confirms value canonicalization remains hot,
but the actual `fr-bench` SET workload with `--datasize 3` writes `value_for_request`
payloads. Only request indices `0..99` remain nonnumeric (`xx0` through `x99`);
from request index `100` onward the payload is a decimal suffix because
`suffix.len() >= template.len()`. Across SET/P16/C50/3M, a nonnumeric bypass would
cover only about `5,000 / 3,000,000 = 0.17%` of writes.

This makes the proposed lever too narrow for the measured hot path. It also
overlaps already rejected canonicalization families:

- pass105 first-byte integer-candidate gate: proof-clean, benchmark tie;
- pass106 borrowed `SmallStr` canonicalization: proof-clean, benchmark tie;
- pass107 lazy borrowed SET integer classification: proof-clean, benchmark
  rejection;
- pass110 positive short integer parse fast path: reversed hyperfine failed.

## Score

- Impact: `0.2`
- Confidence: `4.0`
- Effort: `1.0`
- Score: `0.8`, below the `>=2.0` keep gate.

## Isomorphism Proof

No production source changed.

- Ordering preserved: yes, no command execution or output path changed.
- Tie-breaking unchanged: yes, no ordered data structure comparator changed.
- Floating-point: N/A.
- RNG seeds: unchanged.
- Golden outputs: no candidate binary was produced because no source hunk passed
  the opportunity gate.

If revisited, the proof must preserve Redis integer object encoding for `0`,
positive/negative canonical integers, overflow rejection, empty strings, leading
zeros, raw string object encoding, digest equality, expiry clearing, LFU/LRU
metadata, and SET/GET transcript bytes.

## Next Route

Stop SET integer-classification variants. The same fresh profile exposes a
larger post-writer-pool frontier in writer-thread `__send` cost, while main-thread
key/hash/probe rows are below the rejected canonicalization family. The next
profile-backed primitive should attack safe writer-side syscall coalescing or
buffer batching, preserving per-client reply order and byte-identical transcripts.
