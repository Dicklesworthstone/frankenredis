# frankenredis-yjedr rejection report

## Target

Post-cms7p LPUSH profile still pointed at `Store::lpush` / list-value work after
`frankenredis-63c8x` rejected per-command memory-cache deltas. The active
LPUSH P16/C50/keyspace=100k benchmark keeps lists in the packed small-list
regime, so the candidate targeted `PackedList::push_front`, not promoted
`ChunkedList` chunks.

Candidate lever: replace `PackedList::push_front`'s temporary encoded `Vec`
plus `Vec::splice(0..0, enc)` with an in-place front insert:

1. reserve once,
2. resize,
3. shift old packed bytes with `copy_within`,
4. write the varint header directly,
5. copy element bytes.

## Behavior proof

- Focused packed-list test passed through RCH:
  `cargo test -p fr-store --lib packed_set::tests::list_basic_ops_and_order -- --nocapture`.
- Existing proptest isomorphism passed through RCH:
  `cargo test -p fr-store --lib packed_set::tests::list_equivalent_to_vecdeque -- --nocapture`.
- RCH `cargo check -p fr-store --lib` passed.
- RCH release build for `fr-server` and `fr-bench` passed.
- Raw TCP golden covered list order, integer replies, wrongtype, expired-key
  rewrite, and per-key `MEMORY USAGE`.
- Golden input SHA256:
  `459126abe043c0d638b75cdf8d443f2e01f687dd0dbc6947d8f8d3e80a098aa0`.
- Baseline and candidate raw output SHA256 both:
  `8169647db39fbfe1951fc2291fd7332a77911becbf0427640146b796adb343e1`.
- Isomorphism notes: candidate produced identical packed `[varint len][bytes]`
  records at the front of the buffer, preserving order, bounds, iter/get,
  set/insert/remove/pop behavior, integer replies, lazy expiry, LFU/touch,
  RNG, tie-breaking, and floating-point behavior.

## Benchmark evidence

Baseline LPUSH P16/C50/n1M:

- Hyperfine mean: `1.442326109s +/- 0.132562410s`.
- Last report: `701891.35 ops/sec`, p50 `1013us`, p95 `1303us`, p99 `6983us`.

Candidate independent n1M:

- Hyperfine mean: `1.413046859s +/- 0.090846980s`.
- Last report: `767268.09 ops/sec`, p50 `956us`, p95 `1010us`, p99 `6419us`.

Paired n1M, baseline first:

- Baseline: `1.408460013s +/- 0.091307881s`.
- Candidate: `1.306487575s +/- 0.053564228s`.
- Result: candidate `1.08x` faster by mean.

Paired n1M, candidate first:

- Candidate: `1.400814063s +/- 0.065958934s`.
- Baseline: `1.461686479s +/- 0.147005787s`.
- Result: candidate `1.04x` faster by mean.

Paired n5M, baseline first:

- Baseline: `8.883615508s +/- 0.812812430s`.
- Candidate: `8.669542200s +/- 1.404920492s`.
- Mean ratio: candidate `1.025x` faster with overlapping variance.
- Median regressed: baseline `8.864090613s`, candidate `9.198923015s`.
- Last report regressed throughput: baseline `564270.89 ops/sec`, candidate
  `514386.09 ops/sec`.

## Decision

Rejected. The candidate does not clear Score >= 2.0:

- Impact: `1.0` for an unstable `1.025x` long-run mean and median/last-run
  regressions.
- Confidence: `0.55` because n1M wins did not survive the n5M median/last-run
  checks.
- Effort: `1.0`.
- Score: `1.0 * 0.55 / 1.0 = 0.55`.

The production source hunk was removed before commit. Evidence is retained so
the campaign does not repeat this lever.

## Next route

Stop packed-list front-insert micro-tuning. The next pass should attack a
larger primitive from the same profile family: zero-copy/borrowed list payload
storage across the parser-to-store boundary, batched LPUSH execution over a
pipeline slice, or a list-node layout that removes byte shifting entirely rather
than making the current shift path cheaper.
