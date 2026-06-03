# frankenredis-ptqye Isomorphism Proof

## Change

Cache the cloned dispatch ACL permission snapshot per runtime when the authenticated
user and auth-state generation are unchanged. Invalidate on ACL pubsub-default,
requirepass, added user, SETUSER success, ACL LOAD success, DELUSER success, and
authenticated user switch.

## Profile Target

- Baseline profile: `artifacts/optimization/icywolf-perf-20260603-pass20-current-profile/`
- Hotspot: `HashSet<String>::clone` in `AclUser::to_dispatch_acl_permissions` via
  `Runtime::refresh_current_dispatch_client_context` on ordinary SET dispatch.
- Baseline direct SET p16: `214617.11 ops/sec`, p99 `7219 us`.
- Baseline hyperfine SET p16: `373.0 ms +/- 22.1 ms`.

## Score

- Impact: 3 (removes repeated ACL permission cloning from every dispatch refresh)
- Confidence: 3 (profile-backed hotspot, direct benchmark and p99 both improved)
- Effort: 2 (localized generation counter and cache)
- Score: `(3 * 3) / 2 = 4.5`

## Behavior Isomorphism

- Ordering preserved: yes. The cached value is only a cloned permission snapshot;
  command execution order, pubsub revocation queueing, and pending client kills are
  unchanged.
- Tie-breaking unchanged: yes. ACL root-vs-selector denial depth rules still run in
  the same order inside the copied permission snapshot.
- Floating-point: N/A.
- RNG: N/A.
- Auth mutation visibility: preserved by `dispatch_permissions_generation` bumps on
  every auth-state mutator that can affect dispatch permissions.
- User switch visibility: preserved by comparing `session.current_user_name()` to the
  cached username before each dispatch context refresh.

## Golden Evidence

- Focused runtime test:
  `cargo test -p fr-runtime acl_dispatch_permission_snapshot_cache_invalidates_on_acl_generation_and_user_switch_ptqye -- --nocapture`
- Golden trace digest asserted in test: `cf0aa84b371ae1ca`.
- Equivalent RESP trace sha256:
  `ac2358ad2940d8406417c2eacf4f96f71e1eb97ff64efa6f03bd6733fda21af8`.
- Benchmark artifact sha256 check:
  `sha256sum -c artifacts/optimization/frankenredis-ptqye/candidate-bench.sha256`
  passed for direct and hyperfine JSON artifacts.

## Benchmark Delta

- Candidate direct SET p16:
  `244764.89 ops/sec`, p99 `5667 us`.
- Direct throughput delta:
  `244764.89 / 214617.11 = 1.1405x`.
- Direct p99 delta:
  `5667 us / 7219 us = 0.7850x` (21.5% lower).
- Candidate hyperfine SET p16:
  `363.5 ms +/- 19.7 ms`.
- Hyperfine mean delta:
  `363.5 ms / 373.0 ms = 0.9745x` (2.5% lower).

## Validation

- `RCH_FORCE_REMOTE=1 CARGO_TARGET_DIR=target-icywolf-ptqye-test-rch2 rch exec -- cargo test -p fr-runtime acl_dispatch_permission_snapshot_cache_invalidates_on_acl_generation_and_user_switch_ptqye -- --nocapture`
  passed on worker `vmi1293453`.
- `RCH_FORCE_REMOTE=1 CARGO_TARGET_DIR=target-icywolf-ptqye-check-rch rch exec -- cargo check -p fr-runtime --all-targets`
  passed on worker `vmi1293453`.
- `RCH_FORCE_REMOTE=1 CARGO_TARGET_DIR=target-icywolf-ptqye-clippy-rch rch exec -- cargo clippy -p fr-runtime --all-targets -- -D warnings`
  passed on worker `vmi1293453`.
- `rustfmt --edition 2024 --check crates/fr-runtime/src/lib.rs` passed.
- `cargo fmt --check` was not clean because of pre-existing formatting drift in
  `fr-protocol`, `fr-server`, and `fr-store`; `fr-runtime` itself is formatted.

## RCH Note

The benchmark shell wrapper was rejected by `rch exec` as a non-compilation
command, so the timed run executed against the release-perf target dir named
`target-icywolf-ptqye-candidate-rch`. RCH separately launched and completed a
remote `cargo build --profile release-perf -p fr-server -p fr-bench` job for the
same candidate project while the benchmark evidence was being captured.
