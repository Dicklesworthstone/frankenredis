# Pass 213 Summary - Current Profile And `frankenredis-n3uyd` Closeout

## Decision

Evidence-only profile pass plus stale perf-bead closeout. No production source
change was attempted because the fresh sweeps did not show a scoreable unowned
target, and `frankenredis-n3uyd` pointed to a lever already rejected by its own
referenced artifact.

## Current Head

```text
8e88b4500117d1d1c075d5c90fcb6710a6c7c73e
```

RCH build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-coralox-pass213-current-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Binary SHA256:

```text
frankenredis 0ca6eb41a68812bdcf7e99beeebb48099366aba1a639f8338a75c75641884dd6
fr-bench     eec3a27fac4d67bffcdd4c45a83247dd0c141c9c773a1a290750637c13a1de57
```

## Fresh Sweeps

Default P16/C50/n300k best-of-3 dashboard:

```text
set   redis/fr 0.93x  fr-faster
get   redis/fr 0.84x  fr-faster
incr  redis/fr 0.97x  fr-faster
lpush redis/fr 1.00x  fr-faster
rpush redis/fr 1.03x  FR-SLOWER
hset  redis/fr 1.02x  FR-SLOWER
spop  redis/fr 1.04x  FR-SLOWER
```

Large-value gate:

```text
SET 262144B  fr=18052 op/s  redis=19017 op/s  ratio=0.95x
SET 1048576B fr=4281 op/s   redis=4111 op/s   ratio=1.04x
GET rows all faster than Redis, including 262144B=1.95x and 1048576B=1.79x.
```

Extended C-client interleaved sweep:

```text
All tested commands were median >=0.9x vs Redis.
Lowest medians: incr=0.95x, set=0.96x, zadd=0.98x, hset/spop=0.99x.
```

Large/compute command sweep under visible concurrent load:

```text
GETRANGE 1MB redis=1.317ms fr=1.410ms fr/redis=1.07x
SRANDMEMBER -100 redis=0.145ms fr=0.158ms fr/redis=1.09x
Most large set/zset/list operations were faster or parity.
```

The big-command sweep is routing-only because another benchmark process was
visible concurrently.

## `frankenredis-n3uyd`

`frankenredis-n3uyd` was open but blocked by `frankenredis-ohsk5`, and its
description pointed directly at `artifacts/optimization/coralox-pass208-value-reuse/`.
That referenced artifact already rejected the exact replaced heap-string buffer
reuse lever:

```text
baseline SET 262144B 13032 op/s -> candidate 8972 op/s
baseline SET 1048576B 3307 op/s -> candidate 2059 op/s
candidate was 0.69x / 0.62x of baseline on the target rows
```

The pass208 golden proof also matched Redis/baseline/candidate exactly. I closed
`n3uyd` with `br close --force` as stale rejected tracker debt. No source hunk
was applied in pass213.

## Next Route

Do not repeat large-SET replaced-buffer reuse, reusable read slabs, static chunk
tuning, or GETRANGE/SRANDMEMBER micro-hunks without a stronger fresh profile.
The next perf pass should either coordinate on `frankenredis-ohsk5` with IcyWolf
or wait for a new unblocked, profile-backed child with a real residual.
