# Pass 211 Summary - `frankenredis-ohsk5.63`

## Decision

Evidence-only. No source edit was made.

The current custom cold/read sweep initially showed `ZCARD z` as the clearest unowned residual:

```text
zcard redis=1086956 req/s fr=970873 req/s redis/fr=1.12x
```

That target did not survive a focused baseline on the rch-built current binary:

```text
zcard redis=1006711 req/s fr=1102941 req/s redis/fr=0.913x
```

FrankenRedis was faster in the focused run, so the proposed `Z` bucket cardinality recognizer hoist was not attempted.

## Current Profile Inputs

- `artifacts/optimization/coralox-pass211-current-profile/perf-gap-dashboard.txt`: default P16 dashboard; only `INCR` was slower, Redis/fr `1.03x`.
- `artifacts/optimization/coralox-pass211-current-profile/bench-vs-redis-extended.txt`: extended interleaved sweep; parity-or-faster on all tested commands.
- `artifacts/optimization/coralox-pass211-current-profile/bigcmd-bench.txt`: large-command sweep; only `GETRANGE 1MB` was slower, Redis/fr `1.04x`.
- `artifacts/optimization/coralox-pass211-current-profile/custom-cold-sweep.txt`: custom cold/read sweep; `ZCARD` was the largest apparent row but was invalidated by focused baseline.

## Profiling Limitation

Kernel perf was unavailable on this host:

```text
perf_event_paranoid=4
profile_hot_path perf data size=0
```

No stack-attributed production lever was attempted.

## Next Route

Do not hoist ZCARD without fresh focused evidence. Continue with either a focused confirmed residual or a new alien structural profile once a real gap appears.
