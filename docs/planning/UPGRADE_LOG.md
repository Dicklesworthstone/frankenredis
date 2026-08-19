# Dependency Upgrade Log

**Date:** 2026-04-21, 2026-04-22  |  **Project:** frankenredis  |  **Language:** Rust
**Agents:** Clawdstein-libupdater-frankenredis, codex-frankenredis

## asupersync

**Not applicable for this workspace right now**.

- `frankenredis` is not running a Tokio/Hyper/Axum/Tonic async stack that
  would benefit from an Asupersync runtime swap. A workspace grep found no
  `tokio`, `hyper`, `axum`, `tonic`, `reqwest`, `async-std`, or `smol`
  dependencies.
- The server boundary is already a custom `mio` event loop in
  `crates/fr-server/Cargo.toml`, and the runtime/persistence path is built
  around FrankenRedis-specific orchestration in `fr-runtime`/`fr-persist`
  rather than general-purpose async task scheduling.
- Adding `asupersync = "0.3.1"` now would be an architectural rewrite, not a
  dependency modernization step, and there is no narrow seam where it could be
  introduced without redesigning the runtime contract.

Result: **did not add `asupersync`**; documented here instead, per request.

Specific assessment for the requested durability candidates:

- `fr-persist` is a synchronous encoding/decoding crate with no async runtime
  seam to replace, so `asupersync` would add dependency surface without helping
  persistence correctness or throughput.
- `fr-runtime` is already tied to FrankenRedis’s custom event-loop/runtime
  contract rather than Tokio-style task orchestration. Swapping in
  `asupersync = "0.3.1"` there would require redesigning how command dispatch,
  persistence capture, and replica propagation are driven, which is far beyond
  a dependency update.

## Inventory

External dependencies (non-path) declared in `crates/*/Cargo.toml`:

| Crate | Dep | Declared | Latest | Action |
|-------|-----|----------|--------|--------|
| fr-bench | hdrhistogram | 7.5.4 | 7.5.4 | up-to-date |
| fr-bench | serde | 1.0.228 | 1.0.228 | up-to-date |
| fr-bench | serde_json | 1.0.149 | 1.0.149 | up-to-date |
| fr-command (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fr-conformance | serde | 1.0.228 | 1.0.228 | up-to-date |
| fr-conformance | serde_json | 1.0.149 | 1.0.149 | up-to-date |
| fr-conformance | sha2 | 0.10.9 | 0.11.0 | **BUMP** major 0.10 -> 0.11 |
| fr-persist (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fr-protocol (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fr-runtime | hex | 0.4 | 0.4.3 | **BUMP** exact latest |
| fr-runtime | libc | 0.2.185 | 0.2.185 | up-to-date |
| fr-runtime | sha2 | 0.11.0 | 0.11.0 | up-to-date |
| fr-runtime (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fr-sentinel (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fr-server | mio | 1.0 | 1.2.0 | **BUMP** exact latest |
| fr-server | tikv-jemallocator | 0.6 | 0.6.1 | **BUMP** exact latest |
| fr-server | mimalloc | 0.1 | 0.1.49 | **BUMP** exact latest |
| fr-store | libc | 0.2.184 | 0.2.185 | **BUMP** align with fr-runtime |
| fr-store (dev) | proptest | 1 | 1.11.0 | **BUMP** exact latest |
| fuzz | libfuzzer-sys | 0.4 | 0.4.12 | **BUMP** exact latest |
| fuzz | arbitrary | 1 | 1.4.2 | **BUMP** exact latest |

## Summary

- **Updated:** 3 dep specs (+ one bulk lock refresh covering 13 deps)
- **Skipped / already-latest:** hdrhistogram, serde, serde_json, hex,
  tikv-jemallocator, libc (fr-runtime), sha2 (fr-runtime)
- **Failed:** 0
- **Circuit breaker:** No — clean finish.

First library-updater commit: `8006c7e` (libc alignment)
Latest library-updater commit: `d12f79d` (final dependency audit state)

## 2026-04-22 exhaustive exact-spec normalization

- Normalized the remaining broad version specs to the current crates.io stable
  releases across 8 manifests:
  - `proptest = "1.11.0"` in `fr-command`, `fr-persist`, `fr-protocol`,
    `fr-runtime`, `fr-sentinel`, and `fr-store`
  - `hex = "0.4.3"` in `fr-runtime`
  - `mio = "1.2.0"`, `tikv-jemallocator = "0.6.1"`,
    `mimalloc = "0.1.49"` in `fr-server`
  - `libfuzzer-sys = "0.4.12"` and `arbitrary = "1.4.2"` in `fuzz`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo update`
  completed with **no Cargo.lock delta**: `Locking 0 packages to latest compatible versions`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo check --workspace --all-targets`
  passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo test --workspace -- --nocapture`
  is still red only on two pre-existing `fr-command` tests in the shared
  worktree:
  - `tests::object_freq_and_idletime_require_exact_arity_before_other_paths`
  - `tests::wrong_subcommand_arity_formats_redis_families_with_expected_wording`

This pass changed manifest specs only; it did not introduce any new compile
failures.

## 2026-04-22 workspace dependency unification

- Added a root [workspace.dependencies] table in [Cargo.toml] and rewired the
  `fr-*` manifests plus `fuzz/Cargo.toml` to consume the exact stable external
  versions from one place instead of repeating per-crate literals.
- Centralized external pins:
  - `arbitrary = 1.4.2`
  - `hdrhistogram = 7.5.4`
  - `hex = 0.4.3`
  - `libc = 0.2.185`
  - `libfuzzer-sys = 0.4.12`
  - `mimalloc = 0.1.49`
  - `mio = 1.2.0`
  - `proptest = 1.11.0`
  - `serde = 1.0.228`
  - `serde_json = 1.0.149`
  - `sha2 = 0.11.0`
  - `tikv-jemallocator = 0.6.1`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo update`
  again completed with **no Cargo.lock delta**.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo check --workspace --all-targets`
  passed again.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo test --workspace -- --nocapture`
  remained red only on the same pre-existing `fr-command` failures:
  - `tests::object_freq_and_idletime_require_exact_arity_before_other_paths`
  - `tests::wrong_subcommand_arity_formats_redis_families_with_expected_wording`

This unification pass changed dependency declaration layout only. It did not
change the runtime/test failure surface.

## 2026-04-22 explicit revalidation command

- Re-ran the user-requested command exactly:
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo update --workspace`
    - result: no-op, `Locking 0 packages to latest compatible versions`
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo check --workspace`
    - result: currently blocked by an unrelated shared-worktree compile error in
      [crates/fr-runtime/src/lib.rs](/data/projects/frankenredis/crates/fr-runtime/src/lib.rs:984):
      `AclUser::new_default()` is being called without the required
      `AclPubsubDefault` argument after another in-flight change to that API.

This failure is not caused by the dependency normalization pass; it is a
concurrent code change in `fr-runtime`.

## 2026-04-22 final dependency audit state

- Remaining direct external dependency gaps: **none**.
- Remaining non-centralized external version literals across `Cargo.toml`,
  `crates/fr-*/Cargo.toml`, and `fuzz/Cargo.toml`: **none**.
- `asupersync` is intentionally absent from `Cargo.lock` because it was
  evaluated and rejected for both `fr-persist` and `fr-runtime`; no manifest
  references it, so it should not appear in the lockfile.
- The only blocker left on the exact user-requested verification command is the
  unrelated shared-worktree `fr-runtime` compile error documented above.

## 2026-04-22 exact rerun requested by user

- Re-ran `cargo update --workspace` locally at the repo root:
  - result: still a no-op, `Locking 0 packages to latest compatible versions`
- Re-ran `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo check --workspace`
  exactly as requested:
  - result: still blocked by the same unrelated shared-worktree compile error in
    `crates/fr-runtime/src/lib.rs:984`
  - error: `AclUser::new_default()` is missing the required
    `AclPubsubDefault` argument after another in-flight change to that API

No additional dependency bumps remain to apply. The dependency state is fully
normalized; the remaining failure is outside the library-updater scope.

## 2026-04-22 cod-redis rerun: local update + fr-persist asupersync check

- Re-ran the requested local command again at `/data/projects/frankenredis`:
  - `cargo update --workspace`
  - result: still a no-op, `Locking 0 packages to latest compatible versions`
- Re-evaluated `asupersync = "0.3.1"` specifically for `fr-persist`:
  - `asupersync` remains intentionally absent from `Cargo.lock`
  - `crates/fr-persist` is synchronous AOF/RDB encode-decode logic, not a
    Tokio-style async runtime boundary
  - adding `asupersync` there would increase dependency surface without
    changing durability behavior, because there is no async scheduling seam for
    it to replace
- Result: **did not add `asupersync` to `fr-persist`**. The skip is deliberate,
  not an omission.
- Re-ran the requested remote verification command:
  - `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_frankenredis_cod cargo check --workspace`
  - result: **passed**
  - note: shared worktree still emits two unrelated `fr-runtime` warnings for
    unused `acl_pubsub_default` locals at `crates/fr-runtime/src/lib.rs:904`
    and `:920`, but there is no workspace compile failure from the dependency
    state.

## 2026-04-22 cod-redis cooling rerun

- Re-ran the exact local maintenance command from the repo root:
  - `cargo update --workspace`
  - result: still a no-op, `Locking 0 packages to latest compatible versions`
- Cargo produced no `Cargo.lock` delta, so there are still no remaining
  compatible dependency bumps to land from the current manifests.
- Shared unrelated worktree drift remains confined to:
  - `crates/fr-runtime/src/lib.rs`
  - `crates/fr-conformance/fixtures/core_acl.json`
  and was left untouched during this rerun.

## Updates

### workspace lock-file refresh (cargo update)
- **Purpose:** Pull all SemVer-compatible patch/minor bumps within existing spec
  ranges.
- **Updated in Cargo.lock:**
  - bitflags 2.11.0 → 2.11.1
  - hashbrown 0.16.1 → 0.17.0
  - indexmap 2.13.1 → 2.14.0
  - itoa 1.0.17 → 1.0.18
  - libmimalloc-sys 0.1.44 → 0.1.46
  - mimalloc 0.1.48 → 0.1.49
  - quote 1.0.44 → 1.0.45
  - rand 0.9.2 → 0.9.4
  - syn 2.0.115 → 2.0.117
  - typenum 1.19.0 → 1.20.0
  - unicode-ident 1.0.23 → 1.0.24
  - wasip2 1.0.2 → 1.0.3
  - (added) wit-bindgen 0.57.1 (transitive of wasip2)
- **Check:** `cargo check --workspace --all-targets` — green (0.16s, cached).
- **Tests:**
  - `cargo test -p fr-protocol` — 54 + 10 + 0 pass, 0 fail.
  - `cargo test -p fr-command` — 598 pass, 2 pre-existing failures
    (`function_list_and_stats_match_redis_reply_shapes`,
    `object_freq_and_idletime_require_exact_arity_before_other_paths`)
    addressed by the pre-existing dirty `crates/fr-command/src/lib.rs` (other
    agent). Not introduced by this lock update.

### sha2 (fr-conformance): 0.10.9 → 0.11.0
- **Breaking:**
  - `Digest::finalize()` now returns a `hybrid_array::Array<u8, ...>` newtype
    instead of `GenericArray`; no longer implements `LowerHex` directly.
  - Edition 2024, MSRV 1.85 (workspace already on 2024/compatible).
  - `digest` bumped to v0.11.
- **Migration:**
  - Replaced `format!("{:x}", hasher.finalize())` with explicit byte-wise
    hex formatting (`digest.as_slice()` + `write!(..., "{byte:02x}")`).
  - Call sites fixed: `crates/fr-conformance/src/bin/raptorq_artifact_gate.rs`
    and `crates/fr-conformance/src/bin/live_oracle_orchestrator.rs`.
- **Check:** `cargo check --workspace --all-targets` green.
- **Tests:** `cargo test -p fr-conformance` — 134 pass, 8 pre-existing failures
  (conformance_core_{config,connection,function,generic,object,scripting,server},
  fr_p2c_004_u010_config_requirepass_bridge_mode_split_is_stable), identical
  count to HEAD. No new failures from the sha2 bump.

### mio (fr-server): 1.0 → 1.2 (lock 1.1.1 → 1.2.0)
- **Breaking:** None. 1.x series is SemVer-compatible; 1.0 → 1.2 adds APIs
  (notably additional Unix-source fd handling) without removing any.
- **Check:** `cargo check --workspace --all-targets` green.
- **Tests:** `cargo test -p fr-server` – 56 pass, 1 pre-existing failure
  (`replica_sync_clears_failed_connection_and_schedules_retry`,
  Integer(-1) vs Integer(0) reconnect delta) confirmed to reproduce on HEAD.

### libc (fr-store): 0.2.184 → 0.2.185
- **Reason:** Align with fr-runtime spec; latest stable 0.2.185
- **Breaking:** None (patch version on 0.2.x track)
- **Check:** `cargo check --workspace --all-targets` passed
- **Tests:** `cargo test -p fr-store` – 243 pass, 1 pre-existing failure
  (`function_dump_restore_roundtrip_preserves_library_snapshot`, HashMap
  iteration order) confirmed to reproduce on HEAD before the bump. Not
  introduced by this change.


## Failed

*None yet.*

## Needs Attention

*None yet.*

## Notes

- Pre-existing dirty file `crates/fr-command/src/lib.rs` carries legitimate
  parity fixes from another agent (OBJECT FREQ ENOKEY, FUNCTION LIST
  engine-lowercase). Not reverted; excluded from library-updater commits
  (commits will touch only Cargo.toml/Cargo.lock/UPGRADE_LOG.md/progress JSON
  so other agents can commit the lib.rs fixes independently).
