# Pass 84 - SETEX/PSETEX metadata specialization rejected

Bead: `frankenredis-6kecb`

## Profile Target

Fresh SETEX/PSETEX P16/1M evidence before this pass still showed the
SETEX/PSETEX path paying repeated expiry/runtime/metadata costs:

- `Store::drop_if_expired`: 8.46%
- `__memcmp_avx2_movbe`: 8.29%
- `Runtime::refresh_store_runtime_info_context`: 6.18%
- `clock_gettime`: 5.11%
- `_mi_page_malloc_zero`: 4.82%
- `Runtime::execute_frame_internal`: 2.97%
- `frankenredis::process_buffered_frames`: 2.76%
- `Runtime::dispatch_with_client_context`: 2.10%
- `rewrite_relative_expire_for_propagation`: 1.77%
- `parse_command_args_borrowed_into`: 1.52%

Alien-graveyard match: cache-sized batch/metadata capsule and region-style
amortization. This pass tested only the smallest admissible capsule: extending
the existing `SET` runtime metadata specialization to `SETEX` and `PSETEX`.

## Lever Tested

Candidate source hunk:

- Count `SETEX` / `PSETEX` as writes without calling
  `fr_command::get_command_flags`.
- Answer fixed arity for exact `SETEX key seconds value` and
  `PSETEX key milliseconds value` without generic arity lookup.
- Extract key index 1 for `SETEX` / `PSETEX` without generic key metadata.

No TTL parsing, reply encoding, execution, propagation, AOF, replication,
transaction, keyspace notification, ordering, floating-point, or RNG logic was
changed.

## Builds And Validation

Baseline RCH build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass84-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1153651
```

Candidate RCH build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass84-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1156319
```

Candidate validation while the hunk was applied:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass84-check-target cargo check -p fr-runtime --all-targets
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass84-test-target cargo test -p fr-runtime set_write_metadata_specializations_cover_setex_psetex -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-6kecb-pass84-clippy-target cargo clippy -p fr-runtime --all-targets -- -D warnings
```

All three candidate checks passed. `cargo fmt -p fr-runtime --check` still
reported pre-existing unrelated rustfmt drift in this large file, so no
formatting rewrite was run.

Binary hashes:

```text
b6358392187c0b25086c5c49125d60cbeba8aa4c525eeae4e4522fa2a1981230  baseline frankenredis
cd0293836ec2509219b23bf7bdb2ddff65dbb9dc1fd97599f6a94d06cc90d52a  baseline fr-bench
ec8ac1d50a6f414076142c30847981ad98ba11279089ad7ab6ae41bc11b6f181  candidate frankenredis
9d6d76418266db4b5c706750d580bad6e49c26595a043e31b0132a968c8aa740  candidate fr-bench
```

## Isomorphism Proof

SETEX/PSETEX TCP golden comparator:

```text
baseline bytes: 992
candidate bytes: 992
baseline sha256: dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7
candidate sha256: dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7
equal: true
```

The transcript covers flush, SETEX, PSETEX, GET, PERSIST, lowercase/mixed case,
zero/negative/non-integer TTL errors, huge TTL error, database isolation, and
MULTI/EXEC queueing. Reply bytes, command ordering, TTL/error behavior, DB
selection, queued execution order, tie-breaking, floating-point behavior, and
RNG behavior were unchanged.

## Benchmarks

One-sided baseline before edit:

```text
SETEX/PSETEX alternate P16/1M: 4.731630541671429s +/- 0.2602077052083319s
```

Paired, baseline first:

```text
baseline:  4.5808099720857145s +/- 0.03805543821340276s
candidate: 4.558395363085714s  +/- 0.030071957667622072s
hyperfine: candidate 1.00x +/- 0.01 faster
```

Reversed, candidate first:

```text
candidate: 4.59594142588s       +/- 0.03699489760001452s
baseline:  4.607915604022857s   +/- 0.03911053704857246s
hyperfine: candidate 1.00x +/- 0.01 faster
```

## Decision

Rejected under Score>=2.0. The observed effect is noise-level in both paired
orders, so Impact is 0 and the source hunk was removed. No production code was
retained from this pass.

Next route: stop extending isolated command metadata specializations for this
bead. Attack a wider pipeline-window primitive instead: a batch/event-loop
metadata capsule that carries command class, key span, TTL rewrite class, and
runtime-info sideband decisions across the whole P16 frame group, with target
speedup >=1.20x before keep consideration.
