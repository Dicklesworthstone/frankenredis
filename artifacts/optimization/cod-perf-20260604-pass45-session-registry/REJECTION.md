# pass45 session registry clone rejection

Bead: `frankenredis-yaxr7.3`

Profile target:
- Post-BCAST SET `-P16` profile showed `ClientSession::clone` at 1.36% self
  and `BTreeMap<u64, ClientSession>::insert` at 0.42% self on the hot path.

Lever tested:
- Removed the per-command active-session clone/insert from
  `Runtime::execute_dispatch`.
- Added a focused runtime unit test proving `CLIENT LIST ID <self>` still
  overlays `self.session` when the registry has no current-client snapshot.
- The source/test hunk was reverted after benchmarking because the score was
  below the keep threshold.

Baseline:
- `target-cod-pass45-session-baseline-rch/release-perf/frankenredis`
- SET `-P16`, 50 clients, 500k requests:
  `2.246 s +/- 0.278 s`

Candidate:
- `target-cod-pass45-session-candidate-rch/release-perf/frankenredis`
- Same SET `-P16` harness:
  `2.670 s +/- 0.251 s`

Paired same-window confirmation:
- Baseline server: `2.815 s +/- 0.174 s`
- Candidate server: `2.731 s +/- 0.221 s`
- Hyperfine summary: candidate `1.03 +/- 0.10` times faster.

Decision:
- Reject. The paired result is within noise and scores below 2.0:
  Impact 1.0 x Confidence 0.3 / Effort 1.0 = 0.3.

Behavior proof:
- Golden raw RESP transcript SHA256 matched exactly:
  `7ce1863833dbd3835c324975109bc7074d043034f34b8ee1e742af621f5b854d`
- Transcript covers `PING`, `CLIENT REPLY SKIP`, suppressed `PING`,
  `CLIENT REPLY ON`, `CLIENT TRACKING ON BCAST PREFIX sreg:`,
  BCAST invalidation ordering for `SET sreg:1`, `CLIENT TRACKINGINFO`,
  `CLIENT TRACKING OFF`, `SET sreg:2`, `GET sreg:1`, and `RESET`.
- Ordering/tie-breaking preserved by identical transcript bytes.
- Floating-point and RNG behavior untouched.

Next deeper primitive:
- Do not repeat the same removal-only lever. The next runtime-session attack
  should split heavyweight `ClientSession` storage from lightweight client
  visibility metadata (`ClientSnapshot`/SoA counters updated once per event-loop
  batch), so `CLIENT LIST`/tracking/memory stats can avoid cloning transaction,
  ACL, and cluster state on hot writes.
