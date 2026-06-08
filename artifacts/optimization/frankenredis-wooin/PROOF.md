# frankenredis-wooin rejection proof

## Target

- Bead: `frankenredis-wooin`
- Profile basis: the SETEX/PSETEX P16/1M profile still showed repeated command metadata work:
  `RandomState::hash_one::<&[u8]>` 8.46% flat, `command_table_index` 1.74% flat /
  3.21% children, `command_key_indexes` 1.19% flat / 2.26% children, and
  `acl_command_selectors_for_argv` 0.48% flat / 2.02% children.
- Prior guardrail: `frankenredis-ferss` already rejected the final
  `command_key_indexes` fallback lookup replacement at about 1.00x, so this pass did
  not retry that micro-lever.

## Lever tested

Top-level runtime ACL fast path for the fully permissive root user:

- add an `AclUser` predicate for `+@all ~* &*` with no explicit command/category denies;
- return `None` from `Runtime::acl_permission_error` before allocating/lowercasing ACL
  selectors when that predicate is true;
- fall back to the existing ACL selector/key/channel logic for any restrictive state.

The candidate source hunk and its candidate-only tests were removed after benchmark rejection.
No production source change from this lever is retained.

## Behavior proof while candidate was applied

Focused crate-scoped tests:

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-wooin-test-target cargo test -p fr-runtime all_access_acl_ -- --nocapture`
  - 2 passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-wooin-test-target cargo test -p fr-runtime acl_per_command_deny_specific_commands -- --nocapture`
  - passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-wooin-test-target cargo test -p fr-runtime acl_selectors_parse_render_and_enforce_additively -- --nocapture`
  - passed on `vmi1167313`.

Golden RESP transcript:

- Comparator: `artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py`
- Baseline server: `/tmp/codex-fr-wooin-baseline-target/release-perf/frankenredis`, port 19982.
- Candidate server: `/tmp/codex-fr-wooin-candidate-target/release-perf/frankenredis`, port 19983.
- Artifact: `candidate/resp-golden-compare.json`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Bytes: 992 baseline, 992 candidate.

Isomorphism:

- Ordering/tie-breaking: unchanged; the candidate only skipped ACL metadata work when the
  root permission set already grants every command, key, and channel.
- Error precedence: unchanged for unknown and wrong-arity commands under the all-access user;
  both previously reached dispatch after the ACL gate, and the focused tests pinned that.
- Restrictive ACL behavior: unchanged; explicit command denies and selector/key fallback tests
  exercised the original path.
- Floating-point/RNG: not touched by the lever.
- Propagation bytes and TTL behavior: golden SETEX/PSETEX transcript matched exactly.

Formatting note:

- `cargo fmt -p fr-runtime --check` currently reports pre-existing formatting drift in
  unrelated MONITOR/CONFIG/ACL test blocks; the rejected candidate hunk itself was removed.

## Benchmark

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-wooin-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-wooin-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Paired hyperfine artifact: `candidate/setex-p16-1m-paired-hyperfine.json`

Workload:

- alternating `SETEX` / `PSETEX`
- 1,000,000 requests
- 50 clients
- pipeline 16
- keyspace 1,000,000
- value size 3 bytes
- 5 measured runs per side

Results:

- Baseline: `4.7614001335400005 s +/- 0.07191982312982163`
- Candidate: `4.910629181140001 s +/- 0.2414928371838538`
- Hyperfine summary: baseline ran `1.03x +/- 0.05` faster than candidate.

## Decision

Reject under the Score>=2.0 gate.

- Impact: negative on the paired benchmark.
- Confidence: high enough to reject because the candidate was slower on the same workload and
  the prior campaign already recorded similar ACL fast-path reversals under longer 1M confirmation.
- Effort: low, but Score is 0 because the measured effect is not a win.

Next route: stop ACL/metadata micro-skips and attack a structurally different primitive from the
same profile family: a zero-copy RESP frame or arena/slab command packet that avoids repeated
owned `Vec<Vec<u8>>` materialization and metadata hashing as a class, target ratio at least 1.20x
on SETEX/PSETEX P16/1M before keeping.
