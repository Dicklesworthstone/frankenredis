# frankenredis-9qbbb Optimization Report

Date: 2026-06-13T13:26:28Z
Agent: CoralOx
Bead: frankenredis-9qbbb
Decision: reject, Score < 2.0

## Target

Post-cms7p LPUSH profiling still showed `Store::lpush` and list-value work
after two smaller rejected levers (`frankenredis-63c8x` memory-cache delta and
`frankenredis-yjedr` front insertion). This pass tested one deeper but still
local lever: borrow LPUSH/RPUSH payload bytes directly into `PackedList` and
only allocate a `Vec<u8>` when `ListValue` is already promoted to deque storage.

## Candidate Lever

- Added borrowed `ListValue::push_front_bytes` and `push_back_bytes`.
- Routed `Store::lpush` and `Store::rpush` through those borrowed methods.
- Added an isomorphism unit test comparing borrowed and owned push sequences
  across packed-to-deque promotion.
- Removed the production source hunk before commit because benchmark evidence
  did not clear the Score gate.

## Behavior Proof

- RCH focused borrowed-push test passed:
  `artifacts/optimization/frankenredis-9qbbb/candidate/test-borrowed-pushes.log`
  SHA256 `e552e818c4ce359a2a6fae9df961f6a8300c107f1ab457d5a0f5af0ad895a75e`.
- RCH existing list isomorphism test passed:
  `artifacts/optimization/frankenredis-9qbbb/candidate/test-listvalue-isomorphism.log`
  SHA256 `30a094d362850fda03b84d1962f505ac9ad7bc324809f06edacf25db81ff01ac`.
- RCH crate-scoped check passed:
  `artifacts/optimization/frankenredis-9qbbb/candidate/check-fr-store-lib.log`
  SHA256 `d983247a566885fced69a155a1a2e4558277c1467d8f5740a7029f76591ea2f9`.
- RCH release builds passed:
  - Baseline `frankenredis` SHA256
    `47e43a80fdec7bd1b266b6fb778e845f0af21fb5e818734e52168068d6264afa`.
  - Baseline `fr-bench` SHA256
    `0eef34f0d0cc725426705fb7aabc0ff81e063bc35504cbafb8c2b9209fe69da4`.
  - Candidate `frankenredis` SHA256
    `c4732b1698cd1db6294d53b288e63269566257ea137275183f6e3e25bfbb8b74`.
  - Candidate `fr-bench` SHA256
    `cd4907c8c16d9256ac5d7f7478861ab1c016e71b06b0ac80ca670e72558dfd3c`.
- Raw TCP golden input SHA256:
  `459126abe043c0d638b75cdf8d443f2e01f687dd0dbc6947d8f8d3e80a098aa0`.
- Baseline and candidate raw TCP golden output SHA256 both:
  `8169647db39fbfe1951fc2291fd7332a77911becbf0427640146b796adb343e1`.
- Ordering and tie-breaking: LPUSH/RPUSH element order is preserved by the
  borrowed path and verified against owned pushes across promotion.
- Floating-point: no FP code touched.
- RNG: no RNG code touched.

## Benchmarks

Workload: `fr-bench --workload lpush --clients 50 --pipeline 16 --requests
1000000 --keyspace 100000`, server and benchmark binaries built by RCH.

- Baseline independent n1M:
  `1.465109135s +/- 0.090068749s`, median `1.467420583s`, last
  `730572.92 ops/sec`.
- Candidate independent n1M:
  `1.518353387s +/- 0.097107975s`, median `1.508140567s`, last
  `726098.39 ops/sec`.
- Paired n1M with both DBs flushed before every measurement:
  - Baseline `1.499198487s +/- 0.042113132s`, median `1.497576333s`,
    last `668981.29 ops/sec`.
  - Candidate `1.492872810s +/- 0.101154097s`, median `1.482397391s`,
    last `679375.18 ops/sec`.
  - Hyperfine summary: candidate `1.00 +/- 0.07x` faster than baseline.

## Score

Impact: 0.2
Confidence: 0.6
Effort: 1.0
Score: 0.12

The candidate removes a plausible temporary allocation but does not produce a
credible wall-clock win at the existing LPUSH gate. It is rejected under the
Score>=2.0 rule.

## Next Route

Do not repeat LPUSH per-element allocation/accounting micro-levers. The next
profile-backed primitive should change the shape of the hot path, for example
batched pipeline execution from parsed command groups into store operations,
parser-to-command-to-store borrowed payload lifetimes that remove command-frame
materialization, or a list-node layout that avoids per-element packed-buffer
shifts entirely.
