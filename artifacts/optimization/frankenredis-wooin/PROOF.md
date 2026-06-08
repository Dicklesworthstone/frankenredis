# frankenredis-wooin rejection proof

## Target

- Bead: `frankenredis-wooin`
- Final pass source basis: production source equivalent to `ecbe6dfa4`; current
  HEAD `650bd5609` only added the earlier artifact closeout for this bead.
- Profile-backed workload: alternating `SETEX` / `PSETEX`, 1,000,000 requests,
  50 clients, pipeline 16, keyspace 10,000, value size 3 bytes.
- Baseline build:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-wooin-baseline-rch cargo build --profile release-perf -p fr-server -p fr-bench`
- Baseline server:
  `/data/projects/frankenredis/target-cod-wooin-baseline-rch/release-perf/frankenredis`
- Baseline hyperfine artifact:
  `baseline/setex-p16-1m-hyperfine.json`
- Baseline result: `4.744888082168571s +/- 0.09383303794878903s`.

## Fresh Profile

Fresh `perf record` on the baseline workload captured 23,671 samples with
`Total Lost Samples: 0`.

Report artifacts:

- `baseline/setex-p16-1m-perf-flat.txt`
- `baseline/setex-p16-1m-perf-children.txt`
- `baseline/last-setex-p16-1m-profile.json`

Relevant rows:

- `<std::hash::random::RandomState as BuildHasher>::hash_one::<&[u8]>`:
  2.13% flat / 2.16% children.
- `Runtime::execute_frame_internal`: 0.67% flat / 0.92% children.
- `fr_command::command_key_indexes`: 0.57% flat / 0.58% children.
- `Runtime::dispatch_with_client_context`: 0.56% flat.
- `fr_command::command_table_index`: 0.54% flat / 0.60% children.
- `core::str::from_utf8`: 0.35% flat.
- `foldhash::quality::RandomState::hash_one::<&[u8]>`: 0.27% flat.
- `acl_command_selectors_for_argv`: 0.23% flat / 0.28% children.
- `check_full_command_arity`: 0.23% flat / 0.25% children.

Profiling notes: `perf report` emitted `addr2line` sentinel noise and
kernel-symbol restriction warnings, but the user-space reports above were
generated and the sample loss counter was zero.

## Lever Tested

Alien-graveyard primitive: a fast internal hasher / Swiss-table-adjacent map
route for a non-DoS internal hot map.

Candidate hunk:

- Add `foldhash = "0.1"` to `fr-runtime`.
- Type `ServerState.client_tracking_observed_keys` as
  `HashMap<Vec<u8>, HashSet<u64>, foldhash::quality::RandomState>`.
- Initialize the map with `foldhash::quality::RandomState::default()`.
- Leave pub/sub maps, command metadata tables, external key bytes, and all
  command semantics unchanged.

The candidate source hunk was removed after benchmark rejection. No production
source change from this lever is retained.

## Behavior Proof While Candidate Was Applied

Validation:

- `cargo fmt -p fr-runtime --check` reported unrelated shared-tree formatting
  drift in runtime test blocks. The candidate alias was manually formatted and
  the peer drift was left untouched.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-wooin-check-rch cargo check -p fr-runtime --all-targets`
  passed on `vmi1153651`.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-wooin-candidate-rch cargo build --profile release-perf -p fr-server -p fr-bench`
  completed using RCH local fallback.

Golden RESP transcript:

- Comparator: `artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py`
- Baseline server:
  `/data/projects/frankenredis/target-cod-wooin-baseline-rch/release-perf/frankenredis`,
  port 26805.
- Candidate server:
  `/data/projects/frankenredis/target-cod-wooin-candidate-rch/release-perf/frankenredis`,
  port 26806.
- Artifact: `golden-compare.json`
- Baseline SHA-256:
  `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256:
  `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Bytes: 992 baseline, 992 candidate.

Isomorphism:

- Ordering/tie-breaking: unchanged. The observed-key map is a lookup/removal
  index for client tracking invalidation, not an externally iterated reply
  source. Invalidation owner IDs remain sorted before output, and BCAST key
  order follows command key order.
- Duplicate and key-order semantics: unchanged. Command key extraction,
  duplicate first-occurrence behavior, pub/sub channel handling, and
  propagation rewrite code were untouched.
- Hash collision semantics: unchanged at the Rust `HashMap` API level for this
  internal map. Only the hasher implementation changed while the candidate was
  applied.
- Floating-point behavior: untouched.
- RNG behavior: untouched.
- RESP output, TTL behavior, and propagation-visible replies: pinned by the
  matching golden transcript.

## Benchmark

Paired hyperfine artifact:
`wooin-setex-p16-1m-paired-hyperfine.json`.

- Baseline: `4.606549966265713s +/- 0.03327005247669873s`.
- Candidate: `4.638218255408573s +/- 0.16860288559408193s`.
- Hyperfine summary: baseline ran `1.01x +/- 0.04` faster than candidate.

Reversed-order hyperfine artifact:
`wooin-setex-p16-1m-reversed-hyperfine.json`.

- Candidate: `4.572880785394285s +/- 0.03690837225489876s`.
- Baseline: `4.561343114822857s +/- 0.06319433262671649s`.
- Hyperfine summary: baseline ran `1.00x +/- 0.02` faster than candidate.

## Decision

Reject under the Score>=2.0 gate.

- Impact: not a win; both paired and reversed comparisons favor baseline within
  noise.
- Confidence: high enough to reject because both orders failed on the same
  workload and golden behavior was byte-identical.
- Effort: low, but Score is 0 because the measured effect is not positive.

No source hunk is retained.

Next route: stop the client-tracking hash micro-family and attack a materially
different zero-copy or batched command packet. The next target is to remove
owned command argument materialization and repeated command metadata hashing as
a class, threading a proof-carrying packet through key extraction, ACL selector
lookup, arity/classification, dispatch, and propagation rewrite. Target ratio:
at least 1.20x on SETEX/PSETEX P16/1M before keeping.
