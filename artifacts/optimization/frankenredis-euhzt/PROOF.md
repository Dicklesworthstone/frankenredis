# frankenredis-euhzt proof

## Target

- Bead: `frankenredis-euhzt`
- Profile source: current HEAD `8c6ca0a5c`, clean detached worktree, RCH release-perf build.
- Workload: alternating `SETEX` / `PSETEX`, 1,000,000 requests, 50 clients, pipeline 16.
- Profile artifact: `artifacts/optimization/frankenredis-pass75-current-profile/perf-report-flat.txt`

Top relevant profile rows:

- `std::hash::RandomState::hash_one::<&[u8]>`: 8.46% flat, 9.23% children
- `fr_command::command_key_indexes`: 1.19% flat, 2.26% children
- `fr_runtime::Runtime::queue_client_tracking_invalidations`: 0.43% flat
- `HashMap<Vec<u8>, HashSet<u64>>::remove::<[u8]>`: present in the profile

Lever tested: early-return from `Runtime::queue_client_tracking_invalidations` when both `client_tracking_observed_keys` and `client_tracking_bcast_clients` are empty. This skips per-write key hashing in the default no-CLIENT-TRACKING case.

## Isolation Note

An initial candidate benchmark from the shared checkout is recorded but ignored because unrelated `frankenredis-7tpx0` runtime edits appeared in `crates/fr-runtime/src/lib.rs` during validation. The final keep/reject decision uses a clean candidate worktree at `8c6ca0a5c` with only the euhzt guard applied:

```text
/data/projects/.scratch/frankenredis-euhzt-candidate-8c6ca0a5c-20260608T1030
```

No source hunk is retained in the shared checkout for this rejected lever.

## Behavior Isomorphism

- Golden transcript artifact: `golden-compare.json`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Bytes: 992 baseline, 992 candidate
- Equality: true

The SETEX/PSETEX/PTTL/PERSIST transcript bytes are identical. Ordering, DB selection, TTL/PERSIST/lazy-expiry behavior, active-expire due-key fallback, tie-breaking, floating-point behavior, and RNG behavior are unchanged.

The guard is behavior-preserving by construction: when both the observed-key map and BCAST-client set are empty, the old path could not enqueue any invalidation, remove any observer entry, or publish any client-tracking message. Existing focused tests also preserve enabled tracking behavior:

- RCH `cargo test -p fr-runtime client_tracking -- --nocapture`: 9 tests passed on `vmi1156319`
- `cargo test -p fr-runtime tracking -- --nocapture`: 14 tests passed; this broader run fell back locally through `rch`

## Validation

- `cargo fmt -p fr-runtime --check`
- RCH `cargo check -p fr-runtime --all-targets` on `vmi1156319`
- RCH `cargo test -p fr-runtime client_tracking -- --nocapture` on `vmi1156319`
- RCH/local fallback `cargo test -p fr-runtime tracking -- --nocapture`
- RCH `cargo clippy -p fr-runtime --all-targets -- -D warnings` on `vmi1167313`
- RCH clean candidate release build on `vmi1156319`

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass75-profile-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1167313
```

Clean candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-euhzt-clean-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1156319
```

Standalone baseline, P16/1M:

- Baseline: `4.4187890669s +/- 0.0553647863s`
- Artifact: `baseline-setex-p16-1m-hyperfine.json`

Clean paired P16/1M:

- Baseline: `4.4972667007s +/- 0.0244240857s`
- Candidate: `4.4742455843s +/- 0.0275518635s`
- Ratio: candidate `1.01x +/- 0.01` faster
- Artifact: `euhzt-clean-setex-p16-1m-paired-hyperfine.json`

Clean reversed P16/1M:

- Candidate: `4.4526088730s +/- 0.0391337749s`
- Baseline: `4.4740804867s +/- 0.0486954105s`
- Ratio: candidate `1.00x +/- 0.01` faster
- Artifact: `euhzt-clean-setex-p16-1m-reversed-hyperfine.json`

## Decision

Reject under the Score>=2.0 keep gate.

- Impact: 1
- Confidence: 0
- Effort: 1
- Score: 0.0 because the measured effect is only `1.00x` to `1.01x` and below the campaign's real-win threshold.

Do not retry no-observer client-tracking invalidation guards as a standalone lever. The next pass must attack a deeper shifted primitive from the same profile, likely the broader command metadata/hash family (`std::hash::RandomState::hash_one`, `command_key_indexes`, `command_table_index`, `acl_command_selectors_for_argv`) or output syscall cost, with a materially different structure and fresh baseline.
