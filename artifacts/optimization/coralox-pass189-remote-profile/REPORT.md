# Pass 189 Evidence Report: perf-capable userspace profile attempt

Bead: `frankenredis-ohsk5.56`

## Scope

Evidence-only profiling pass after pass188 closed without a source lever. The
target stayed on the command-packet/event-loop/owned-I/O route, but no
production source was edited because no stable source hotspot was confirmed.

The shared checkout had an unrelated uncommitted `OBJECT ENCODING` source hunk,
so this pass used a detached clean worktree at:

```text
/data/projects/.scratch/frankenredis-coralox-pass189-20260613T2236Z
```

Base commit:

```text
44e9f2d521b637bc27f2bed8ee8b8fcb78b36fba
```

## Build

Initial `rch` build on `vmi1227854` failed because the detached worktree did not
contain the untracked Redis command metadata directory required by
`fr-command/build.rs`:

```text
legacy_redis_code/redis/src/commands: No such file or directory
```

After copying only that metadata directory into the scratch worktree, the same
crate-scoped build succeeded on `vmi1156319`:

```text
rch exec -- env CARGO_TARGET_DIR=target-coralox-pass189-baseline \
  cargo build --profile release-perf -p fr-server -p fr-bench
```

Binary hashes:

```text
1e9ca62c7ba2c9da847fba63206c500b13efa57506b6329b7c9ffdbae97b73bf  target-coralox-pass189-baseline/release-perf/frankenredis
d2c66d440ddeaa4f61ab16b8d28b57cf095fcef564c66c9b8c0ea9da981e1cb9  target-coralox-pass189-baseline/release-perf/fr-bench
```

## Benchmarks

The fixed-port dashboard script was avoided because an older Redis process was
still listening on `23961`. This pass used unique ports `24440/24441`.

Single-run P16/C50/n300k sweep:

```text
cmd    redis       fr          redis/fr
incr   877192.94   668151.44   1.3129
sadd   826446.25   837988.81   0.9862
spop   874635.56   920245.38   0.9504
lpush  840336.12   892857.12   0.9412
hset   779220.81   867052.06   0.8987
```

The `INCR` row was the only slower row, so it was repeated before any source
work:

```text
rep  redis       fr          redis/fr
1    735294.12   641025.62   1.1471
2    884955.81   937500.00   0.9440
3    712589.06   983606.56   0.7245
4    729927.00   931677.00   0.7835
5    821917.81   999999.94   0.8219
```

The apparent `INCR` residual did not reproduce: FrankenRedis was faster in four
of five confirmation reps, and best-of-confirmation also favored FrankenRedis.

## Profile Evidence

The `rch exec` non-compilation perf probe did not offload; it reported the same
local restriction:

```text
perf_event_paranoid=4
```

Direct `perf record` against the built server failed with status `255`:

```text
Access to performance monitoring and observability operations is limited.
perf_event_paranoid setting is 4
```

No syscall fallback was used as a source-edit trigger because pass188 already
captured the command-packet/event-loop syscall shape, and pass189 did not
confirm a stable command gap.

## Isomorphism

- Ordering: unchanged; no production source changed.
- Tie-breaking: unchanged; no production source changed.
- Floating point: N/A.
- RNG: unchanged; only benchmark key selection varied.
- AOF/replication ordering: unchanged; no production source changed.
- CLIENT REPLY suppression: unchanged; no production source changed.
- Golden output: no candidate golden was generated because no source lever was
  attempted.

## Decision

Evidence-only closeout. Score: `0.0`; no source lever cleared the
profile-backed target gate.

Next route: get a perf-capable userspace profile, or test a command-packet,
arena, or owned-I/O primitive only after fresh evidence names parser argv
allocation, packet metadata, ownership transfer, or batch I/O coordination as a
measured source row.
