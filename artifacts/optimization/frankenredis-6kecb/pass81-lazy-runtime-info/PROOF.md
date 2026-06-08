# Pass 81: lazy runtime-info refresh rejected

Bead: `frankenredis-6kecb`

## Profile basis

The active SETEX/PSETEX P16/1M profile-backed bead points at the generic parser
and dispatch path after earlier SETEX/PSETEX-specific borrowed branches,
static `+OK` output, writev, store insertion, and matcher-router levers were
rejected. The residual profile rows include
`Runtime::refresh_store_runtime_info_context`, `dispatch_with_client_context`,
`parse_command_args_borrowed_into`, and command metadata hashing.

## Lever tested

Candidate source diff: `candidate-source.diff`.

The candidate removed the eager `refresh_store_runtime_info_context()` call from
the generic `execute_dispatch` hot path and refreshed the store's INFO-sideband
runtime counters only at `handle_info_command` entry. This targets the class of
per-command recomputation for tracking clients, tracking key totals,
persistence rewrite flags, and replication backlog byte accounting.

The candidate did not change command implementations, key ordering, expiry
deadline math, replication/AOF propagation, tie-breaking, floating-point
behavior, or RNG behavior. `maxmemory_bytes_live` remains updated by the
existing CONFIG/maxmemory update paths; the moved counters are INFO-facing.

## Validation while candidate was applied

- Baseline source: `72fdd061024a2ec7244655e8d2735e4a018615b6`.
- Baseline release-perf build via RCH:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass81-baseline-target cargo build --profile release-perf -p fr-server`.
- Candidate `cargo check -p fr-runtime --all-targets` passed via RCH.
- Candidate focused test passed via RCH:
  `cargo test -p fr-runtime info_clients_refreshes_tracking_context_on_demand -- --nocapture`.
- Candidate release-perf build via RCH:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass81-candidate-target cargo build --profile release-perf -p fr-server`.
- `cargo fmt -p fr-runtime -- --check` still reports pre-existing unrelated
  rustfmt drift across `crates/fr-runtime/src/lib.rs`; the candidate hunk was
  removed before this evidence commit.

Binary hashes:

- Baseline: `2bf9c329618ff8e816f18e3681e89a2097cb38f3ad5f8c25c1f43598675c382c`
- Candidate: `d4991f51f401dcc6c6b8011820aa43132edf33d5524c844d7ef0d051cee4197b`

## Golden output

Comparator:

```bash
python3 artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py 27111 27112 artifacts/optimization/frankenredis-6kecb/pass81-lazy-runtime-info/golden-compare.json
```

Result:

- Baseline bytes: `992`
- Candidate bytes: `992`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Equal: `true`

## Benchmark

Fresh one-sided baseline, SETEX/PSETEX alternate P16/1M, 50 clients, keyspace
10000, datasize 3:

- Baseline: `4.553121303605715s +/- 0.04160120486869498`

Paired hyperfine, same workload and local host, both binaries built via RCH:

- Baseline: `4.470996217114285s +/- 0.041711164604636176`
- Candidate: `4.488928137542857s +/- 0.037870852287557565`
- Summary: baseline ran `1.00 +/- 0.01` times faster than candidate.

## Decision

Rejected under the Score>=2.0 keep gate. The candidate is byte-identical on the
SETEX/PSETEX golden transcript but slightly slower on the paired target
benchmark, so the production source hunk and candidate-only test were removed.

Next route: stop runtime-info refresh micro-skips for this bead. The next
profile-backed attack should be a structurally different batched
parser-to-dispatch packet or event-loop batch primitive that amortizes owned
argv materialization and command metadata hashing across SETEX/PSETEX P16
without changing invalid-frame, ACL, pubsub, transaction, propagation, TTL, or
commandstats/errorstats ordering.
