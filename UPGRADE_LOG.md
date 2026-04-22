# Dependency Upgrade Log

**Date:** 2026-04-21  |  **Project:** frankenredis  |  **Language:** Rust
**Agent:** Clawdstein-libupdater-frankenredis

## asupersync

**Not applicable** — `frankenredis` does not depend on `asupersync` in any
`Cargo.toml` (workspace root or per-crate). asupersync bump phase is a no-op.

## Inventory

External dependencies (non-path) declared in `crates/*/Cargo.toml`:

| Crate | Dep | Declared | Latest | Action |
|-------|-----|----------|--------|--------|
| fr-bench | hdrhistogram | 7.5.4 | 7.5.4 | up-to-date |
| fr-bench | serde | 1.0.228 | 1.0.228 | up-to-date |
| fr-bench | serde_json | 1.0.149 | 1.0.149 | up-to-date |
| fr-command (dev) | proptest | 1 | 1.11.0 | cargo update only |
| fr-conformance | serde | 1.0.228 | 1.0.228 | up-to-date |
| fr-conformance | serde_json | 1.0.149 | 1.0.149 | up-to-date |
| fr-conformance | sha2 | 0.10.9 | 0.11.0 | **BUMP** major 0.10 -> 0.11 |
| fr-persist (dev) | proptest | 1 | 1.11.0 | cargo update only |
| fr-protocol (dev) | proptest | 1 | 1.11.0 | cargo update only |
| fr-runtime | hex | 0.4 | 0.4.3 | cargo update only |
| fr-runtime | libc | 0.2.185 | 0.2.185 | up-to-date |
| fr-runtime | sha2 | 0.11.0 | 0.11.0 | up-to-date |
| fr-runtime (dev) | proptest | 1 | 1.11.0 | cargo update only |
| fr-sentinel (dev) | proptest | 1 | 1.11.0 | cargo update only |
| fr-server | mio | 1.0 | 1.2.0 | bump spec -> 1.2 |
| fr-server | tikv-jemallocator | 0.6 | 0.6.1 | cargo update only |
| fr-server | mimalloc | 0.1 | 0.1.49 | cargo update only |
| fr-store | libc | 0.2.184 | 0.2.185 | **BUMP** align with fr-runtime |
| fr-store (dev) | proptest | 1 | 1.11.0 | cargo update only |

## Summary

- **Updated:** 3 dep specs (+ one bulk lock refresh covering 13 deps)
- **Skipped / already-latest:** hdrhistogram, serde, serde_json, hex,
  tikv-jemallocator, libc (fr-runtime), sha2 (fr-runtime)
- **Failed:** 0
- **Circuit breaker:** No — clean finish.

First library-updater commit: `8006c7e` (libc alignment)
Last library-updater commit:  `db7f076` (workspace lock refresh)

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
