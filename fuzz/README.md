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
| `fuzz_runtime_execute_bytes` | Raw runtime RESP ingress | Structure-aware execute_bytes vs execute_frame differential |
| `fuzz_function_restore` | FUNCTION LOAD/DUMP/RESTORE handling | Structure-aware round-trip + hostile restore atomicity |
| `fuzz_psync_reply` | Replica PSYNC reply parsing | Structure-aware parser shape validation + raw canonicalization |
| `fuzz_tls_config` | TLS config directive parsing and apply planning | Structure-aware parser/validator invariants + rewrite/apply determinism |
| `fuzz_lua_eval` | Embedded Lua parser and evaluator | Structure-aware pure-script determinism + whitespace/semicolon invariance |
| `fuzz_client_tracking` | `CLIENT TRACKING` option parsing and caching-mode validation | Structure-aware state round-trip + raw argv canonicalization |
| `fuzz_client_reply` | `CLIENT REPLY` mode parsing and suppression state transitions | Structure-aware sequence differential against a shadow model + raw command-stream validation |
| `fuzz_migrate_request` | `MIGRATE` request parsing | Structure-aware option-matrix validation + canonical argv round-trip |
| `fuzz_keyspace_events` | `notify-keyspace-events` config parsing | Structure-aware flag-model validation + canonical string round-trip |
| `fuzz_eventloop_validators` | Event-loop planning and validator invariants | Structure-aware state-machine/model checks + raw phase-trace replay |
| `fuzz_config_file` | Redis config file tokenization | Structure-aware directive parsing + raw quoted-token hardening |

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

# Run raw runtime ingress fuzzer
cargo +nightly fuzz run fuzz_runtime_execute_bytes

# Run FUNCTION load/restore fuzzer
cargo +nightly fuzz run fuzz_function_restore

# Run PSYNC reply parser fuzzer
cargo +nightly fuzz run fuzz_psync_reply

# Run TLS config parser/planner fuzzer
cargo +nightly fuzz run fuzz_tls_config

# Run Lua parser/evaluator fuzzer
cargo +nightly fuzz run fuzz_lua_eval

# Run CLIENT TRACKING parser/invariant fuzzer
cargo +nightly fuzz run fuzz_client_tracking

# Run CLIENT REPLY state-machine fuzzer
cargo +nightly fuzz run fuzz_client_reply

# Run MIGRATE request parser fuzzer
cargo +nightly fuzz run fuzz_migrate_request

# Run notify-keyspace-events parser fuzzer
cargo +nightly fuzz run fuzz_keyspace_events

# Run event-loop validator/state-machine fuzzer
cargo +nightly fuzz run fuzz_eventloop_validators

# Run Redis config file parser fuzzer
cargo +nightly fuzz run fuzz_config_file

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
