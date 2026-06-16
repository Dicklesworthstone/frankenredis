# frankenredis differential gate suite

These scripts prove **fr is byte-exact and interop-compatible with vendored
redis 7.2.4**. Each one drives a running `fr` and a `redis-server` oracle with
the same commands and asserts identical replies / state / on-wire formats.
They are how parity is held as the codebase changes.

## Quick start — one-command parity check

```sh
# build fr (rch or local), then:
python3 scripts/parity_suite.py legacy_redis_code/redis/src/redis-server /tmp/fr_bin
```

`parity_suite.py` is the consolidated **release-readiness / migration-safety
runner**. It launches its own servers, runs 23 of the most load-bearing gates
(cross-compat interop + all data types + transactions + semantics), and prints
a PASS/FAIL scorecard (exit non-zero on any failure). A green run means a
redis client / replica / RDB / AOF will interoperate with fr in both
directions.

## Invocation conventions

Most gates take a vendored `redis-server` oracle and an `fr` server and use one
of two argument styles (the runner auto-handles both):

| style | invocation |
|-------|------------|
| positional | `gate.py <oracle_port> <fr_port>` |
| argparse   | `differ.py --oracle <port> --fr <port>` |
| self-orchestrating | `gate.py <redis-server-bin> <fr-bin> [base_port]` (spins up its own servers) |

Both servers generally need `--enable-debug-command yes` (gates use `DEBUG
DIGEST` as a cross-impl oracle — fr emits it byte-identically to redis). Launch
the redis oracle from a **clean cwd** so it never loads a stale
`dump.rdb`/`appendonly*`. For SORT/locale gates, pin the oracle with `LC_ALL=C`.

## Categories

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

## Notes

- A failing fixture or gate may be a **wrong fixture / config mismatch**, not an
  fr bug — verify against bare vendored redis first (config defaults: fr uses
  redis *compiled* defaults, e.g. `list-max-listpack-size=-2`,
  `hash-max-listpack-entries=512`; the shipped `redis.conf` uses 128).
- Inherently non-deterministic replies (`*RANDMEMBER`/`SPOP` selection, glibc
  `qsort` tie order under `SORT ... BY <missing> ALPHA`, `strcoll` under non-C
  locales, time-based TTLs, auto stream IDs) are expected to differ and are
  filtered by the gates rather than treated as bugs.
