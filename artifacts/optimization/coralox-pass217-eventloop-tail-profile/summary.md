# Pass 217 Event-Loop Tail Profile

## Scope

- Bead: `frankenredis-w7rkr`
- Source head: `773a3a47db7b3038bebe65bc29bb4cb5aea3ff96`
- RCH worker: `vmi1167313`
- Build command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- FrankenRedis binary sha256: `f4d4a58b7378e48f314048a050f82c2bf484d6dd5ed871a774f3bb1f299781a2`
- `fr-bench` binary sha256: `bb3db00850ea2d456edc500b837a3e97784218fddd8b2ffd79c4aeb762367d41`
- Redis server oracle sha256: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
- Redis benchmark oracle sha256: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`

## Baseline

The pass217 alien data-plane route looked for a fresh event-loop, batched-I/O,
or tail-latency target after pass215/pass216 invalidated standard P16,
large-value, ZRANGEBYSCORE, and allocation-profile rows.

Matrix: `fr-bench` SET/GET/INCR/MIXED at P1/P4/P16, C50, 60k requests,
10k-key keyspace, 32-byte values, plus `redis-benchmark` PING throughput.

Throughput ratios (`fr/redis`, higher is better) stayed above the 0.9x gate:

- SET: P1 `0.950x`, P4 `1.048x`, P16 `1.013x`
- GET: P1 `1.056x`, P4 `0.988x`, P16 `1.229x`
- INCR: P1 `1.060x`, P4 `1.040x`, P16 `1.011x`
- MIXED: P1 `1.032x`, P4 `1.224x`, P16 `1.436x`
- PING inline: P1 `1.031x`, P4 `1.014x`, P16 `0.983x`
- PING multibulk: P1 `1.011x`, P4 `1.167x`, P16 `1.230x`

P99 tail ratios (`fr/redis`, lower is better) were favorable on every tested
SET/GET/INCR/MIXED row. The only suspicious broad-matrix signal was SET/P1
p999 at `1.591x`; a focused 7-trial SET/P1 confirmation invalidated it:

- Median ops ratio: `0.982x`
- Median p99 ratio: `0.828x`
- Median p999 ratio: `0.912x`

`perf_event_paranoid=4` blocks kernel perf attribution on this host.

## Behavior Proof

No production source changed.

Raw Redis/FrankenRedis RESP transcript matched exactly for:
`FLUSHALL`, `SET k v`, `GET k`, `INCR n`, `GET n`, `PING`, `QUIT`.

- Response sha256: `79529d5aa52e35c347df6f0b5f618fe1c0595dcac4000ed167bd8b5a72773280`
- Response length: `40` bytes on both engines

Isomorphism: no source changed, so ordering, tie behavior, floating point, RNG,
expiry, persistence, replication, dirty count, command stats, and reply bytes
are unchanged.

## Decision

Close `frankenredis-w7rkr` evidence-only. The current event-loop/tail profile did
not expose a below-gate or tail-regression row that could support a Score >= 2.0
source lever.

Next route: do not micro-tune SET/P1 tail or standard event-loop paths without a
new profile row. Pursue a stronger alien primitive with a different measurement
surface, such as cross-client fairness under blocking/large-output interference
or RESP frame scanning only if a fresh profile names it as dominant.
