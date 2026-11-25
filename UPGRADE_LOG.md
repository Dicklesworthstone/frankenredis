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

---

# 2026-11-25 semver lock refresh

## Summary
- **Updated:** 8 packages in `Cargo.lock`, all semver-compatible; no manifest change.
- **Held:** `sha2` 0.11.1 (0.11.2 measured slower, below); `raptorq` 1.8.1 (2.x changes
  RFC 6330 PayloadId assignment, so existing sidecars need a migration plan first, issue #1).
- **fuzz/Cargo.lock:** unchanged (its only candidate was the same `sha2` 0.11.2).
- **Security audit:** `cargo audit` reports 0 vulnerabilities and 0 warnings before and after
  (183 crates), and for `fuzz/Cargo.lock` (87 crates).

## Updates
None of these is on a server hot path. `criterion` (a dev-dependency used by benches) pulls in
clap and zerocopy. `fr-bench`'s hdrhistogram pulls in flate2, and through it crc32fast.
`toml_edit`/`toml_parser`/`zlib-rs` sit behind features this workspace does not enable.
- clap 4.6.9 -> 4.6.10, clap_builder 4.6.7 -> 4.6.10
- crc32fast 1.5.5 -> 1.5.6
- toml_edit 0.25.20 -> 0.25.21, toml_parser 1.1.6 -> 1.1.7
- zerocopy / zerocopy-derive 0.8.64 -> 0.8.65
- zlib-rs 0.6.8 -> 0.6.9

## Held: sha2 0.11.1 -> 0.11.2
SHA-256 is used at runtime for ACL password hashing (64-byte inputs) and FEC sidecars
(512-byte symbol hashes, streamed source hashes). 0.11.2 adds nothing frankenredis uses (riscv
and SHA-512 AVX2 work, backend-flag precedence) but rewrites the portable `soft` backend, which
x86 CPUs without SHA-NI run. A/B: one standalone binary per version (thin LTO, 1 CGU, release),
both built and run in a single remote job on one host, 7 interleaved rounds per arm, medians:

| Host | Backend | 64 B digest | 512 B digest | 64 MiB stream |
|---|---|---|---|---|
| Xeon E3-1245 v2, no SHA-NI, idle | default dispatch | 606.0 -> 617.9 ns (+2.0%) | 2635.7 -> 2685.3 ns (+1.9%) | 212.5 -> 208.7 MiB/s (-1.8%) |
| same | forced `sha2_backend="soft"` | 601.6 -> 626.4 ns (+4.1%) | 2619.7 -> 2712.3 ns (+3.5%) | 210.3 -> 204.1 MiB/s (-3.0%) |
| AMD EPYC, SHA-NI, loaded | default dispatch | 79.0 -> 79.6 ns (+0.7%) | 313.5 -> 319.1 ns (+1.8%) | 1814.8 -> 1773.7 MiB/s (-2.3%) |
| same | forced `sha2_backend="soft"` | 379.2 -> 408.6 ns (+7.7%) | 1842.6 -> 1840.1 ns (-0.1%) | 331.9 -> 306.4 MiB/s (-7.7%) |

On the idle host every 0.11.2 run was slower than every 0.11.1 run. Digests were identical
across all runs.

## Verification
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed (compiles every
  updated crate).
