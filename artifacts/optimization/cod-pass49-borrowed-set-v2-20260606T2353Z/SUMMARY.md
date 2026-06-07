# Pass 49: Borrowed Exact SET Fast Path Rebase Rejection

Bead: `frankenredis-ohsk5`

Lever tested: route the strict multibulk `SET key value` hot path from
`fr-server` directly into a conservative borrowed runtime entry point. The
fast path avoids building `Vec<Vec<u8>>` argv and bypasses generic command
dispatch only when visible side channels are disabled.

Final decision after rebase: rejected. Remote parent `5ce3f7231` already
contains `352b97906 perf: fast-path plain SET dispatch`, which absorbed the
large pre-rebase win. The additive exact-SET/raw-OK shortcut did not clear the
Score >= 2.0 gate against the new parent, so no production code from this lever
is retained.

## Profile Target

Pre-change SET P16/1M profile on clean HEAD `60eacd9e0`:

- `RandomState::hash_one::<&[u8]>`: 8.73%
- `Runtime::execute_frame_internal`: 3.92%
- `Runtime::execute_dispatch`: 3.92%
- `SipHasher::write`: 2.42%
- `dispatch_with_client_context`: 2.28%
- `Store::internal_entries_insert`: 1.63%
- `parse_command_args_borrowed_into`: 1.39%
- `fr_command::set`: 0.89%

## Benchmarks

Baseline build: `target-cod-pass49-clean-baseline` via RCH.
Candidate build: `target-cod-pass49-borrowed-set-release-v2` via RCH.

- Baseline pre-change SET P16/300k: 0.9567925983s +/- 0.0105071179.
- Paired SET P16/300k:
  - baseline: 0.9436540444s +/- 0.0208909137
  - candidate: 0.7000215882s +/- 0.0157093562
  - result: candidate 1.35x +/- 0.04 faster
- Reversed-order SET P16/1M:
  - candidate first: 1.7225155401s +/- 0.0982520827
  - baseline second: 2.6280524214s +/- 0.1316216661
  - result: candidate 1.53x +/- 0.12 faster

Pre-rebase score was 10.0 = Impact 4.0 x Confidence 5.0 / Effort 2.0.

Rebase gate against parent `5ce3f7231`:

- Parent build: `target-cod-pass49-rebase-baseline` via RCH from detached
  worktree `/data/projects/.scratch/frankenredis-pass49-rebase-baseline-20260607T0016Z`.
- Candidate build: `target-cod-pass49-rebase-candidate` via RCH.
- Paired SET P16/300k:
  - parent: 0.7399418305s +/- 0.0247442159
  - candidate: 0.7331347502s +/- 0.0324308062
  - result: candidate 1.01x +/- 0.06 faster
- Reversed-order SET P16/1M:
  - candidate first: 1.8217507197s +/- 0.0756367129
  - parent second: 1.9052516868s +/- 0.1347439838
  - result: candidate 1.05x +/- 0.09 faster

Rebase score: below 2.0. The candidate is rejected and production source was
restored to the parent plain-SET implementation.

## Proof

- Golden input sha256:
  `e7a11a6135058dd81b9593b9002c5d93469ed8d1f26b1838fcb165749c5d0f04`
- Baseline output sha256:
  `5c82044b4b0062c0db526300576dcf15087e4d8c64f07c6fc01965df18100508`
- Candidate output sha256:
  `5c82044b4b0062c0db526300576dcf15087e4d8c64f07c6fc01965df18100508`
- Output size: 33 bytes for baseline and candidate.
- `cmp -s` passed.

The runtime test `borrowed_set3_fast_path_matches_generic_set_core_state`
compares store state, counters, command histogram, session command metadata,
and mixed-case slowlog argv preservation against generic dispatch.

## Validation

- `rch exec -- cargo check -p fr-runtime -p fr-server --all-targets`
- `rch exec -- cargo test -p fr-runtime borrowed_set3_fast_path -- --nocapture`
- `rch exec -- cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings`
- `rch exec -- env CARGO_TARGET_DIR=target-cod-pass49-borrowed-set-release-v2 cargo build --release -p fr-server -p fr-bench`
- `rch exec -- cargo test -p fr-conformance --lib core_set -- --nocapture`
- `cargo fmt -p fr-runtime -p fr-server`
- `cargo fmt -p fr-runtime -p fr-server --check`
- Rebase validation after removing rejected code:
  - `cargo fmt -p fr-runtime -p fr-server --check`
  - `rch exec -- cargo test -p fr-runtime plain_set_borrowed_fast_path -- --nocapture`
  - `rch exec -- cargo clippy -p fr-runtime -p fr-server --all-targets -- -D warnings`
  - `rch exec -- cargo test -p fr-conformance --lib core_set -- --nocapture`
  - `rch exec -- cargo check -p fr-runtime -p fr-server --all-targets`

Full `cargo test -p fr-conformance -- --nocapture` passed the conformance
library suite, including live core SET parity, then failed an unrelated
`live_oracle_orchestrator::run_fingerprint_is_stable` bin-unit fingerprint
expectation. No `fr-conformance` files were changed for this lever.

`ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs` returned the
existing file-wide inventory: exit 1, 2 files scanned, 273 critical, 4057
warning, and 774 info items. Embedded fmt, check, clippy, and test-build gates
inside UBS were clean; full UBS output is recorded in
`ubs-touched-files.txt`.

## Candidate Reprofile Before Rebase Rejection

Candidate SET P16/1M post-profile:

- elapsed: 1440 ms
- throughput: 693986.40 ops/sec
- latency: p50 1078us, p95 1507us, p99 1938us, p999 2671us
- lost perf samples: 0
- top flat symbols:
  - `RandomState::hash_one::<&[u8]>`: 8.31%
  - `Runtime::refresh_store_runtime_info_context`: 6.28%
  - vdso/time path: 5.28%
  - `SipHasher::write`: 1.61%
  - `parse_command_args_borrowed_into`: 1.56%
  - `Runtime::try_execute_borrowed_set3`: 1.55%
  - `Store::internal_entries_insert`: 1.39%

Next target: replace per-command runtime-info refresh/hash/time work with a
batch-scoped or dirty-bit primitive, not another command metadata micro-gate or
another additive exact-SET shortcut.
