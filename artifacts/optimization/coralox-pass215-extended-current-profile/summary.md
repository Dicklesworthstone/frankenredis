# Pass 215 Extended Residual Profile

## Scope

- Bead: `frankenredis-lcqgd`
- Source build: `897cb010173dbc71dde4c64a94096403386d73a5`
- Repository head at closeout: `23450407ce0ce9e1b913ad207c163b27d9d16d10`
- Note: `23450407c` only added/updated scripts; the measured `fr-server` data path was unchanged from the rch-built binary.
- Binary sha256:
  - `frankenredis`: `dc402b78f712258b540c2d7996216b21e1d4135e4b127343624737cb8df12fc4`
  - `fr-bench`: `a3b6a98e2596edd36c307c8cb3a7c1010156d8e6bd2470c8fb85c3b1520d66a1`
  - Redis server oracle: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
  - Redis benchmark oracle: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`

## Benchmark

RCH release-perf binaries were compared with vendored Redis 7.2.4 using
`scripts/bench_vs_redis.py` at P16/C50/n300k, five trials per command.
The run happened under visible shared-machine load (`loadavg=11.40`), so
absolute throughput is noisy; the same-window Redis/FrankenRedis ratios are
the decision evidence.

All tested median ratios were at or above the 0.9x Redis gate:

- `ping_inline`: `0.98x`
- `ping_mbulk`: `1.02x`
- `set`: `1.02x`
- `get`: `1.10x`
- `incr`: `1.03x`
- `lpush`: `1.08x`
- `rpush`: `1.07x`
- `lpop`: `1.16x`
- `rpop`: `1.11x`
- `sadd`: `1.00x`
- `hset`: `1.01x`
- `spop`: `1.04x`
- `zadd`: `1.05x`
- `mset`: `1.26x`
- `lrange_100`: `1.09x`
- `lrange_300`: `1.29x`
- `lrange_500`: `1.25x`
- `lrange_600`: `1.16x`

`perf_event_paranoid=4` blocks kernel perf attribution on this host.

## Decision

No production source lever was attempted or kept. The current extended sweep did
not identify a below-gate, profile-backed target likely to score at least 2.0.

Behavior proof is no-op isomorphism: no source changed, so RESP ordering,
tie-breaking, floating-point, RNG, expiry, persistence, replication, command
stats, and reply bytes are unchanged. The golden-output artifact for the closeout
is the unchanged benchmark/oracle transcript bundle copied here.

Next route: do not repeat large-SET buffer reuse/read-slab/static-chunk or
standard P16 command micro-levers without a new below-gate row. The next useful
primitive is a fresh profile-capable data-plane probe, such as an epoll-vs-
io_uring attribution run with SQ/CQ utilization evidence, or a region/slab
command-storage profile that names parser/argv allocation or buffer ownership as
the dominant row.
