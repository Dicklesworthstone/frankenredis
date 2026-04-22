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

*Filled at end of session.*

## Updates

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
