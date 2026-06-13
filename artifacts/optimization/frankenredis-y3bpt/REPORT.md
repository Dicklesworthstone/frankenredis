# frankenredis-y3bpt Optimization Report

Date: 2026-06-13T13:45:27Z
Agent: CoralOx
Bead: frankenredis-y3bpt
Decision: reject, Score < 2.0

## Target

Post-cms7p LPUSH P16/C50 profiling showed parser/dispatch overhead after the
list-store micro-levers failed: `fr_protocol::parse_command_args_borrowed_into`
was about 1.85% self and `parse_borrowed_multibulk_action` about 0.70%, with
allocator/syscall time still visible. This pass tested a structural
parser-to-store shortcut for the profiled single-value `LPUSH` shape.

## Candidate Lever

- First attempted to reuse a `Vec<&[u8]>` borrowed argv scratch across the
  buffered processing loop. The borrow checker correctly rejected this because
  the vector element lifetime would be tied to `conn.read_buf` across later
  mutable connection operations.
- Replaced that with the viable safe-Rust form: a direct single-value
  `SADD|LPUSH|RPUSH key value` packet parser ahead of the generic borrowed argv
  parser, matching the existing direct GET/SET packet-parser pattern.
- The direct parser used the existing strict bulk parser and fell back to the
  generic path for multi-value writes, unrelated commands, malformed frames, and
  parser-limit errors.
- Removed the production source hunk before commit because benchmark evidence
  did not clear the Score gate.

## Behavior Proof

- RCH `cargo check -p fr-server --bin frankenredis` passed:
  `artifacts/optimization/frankenredis-y3bpt/candidate/check-fr-server-bin.log`
  SHA256 `45e9c28d7df7b387567fc521bbb7982f6849d23480b87175138ea791f5ec4494`.
- RCH focused parser tests passed:
  `artifacts/optimization/frankenredis-y3bpt/candidate/test-keyed-value-parser.log`
  SHA256 `9cce027424f3d44946a2db744235677d9e82e8da8768d0bc0e9896d643ee2953`.
- `cargo fmt -p fr-server -- --check` passed.
- RCH release builds passed:
  - Baseline `frankenredis` SHA256
    `c58c9d2b23463f47c5b56de512b30f30b96d4ef964a8824b1c2445bc456b8ee3`.
  - Baseline `fr-bench` SHA256
    `b17de30718472185c8c262ce2597aad56610a31d57fea5f5413db22737c48ba7`.
  - Candidate `frankenredis` SHA256
    `f77117b9f5fb824d40c6a52ca1bbeaa9d9234ddf7f93adf79d74c804f6ee8e5d`.
  - Candidate `fr-bench` SHA256
    `1d3d84ff97988d86f6955907ba8e93a1adac88ec0fb08325a0712f0b3482cd23`.
- Raw TCP golden input SHA256:
  `459126abe043c0d638b75cdf8d443f2e01f687dd0dbc6947d8f8d3e80a098aa0`.
- Baseline and candidate raw TCP golden output SHA256 both:
  `8169647db39fbfe1951fc2291fd7332a77911becbf0427640146b796adb343e1`.
- Ordering/tie-breaking: the direct packet parser only changed how a
  single-value keyed write reached the same borrowed runtime fast path; command
  order, output order, and fallback ordering were unchanged.
- Floating-point: no FP code touched.
- RNG: benchmark RNG only; server RNG path not touched.

## Benchmarks

Workload: `fr-bench --workload lpush --clients 50 --pipeline 16 --keyspace
100000`, server and benchmark binaries built by RCH.

- Baseline independent n1M:
  `1.592692369s +/- 0.144715850s`, median `1.578310486s`, last
  `525796.36 ops/sec`.
- Candidate independent n1M:
  `1.481751134s +/- 0.052754462s`, median `1.473500341s`, last
  `681965.09 ops/sec`.
- Paired n1M with both DBs flushed before every measurement:
  - Baseline `1.597910123s +/- 0.055244649s`, median `1.579998366s`,
    last `643685.56 ops/sec`.
  - Candidate `1.568042756s +/- 0.067412463s`, median `1.541981688s`,
    last `598116.30 ops/sec`.
  - Hyperfine summary: candidate `1.02 +/- 0.06x` faster than baseline.
- Paired n5M with both DBs flushed before every measurement:
  - Baseline `9.977132805s +/- 1.359536001s`, median `10.594149178s`,
    last `457742.05 ops/sec`.
  - Candidate `9.644034881s +/- 1.234231366s`, median `10.094685404s`,
    last `477357.46 ops/sec`.
  - Hyperfine summary: candidate `1.03 +/- 0.19x` faster than baseline.

## Score

Impact: 0.4
Confidence: 0.55
Effort: 1.0
Score: 0.22

The candidate is plausible but not a credible wall-clock win at the existing
LPUSH gates. The source hunk was removed under the Score>=2.0 rule.

## Next Route

Do not repeat single-command parser shims. The next profile-backed primitive
should change batch shape rather than command recognition: parse an entire
pipeline slice into a compact command group, execute repeated same-shape writes
with one batch-level dispatch, or move reply construction into a per-readable
batch arena/output segment so parser, dispatch, and output overhead are reduced
as a class.
