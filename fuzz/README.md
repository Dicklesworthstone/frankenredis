# FrankenRedis Fuzz Targets

This directory contains fuzz targets for security-critical parser surfaces.

## Targets

| Target | Function | Oracle Type |
|--------|----------|-------------|
| `fuzz_resp_parser` | RESP protocol parsing | Crash detector |
| `fuzz_resp_roundtrip` | RESP encode/decode | Round-trip invariant |
| `fuzz_aof_decoder` | AOF file parsing | Crash detector |
| `fuzz_rdb_decoder` | RDB file parsing | Crash detector |
| `fuzz_dump_restore` | DUMP/RESTORE payload handling | Structure-aware round-trip + hostile payload invariants |
| `fuzz_acl_rules` | ACL file parsing and canonicalization | Structure-aware round-trip + hostile text stabilization |

## Running Fuzz Tests

```bash
# Run RESP parser fuzzer (recommended first target)
cargo +nightly fuzz run fuzz_resp_parser

# Run with specific corpus
cargo +nightly fuzz run fuzz_resp_parser fuzz/corpus/fuzz_resp_parser

# Run AOF decoder fuzzer
cargo +nightly fuzz run fuzz_aof_decoder

# Run RDB decoder fuzzer
cargo +nightly fuzz run fuzz_rdb_decoder

# Run DUMP/RESTORE fuzzer
cargo +nightly fuzz run fuzz_dump_restore

# Run ACL rule parser fuzzer
cargo +nightly fuzz run fuzz_acl_rules

# Run round-trip invariant checker
cargo +nightly fuzz run fuzz_resp_roundtrip
```

## Corpus Management

```bash
# Minimize corpus (removes redundant inputs)
cargo +nightly fuzz cmin fuzz_resp_parser

# Minimize a crash input for debugging
cargo +nightly fuzz tmin fuzz_resp_parser artifacts/fuzz_resp_parser/crash-xxx
```

## CI Integration

The fuzz corpus serves as regression tests. Run the corpus without fuzzing:

```bash
cargo +nightly fuzz run fuzz_resp_parser -- -runs=0
```

## Adding New Targets

1. Create `fuzz_targets/fuzz_<name>.rs`
2. Add `[[bin]]` entry to `Cargo.toml`
3. Create seed corpus in `corpus/fuzz_<name>/`
4. Run initial fuzzing campaign

## Crash Triage

1. Always minimize first: `cargo +nightly fuzz tmin <target> <crash>`
2. Reproduce: run minimized input multiple times
3. Deduplicate: same top-5 stack frames = same bug
4. Fix and add regression test
