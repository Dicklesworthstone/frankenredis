# FrankenRedis fidelity and differential gates

FrankenRedis has several complementary compatibility test layers. They should not be
collapsed into one number: bespoke differential fixtures, targeted live-oracle probes,
cross-version persistence/replication gates, fuzzers, and Redis's own upstream Tcl tests
catch different failure classes.

The compatibility target for V1 is Redis 7.2.4. CI builds the official 7.2.4 source at
commit `d2c8a4b91e8c0e6aefd1f5bc0bf582cddbe046b7` and verifies that exact SHA before using the
tree as an oracle or upstream-test source.

## Upstream Redis Tcl tests

There are now two upstream-test lanes, both executing the **unmodified Redis 7.2.4 Tcl
tests themselves** through Redis's own `runtest` external-server mechanism.

### Fast blocking upstream slice

`scripts/upstream_redis_tcl_fidelity.py` runs 72 selected client-visible assertions across
strings, integer arithmetic, keyspace semantics, transactions, and protocol behavior on
every main conformance run.

This lane is deliberately anti-vacuous: it fails if the upstream checkout is not the exact
7.2.4 release commit, if a selected test disappears or is skipped, if it executes more than
once, or if the upstream harness returns failure. A green `0/0` run therefore cannot be
mistaken for compatibility evidence.

```sh
python3 scripts/upstream_redis_tcl_fidelity.py \
  --upstream legacy_redis_code/redis \
  --fr target/debug/frankenredis \
  --report /tmp/upstream-redis-tcl.json
```

### Complete upstream external-mode suite

`scripts/upstream_redis_tcl_full.py` asks the pinned Redis 7.2.4 harness for its complete
default unit list (`runtest --list-tests`) and then runs that entire suite against one live
FrankenRedis process with `--host`/`--port`. There is **no FrankenRedis-maintained skip
list**. Redis's own `external:skip` tags remain authoritative, exactly as they are in the
upstream harness.

The full runner strengthens the upstream exit status with anti-vacuity checks: every default
unit returned by `--list-tests` must complete exactly once, at least one upstream assertion
must actually pass, the target port must be unoccupied before launch, the upstream Git SHA
must be exactly 7.2.4, and FrankenRedis must remain alive for the run. Its JSON report records
completed units, pass/skip/ignore/error counts and records, missing/duplicate/unexpected
units, and the upstream suite exit code; the raw upstream log is retained alongside it.

```sh
python3 scripts/upstream_redis_tcl_full.py \
  --upstream legacy_redis_code/redis \
  --fr target/debug/frankenredis \
  --report /tmp/upstream-redis-tcl-full.json \
  --log /tmp/upstream-redis-tcl-full.log
```

`.github/workflows/upstream-redis-7.2.4-full.yml` executes the complete lane on relevant
pushes to `main`, nightly, and on demand. The workflow checks out the exact Redis 7.2.4
release commit, builds its test helper binaries (`redis-cli`, `redis-benchmark`, etc.), runs
every default upstream test unit in external mode, publishes a GitHub step summary, and
uploads the complete evidence bundle. Until the initial incompatibility set has been closed
or explicitly baselined, automatic runs are observational rather than branch-blocking;
`workflow_dispatch` exposes an `enforce=true` switch that makes the full upstream verdict a
hard failure immediately.

Accordingly, do not claim that the complete upstream suite is green unless a recorded full
run says so. The important distinction is now explicit: the repository **runs the complete
upstream external-mode suite**, while the smaller 72-test lane is the current always-blocking
upstream gate.

## Consolidated differential parity runner

```sh
# Build fr first, then:
python3 scripts/parity_suite.py legacy_redis_code/redis/src/redis-server /tmp/fr_bin
```

`parity_suite.py` is the consolidated **release-readiness / migration-safety runner**. It
launches its own servers and runs the current load-bearing cross-compatibility and semantic
gates, printing a PASS/FAIL scorecard and exiting non-zero on failure. Do not infer its gate
count from old documentation: the runner has grown substantially and its source is the
canonical manifest.

A green run is strong evidence for the surfaces it exercises, especially bidirectional
RDB/AOF/replication interoperability, but it is not by itself proof of every behavior in the
upstream Redis test suite.

## Live-oracle fixture matrix

`crates/fr-conformance/fixtures/live_oracle_matrix.json` is the manifest for the Rust
live-oracle orchestrator. `baseline` is a small smoke matrix; `parity` is the broader
Redis-vs-FrankenRedis matrix used by the main CI fidelity workflow.

```sh
./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379 --matrix parity
```

The repository also contains a 4,975-case bespoke conformance corpus spanning the command
surface. That corpus size is useful inventory information; it must not be stated as though
all 4,975 cases are necessarily executed by every CI invocation unless the selected matrix
and report actually establish that.

## Invocation conventions

Most standalone differential gates take a Redis oracle and a FrankenRedis server in one of
these forms:

| style | invocation |
|-------|------------|
| positional | `gate.py <oracle_port> <fr_port>` |
| argparse | `differ.py --oracle <port> --fr <port>` |
| self-orchestrating | `gate.py <redis-server-bin> <fr-bin> [base_port]` |

Both servers often need `--enable-debug-command yes` because some gates use `DEBUG DIGEST`
as a cross-implementation state oracle. Launch the Redis oracle from a clean working
directory so it cannot load stale `dump.rdb` or append-only state. For SORT/locale gates,
pin the oracle with `LC_ALL=C`.

## Differential gate categories

- **Interop / migration safety** (bidirectional, self-orchestrating): `rdb_cross_compat_gate`, `aof_cross_compat_gate`, `replication_cross_compat_gate`, `dump_byte_equality_gate`, plus `replication_convergence_gate`, `replication_multi_wrap_gate`, `aof_propagation_stream_gate`, `dump_restore_differ`, `reload_dump_determinism_gate`, `restore_encoding_differ`, `config_persistence_reload_gate`.
- **Data types**: `zset_differ`, `hash_differ`, `set_differ`, `list_differ`, `bitmap_differ`, `bitfield_differ`, `geo_differ`, `sort_differ`, `scan_differ`, `stream_*`, `zset_store_bulk_differ`, `multikey_pop_differ`, `copy_command_differ`, `lpos_differ`, `string_growth_differ`, `strlist_encoding_differ`.
- **Semantics / transactions**: `ttl_semantics_differ`, `watch_semantics_differ`, `multi_exec_differ`, `transaction_differ`, `validation_order_differ`, `reset_state_differ`, `object_policy_differ`, `blocking_differ`, `blocking_edge_differ`, `blocking_fairness_gate`, `rare_write_state_gate`.
- **Encoding**: `encoding_differ`, `encoding_config_boundary_differ`, `object_encoding_boundary_gate`, `store_encoding_differ`, `meta_encoding_chain_gate`, `reload_encoding_survival_gate`.
- **Pub/sub & client tracking**: `pubsub_differ`, `subscribe_mode_differ`, `keyspace_notif_differ`, `client_tracking_differential_probe`, `track_crosskey_differ`.
- **Client / connection / limits**: `client_kill_differ`, `monitor_differ`, `large_pipeline_drain_gate`, `strict_limit_gate`.
- **Scripting**: `eval_semantics_differ`, `lua_semantics_differ`, `lua_lib_differ`, `function_fcall_gate`.
- **ACL / cluster / sentinel**: `acl_semantics_gate`, `cluster_admin_parity_gate`, `sentinel_differ`.
- **Stats / introspection**: `info_stats_differ`, `keyspace_accounting_gate`, `cmdstat_keyspace_parity_gate`, `dirty_accounting_gate`, `slowlog_trunc_differ`, `command_getkeys_gate`, `command_introspection_gate`, `resp3_type_fidelity_gate`, `getkeys_flags_differ`, `arity_error_differ`, `introspection_semantics_gate`.
- **Numeric / format / config**: `float_format_differ`, `hexfloat_incr_differ`, `config_defaults_gate`, `config_set_validation_differ`.
- **Randomized fuzzers**: `random_command_differ`, `random_reply_differ`, `random_state_differ`, `random_differential_fuzz`, `fuzz_untrodden_differ`, `option_fuzz_differ`, `edge_sweep_differ`, `edge_sweep2_differ`.
- **Performance**: `large_value_perf_gate`.

## Interpretation notes

- A failing fixture can be a FrankenRedis bug, a bad fixture, or a configuration mismatch;
  reproduce against a bare pinned Redis 7.2.4 oracle before classifying it.
- Non-deterministic replies such as random-member selection, locale-sensitive sort order,
  time-based TTLs, and auto-generated stream IDs need semantic comparison rather than naive
  byte equality.
- Command presence is not behavioral fidelity. “241 commands implemented” and “241 commands
  verified against upstream behavior” are different claims and should stay separate.
- A compatibility claim should always identify the evidence tier: upstream Tcl, live oracle,
  bespoke differential fixture, interop gate, or implementation-surface audit.
