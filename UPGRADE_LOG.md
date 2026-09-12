# Dependency Upgrade Log

**Date:** 2026-09-11 | **Project:** FrankenRedis | **Language:** Rust

## Summary
- **Updated:** 54 packages in Cargo.lock, including workspace dependencies
- **Skipped:** 2 (`raptorq` pinned to 1.8.x per RaptorQ durability schema contract; `tikv-jemallocator` 0.6.1)
- **Failed:** 0
- **Security Audit:** Clean (0 vulnerabilities found across 177 dependencies)

## Key Updates

### libc: 0.2.185 → 0.2.189
- **Type:** Workspace dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### memchr: 2.8.0 → 2.8.3
- **Type:** Workspace dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### mio: 1.2.0 → 1.2.3
- **Type:** Workspace dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### serde / serde_json: 1.0.228 / 1.0.149 → 1.0.229 / 1.0.151
- **Type:** Workspace dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### icu_collator / icu_locale_core: 2.2.0 → 2.3.1 / 2.3.0
- **Type:** `fr-command` dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### io-uring: 0.7.13 → 0.7.15
- **Type:** `fr-uring` dependency
- **Breaking:** None
- **Tests:** ✓ Passed

### Transitive Dependencies
- 54 transitive crates bumped to latest patch/minor versions (`bitflags`, `clap`, `cpufeatures`, `crossbeam`, `flate2`, `indexmap`, `syn`, `zerocopy`, `zerovec`, etc.)

## Security & Verification
- `cargo audit`: 0 vulnerabilities reported.
- `cargo check --workspace --all-targets`: Passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: Passed.
- `cargo fmt --check`: Passed.
