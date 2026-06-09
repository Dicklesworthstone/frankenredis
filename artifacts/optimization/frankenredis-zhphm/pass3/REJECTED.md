# Pass 3 Rejection: Client Memory Aggregate Delta Update

Bead: `frankenredis-zhphm.2`

## Profile-backed target

After `frankenredis-zhphm.1`, `perf report` on SET/P16/1M showed
`Runtime::refresh_client_memory_aggregates` at 4.87% self. The attempted lever
replaced per-session-record full aggregate recomputation with a remembered
normal/replica memory bucket and O(1) subtract/add update.

## Behavior proof

- Focused RCH test passed:
  `cargo test -p fr-runtime record_client_session_updates_client_memory_aggregates_by_delta -- --nocapture`
- Crate-scoped RCH check passed:
  `cargo check -p fr-runtime --all-targets`
- Crate-scoped RCH clippy for edited crate passed:
  `cargo clippy -p fr-runtime --all-targets --no-deps -- -D warnings`
- Broad `cargo clippy -p fr-runtime --all-targets -- -D warnings` was blocked
  by an unrelated dependency lint in `crates/fr-command/src/lua_eval.rs`.
- Golden RESP transcript matched exactly:
  baseline/candidate sha256
  `bdcab17275344bc6c13913945fd5cad11c800de1108c6f77776e16b1a09cc1f4`.

Isomorphism: command ordering, reply bytes, tie-breaking, floating-point, and
RNG behavior are unaffected; the candidate only changed internal accounting of
already-recorded `ClientSession` memory totals. The focused unit test covered
normal update, replica update, normal-to-replica bucket transition, and
disconnect subtraction.

## Benchmark evidence

SET/P16/1M, release-perf fr-server/fr-bench built via RCH.

- Standalone baseline: 1.756033s +/- 0.350516s
- Standalone candidate: 1.192786s +/- 0.018928s
- Paired baseline -> candidate:
  - baseline 1.217949s +/- 0.049879s
  - candidate 1.852694s +/- 0.399192s
  - baseline 1.52x +/- 0.33 faster
- Reversed candidate -> baseline:
  - candidate 1.295227s +/- 0.028804s
  - baseline 1.408120s +/- 0.155167s
  - candidate 1.09x +/- 0.12 faster

## Decision

Rejected. The paired and reversed evidence is contradictory and does not clear
the Score >=2.0 confidence gate. Production source changes were removed.

Next profile-backed target should move deeper into the post-pass2 I/O frontier:
safe thread-pool read/parse/write offload or another structural batching
primitive from the `frankenredis-zhphm` io-threads parent, not another
`refresh_client_memory_aggregates` micro-lever.
