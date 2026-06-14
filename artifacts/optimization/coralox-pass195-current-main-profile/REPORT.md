# Pass 195 Report - current-main residual sweep

Bead: `frankenredis-x4nzl`
Base: `4dfd1ac5f6fbdd0571a17ff1abb6da5f0d114283`

## Baseline

RCH release-perf build:

```text
frankenredis 538e44f525c2c89416d61f706fc945b4b5d44ee6dc6e924a02161841a34e032c
fr-bench     3812ac396923f9d1aa87274240ad035b2606343b300499f08214a06e9b4e35e2
```

Command:

```text
scripts/perf_gap_dashboard.sh --bin /data/tmp/frankenredis-pass195-current-release-target/release-perf/frankenredis --no-build -n 300000 -P 16 -c 50 --reps 5 --cmds "ping_inline ping_mbulk set get incr lpush rpush lpop rpop sadd hset spop zadd zpopmin lrange_100 lrange_300 lrange_500 lrange_600 mset"
```

## Result

No source lever was attempted. The largest current standard-command residual was
only `lrange_500` at `1.07x` Redis/FR; `ping_mbulk` was `1.06x`, `rpush` was
`1.04x`, and PING_INLINE is now fr-faster after the prior kept pass.

```text
ping_inline  redis=1023890 fr=1052631 redis/fr=0.97x
ping_mbulk   redis=1132075 fr=1067615 redis/fr=1.06x
rpush        redis=1045296 fr=1006711 redis/fr=1.04x
lrange_500   redis=1041666 fr=977198  redis/fr=1.07x
mset         redis=237341  fr=326441  redis/fr=0.73x
```

Score for production source: `0.0`; no profile-backed target clears the
`Score >= 2.0` keep gate.

## Isomorphism

No production source changed. Command ordering, tie-breaking, floating-point,
RNG, replication ordering, and reply bytes are unchanged by this evidence-only
pass.

## Next Route

Do not repeat exact inline PING. The next pass needs a fresh alien/deeper
primitive only after a profile names a concrete source surface, likely outside
the already-flat standard P16 command set.
