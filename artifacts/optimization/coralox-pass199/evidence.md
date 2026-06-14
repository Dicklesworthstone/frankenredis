# frankenredis-ohsk5.62 PASS 199 Evidence

Date: 2026-06-14
Agent: CoralOx
Head: 045bd8f4c1664f64164ecda048e6e1446f691386

## Build

- Command: `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-pass199-current cargo build --profile release-perf -p fr-server -p fr-bench`
- Worker: `vmi1156319`
- FrankenRedis SHA256: `625d8d79682873ac28f91a6edfb21919c92b6d38a53005286acc37684663b7ef`
- fr-bench SHA256: `8347487dbc787b2c165df22b1791a1c3369f252f4ffe2b107b5e8acc2369f424`
- Redis server SHA256: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
- redis-benchmark SHA256: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`

## Standard P16/C50 Sweep

Command:

```bash
python3 scripts/bench_vs_redis.py 26399 26400 --trials 5 --n 100000 --pipeline 16 --clients 50 --bench legacy_redis_code/redis/src/redis-benchmark
```

Artifact: `bench_vs_redis_standard.txt`
Artifact SHA256: `a3416a1cbe23a494d982fef5023135d75cdd5f04d2f24d32fc8a31d5e3972d42`

Median fr/redis ratios:

- set `1.05x`
- get `1.03x`
- incr `1.03x`
- lpush `0.94x`
- rpush `1.05x`
- lpop `1.10x`
- rpop `1.04x`
- sadd `1.04x`
- hset `1.02x`
- spop `1.01x`
- zadd `1.02x`
- lrange_100 `1.12x`
- mset `1.24x`

Verdict: no stable standard-command target below the `0.9x` parity threshold.

## Extended Parser/Event Sweep

Command:

```bash
python3 scripts/bench_vs_redis.py 26399 26400 --trials 5 --n 200000 --pipeline 16 --clients 50 --tests ping_inline,ping_mbulk,set,get,incr,lpush,rpush,sadd,hset,zadd,lrange_100,mset --bench legacy_redis_code/redis/src/redis-benchmark
```

Artifact: `bench_vs_redis_extended.txt`
Artifact SHA256: `5294081b0849c8e587ffbdf97cf4afd327901a19fe27710c56982711fe2e324c`

Only borderline row: `ping_inline 0.8996960509264438x`, with noisy trials `1.43, 0.49, 0.90, 1.74, 0.82`.

Focused confirmation:

```bash
python3 scripts/bench_vs_redis.py 26399 26400 --trials 9 --n 500000 --pipeline 16 --clients 50 --tests ping_inline --bench legacy_redis_code/redis/src/redis-benchmark
```

Artifact: `bench_vs_redis_ping_inline_focus.txt`
Artifact SHA256: `2b42d2de834f97f88e5e8b7c03d490a2b534576b87b14d759ef4fea829945a60`

Focused `ping_inline` median: `1.01x`, so the borderline extended row did not reproduce.

## Isomorphism / Golden Proof

No source code was changed in this pass.

- Ordering preserved: yes, no command behavior changed.
- Tie-breaking unchanged: yes, no command behavior changed.
- Floating-point: N/A.
- RNG: N/A.
- Golden outputs: existing benchmark/oracle outputs unchanged by this evidence-only closeout; artifact checksums above identify the evidence.

## Decision

Close `frankenredis-ohsk5.62` evidence-only. No non-overlapping parser/event/store target cleared the profile-backed Score>=2.0 gate. Pivot to the separate measured large-value framing gap, `frankenredis-largeval-bigbulk-zerocopy-qesp3`.
