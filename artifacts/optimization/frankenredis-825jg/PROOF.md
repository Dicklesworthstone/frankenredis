# frankenredis-825jg active-expire min-deadline sidecar proof

## Target

- Bead: `frankenredis-825jg`
- Lever: exact `BTreeMap<u64, usize>` count sidecar for key-expiry deadlines.
- Workload: alternating `SETEX` / `PSETEX`, 1,000,000 requests, 50 clients,
  pipeline 16, keyspace 1,000,000, value size 3.
- Baseline source: `45147c8c7dbe2bd4f93985df42195d595039d8b4`.

The prior small-keyspace deadline-count attempt was rejected because per-write
index maintenance dominated. This pass targets the shifted high-keyspace
long-TTL profile where active-expire miss scans were a top cost; the final
benchmark below was rerun against a sidecar-only candidate built from the exact
source hunk retained in this commit.

## Baseline and Profile Basis

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH had no admissible remote worker and used local fallback; the build stayed
crate-scoped.

Initial baseline hyperfine:

```text
8.81261031754 s +/- 0.49672681172 s
```

Previous profile from `frankenredis-svgvb` selected this target:

- `Store::run_active_expire_cycle`: 7.22% to 8.95% self
- BTree volatile-key iterator/range rows: about 3% + 3% self
- `__memcmp_avx2_movbe`: 6% to 9%, with a large branch under active expiry
- command/parser/dispatch rows were much smaller

## Behavior Proof

Golden transcript:

- valid `SETEX` and `PSETEX`
- `PERSIST` expiry-state proof
- lower/mixed-case commands
- invalid TTL fallback
- non-DB0 behavior
- `MULTI`/`EXEC` fallback

SHA-256:

```text
baseline  dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
candidate dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
```

Isomorphism:

- Ordering: preserved. If no key can expire yet, no deletion exists to order;
  once a deadline is due, the existing deterministic `volatile_keys` BTree
  cursor path still selects and propagates keys in the same order.
- Tie-breaking: unchanged. Key-order sampling remains BTree-based when due.
- Floating point: N/A.
- RNG: unchanged.
- Expiry behavior: unchanged at Redis-visible surfaces; lazy expiry still checks
  the entry deadline, active expiry still rechecks every sampled key, and
  future-deadline cycles only skip work that could not delete a key.

Validation:

```text
cargo fmt -p fr-store --check
python3 -m py_compile artifacts/optimization/frankenredis-svgvb/setex_bench.py artifacts/optimization/frankenredis-svgvb/run_setex_bench_once.py artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-check-target cargo check -p fr-store --all-targets
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-test-active-target cargo test -p fr-store active_expire -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-test-volatile-target cargo test -p fr-store volatile_keys_index_tracks_ttl_transitions -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-test-getex-target cargo test -p fr-store getex -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-clippy-target2 cargo clippy -p fr-store --all-targets -- -D warnings
```

`cargo check` ran remotely on `vmi1156319`; the other rch commands used local
fallback because workers were under pressure or excluded. The final candidate
release build for the kept source was:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-825jg-sidecaronly-target cargo build --profile release-perf -p fr-server -p fr-bench
```

## Benchmarks

Final sidecar-only paired confirmation:

- baseline: `8.53389880746 s +/- 0.15990945421 s`
- candidate: `5.61001787046 s +/- 0.75767578971 s`
- hyperfine summary: candidate `1.52x +/- 0.21` faster

Additional isolation run over a memory-only baseline confirmed the sidecar
itself was carrying the win:

- memory-only baseline: `8.485 s +/- 0.634 s`
- sidecar candidate: `5.192 s +/- 0.073 s`
- hyperfine summary: candidate `1.63x +/- 0.12` faster

Score:

- Impact 5, Confidence 4, Effort 2 => `10.0`
- Decision: keep.

## Post-Change Profile

Candidate profile workload:

- 500,000 requests, 50 clients, pipeline 16, keyspace 1,000,000
- throughput: `184641.60877144994 ops/sec`
- zero lost samples

Top shifted flat samples:

- `BTreeMap<Vec<u8>, SetValZST>::insert`: 8.84%
- `__memcmp_avx2_movbe`: 6.72%
- `SipHasher::write`: 5.73%
- `foldhash::RandomState::hash_one::<&Vec<u8>>`: 3.10%
- `core::str::from_utf8`: 2.20%
- `Runtime::refresh_store_runtime_info_context`: 2.17%
- `Store::update_expiry_deadline`: 2.04%
- `Store::internal_entries_insert`: 2.04%
- `parse_command_args_borrowed_into`: 1.77%
- `Runtime::dispatch_with_client_context`: 1.76%

Next route: attack TTL write-index layout. The active-expire miss scan is no
longer the target; the shifted primitive is the per-write `volatile_keys`
BTree insert/key-compare path, likely via a bucketed expiry-wheel or per-db
TTL write index that avoids key-ordered BTree insertion on long-TTL writes while
preserving due-key propagation order.
