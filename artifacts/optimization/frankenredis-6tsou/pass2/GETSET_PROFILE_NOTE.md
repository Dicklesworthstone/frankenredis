# frankenredis-6tsou Pass 2 - GETSET Profile Note

## Scope

Mission: GETSET profile-backed candidate only.

No production Rust files were edited in this pass. Measurements used a fresh detached clean HEAD worktree at:

- `/data/projects/.scratch/frankenredis-6tsou-pass2-head-1288f679`
- HEAD: `1288f679e15d275b1a971f86a7c292118e379928`

This avoids contamination from shared-checkout dirt while preserving the current committed baseline.

## Build

Command:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6tsou-pass2-head-target cargo build --profile release-perf -p fr-server
```

Result:

- RCH worker: `vmi1149989`
- Status: success
- Binary: `/tmp/codex-fr-6tsou-pass2-head-target/release-perf/frankenredis`
- Binary sha256: `1f706f06749c15a6fe7b733b85a05608c952a806af3d317cc6f933b87e05679d`

## Baseline

Command:

```bash
hyperfine --warmup 3 --runs 10 --export-json artifacts/optimization/frankenredis-6tsou/pass2/getset-hit-baseline-hyperfine.json 'python3 artifacts/optimization/frankenredis-6tsou/run_resp_mode_once.py --server-bin /tmp/codex-fr-6tsou-pass2-head-target/release-perf/frankenredis --mode getset-hit --port 26362 --json-out artifacts/optimization/frankenredis-6tsou/pass2/last-getset-hit-baseline.json --requests 300000 --clients 50 --pipeline 16 --keyspace 10000 --datasize 3 --key-prefix fr6tsou-pass2-getset'
```

Result:

- Workload: `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000, datasize 3
- Mean: `2.350s +/- 0.307`
- Median: `2.219s`
- Min/max: `2.131s .. 3.040s`
- Hyperfine warned about outliers.
- Pass 1 comparable baseline was `2.425s +/- 0.059`.

Profile-run workload summary:

- Seconds: `3.060015207`
- Ops/sec: `98038.728`
- Pipeline latency p50/p95/p99: `6984.020us / 17346.580us / 25756.732us`

## Profile

Command:

```bash
perf record -F 499 -g -o artifacts/optimization/frankenredis-6tsou/pass2/getset-hit-baseline-perf.data -- python3 artifacts/optimization/frankenredis-6tsou/run_resp_mode_once.py --server-bin /tmp/codex-fr-6tsou-pass2-head-target/release-perf/frankenredis --mode getset-hit --port 26363 --json-out artifacts/optimization/frankenredis-6tsou/pass2/last-getset-hit-profile.json --requests 300000 --clients 50 --pipeline 16 --keyspace 10000 --datasize 3 --key-prefix fr6tsou-pass2-getset-profile
```

Result:

- Samples: `11699`
- Lost samples: `0`
- Note: kernel symbols were restricted by host perf settings.

Server flat top rows from `getset-hit-baseline-perf-server-nochildren.txt`:

| Hotspot | Self |
| --- | ---: |
| `Runtime::refresh_store_runtime_info_context` | 1.45% |
| `Runtime::execute_frame_internal` | 1.35% |
| `foldhash::RandomState::hash_one::<&Vec<u8>>` | 1.17% |
| `fr_protocol::parse_command_args_borrowed_into` | 0.79% |
| `Store::internal_entries_insert` | 0.63% |
| `frankenredis::process_buffered_frames` | 0.63% |
| `Runtime::dispatch_with_client_context` | 0.59% |
| `fr_command::rewrite_effect_command_for_propagation` | 0.57% |
| `fr_command::command_table_index` | 0.41% |
| `Runtime::execute_dispatch` | 0.38% |
| `Vec<u8>::clone` | 0.30% |
| `fr_store::canonical_string_value` | 0.30% |
| `Store::getset` | 0.28% |
| `frankenredis::copy_borrowed_argv_into_scratch` | 0.27% |
| `fr_command::getset` | 0.25% |
| `fr_command::acl_command_selectors_for_argv` | 0.25% |

Server children rows from `getset-hit-baseline-perf-server-children.txt`:

| Hotspot | Children | Self |
| --- | ---: | ---: |
| `ClientConnection::try_flush` | 6.21% | 0.00% |
| `Runtime::execute_frame_internal` | 2.99% | 1.35% |
| `Runtime::refresh_store_runtime_info_context` | 2.64% | 1.45% |
| `Store::internal_entries_insert` | 1.35% | 0.63% |
| `frankenredis::process_buffered_frames` | 1.32% | 0.63% |
| `Runtime::dispatch_with_client_context` | 1.31% | 0.59% |
| `fr_protocol::parse_command_args_borrowed_into` | 1.10% | 0.79% |
| `Store::drop_if_expired` | 1.07% | 0.03% |
| `fr_command::classify_command` | 1.06% | 0.36% |
| `fr_command::command_table_index` | 1.05% | 0.41% |
| `fr_command::getset` | 0.44% | 0.25% |
| `Store::getset` | 0.32% | 0.28% |

## Code-path inspection

Current generic path:

- Server parses borrowed RESP args, then copies them into reused argv scratch for generic dispatch: `crates/fr-server/src/main.rs:1650`.
- Runtime dispatches argv through `execute_argv_with_unix_time_us` and `execute_dispatch`: `crates/fr-runtime/src/lib.rs:5816`.
- Command handler clones key and value into the store call: `crates/fr-command/src/lib.rs:4000`.
- Store `getset` uses owned key/value because it must insert a new entry and return the previous value: `crates/fr-store/src/lib.rs:4455`.
- Store semantics include keyspace hit/miss accounting, optional LFU bump, wrong-type preservation, TTL clearing through `Entry::new(..., None, now_ms)`, old-value return, and dirty increment.

A GETSET borrowed fast path would avoid some generic dispatch work and `copy_borrowed_argv_into_scratch`, but it would still need to allocate/copy the key and value for the replacement entry, allocate/copy the returned old string, execute insert bookkeeping, preserve write stats, and preserve lazy-expiry propagation. The directly GETSET-specific `Store::getset` + `fr_command::getset` rows are only about `0.76%` children combined, while the largest rows are shared bookkeeping/output.

## Opportunity Matrix

| Lever | Impact | Confidence | Effort | Score | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Add `execute_plain_getset_borrowed` plus `Store::getset_borrowed` | 1 | 2 | 3 | 0.67 | Reject for this pass |
| Batch/output write-buffer primitive after profiling write-heavy commands | 3 | 3 | 3 | 3.00 | Route to later pass, outside GETSET-only scope |
| Runtime bookkeeping reuse around command metadata/context refresh | 2 | 3 | 3 | 2.00 | Route to later pass, not GETSET-specific |

## Recommendation

Reject implementing a GETSET borrowed fast path in Pass 2.

Reason: the profile does not show a GETSET-specific top-hotspot. `fr_command::getset`, `Store::getset`, argv copy, and `Vec<u8>::clone` are individually small, and the previous adjacent SETNX borrowed fast path already measured as noise (`1.00x-1.01x`). A GETSET-only fast path is unlikely to clear the Score >= 2.0 gate, and implementing it would add high proof surface around TTL clearing, LFU state, wrong-type behavior, keyspace stats, slowlog/latency/threat accounting, lazy-expiry propagation, and write counters.

Next profile-backed direction for later passes: stop adding one-command write fast paths unless a command-specific profile changes. The stronger profile-backed primitives are shared output batching/write-buffer flushing and runtime bookkeeping reuse, because `try_flush`, `execute_frame_internal`, `refresh_store_runtime_info_context`, command classification/indexing, and shared dispatch rows dominate over GETSET itself.
