# Pass 65 Supplemental Hot-Key Profile Evidence

## Target

- Bead: `frankenredis-l67mp`.
- Supplemental clean baseline source: `c233eb801`.
- Candidate family under evaluation: epoch/fingerprint hot-key GET read
  certificate or stable access sidecar.

## Locality Baseline

GET P16/300k, 50 clients, 3-byte values:

- hot-key `keyspace=1`: `0.3558 s +/- 0.0054`
- uniform `keyspace=10000`: `0.4354 s +/- 0.0103`
- hot-key ran `1.22x +/- 0.03` faster than uniform.

Longer drive runs:

- hot-key GET P16/5M: `861546.45 ops/sec`, p99 `1434 us`.
- uniform GET P16/1M: `692147.32 ops/sec`, p99 `1894 us`.

## Profile Evidence

Hot-key GET P16/1M attached server profile:

- child-inclusive dominant path: `ClientConnection::try_flush` -> `__send`.
- `Runtime::refresh_store_runtime_info_context`: `6.01%` self.
- `fr_protocol::parse_command_args_borrowed_into`: `2.07%` self.
- `Runtime::execute_plain_get_borrowed`: `1.92%` self.
- Store lookup/expiry/touch did not break out as a top self hotspot.

The longer 5M attached profile is retained as an artifact, but the fixed perf
window let idle `epoll_wait` dominate after the workload finished, so it is not
used as a source-edit target.

## Decision

This supplemental profile agrees with the no-source rejection: hot-key locality
exists, but the current profile does not justify a store certificate edit. Any
pass66 stable-entry sidecar must first re-establish store/key lookup dominance
with cleaner workload-aligned profiling. Otherwise the next implementation
target should pivot to output/syscall batching or runtime metadata amortization.
