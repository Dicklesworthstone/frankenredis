# Pass 188 - command/event-loop owned-I/O profile closeout

- Bead: `frankenredis-ohsk5.54`
- Base commit: `1c41688fd6de8892ef22a020938955273ba6b85e`
- Target: SADD P16/C50 residual after pass187 syscall-floor profile
- Decision: evidence-only, no production source lever

## Baseline

RCH built the baseline binaries with:

```bash
env CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-pass188-baseline \
  cargo build --profile release-perf -p fr-server -p fr-bench
```

Artifact hashes:

```text
2dd52c910a385853a9158b6249427694c706af9fb59d0af6d9bfde4e5be17a83  target-coralox-pass188-baseline/release-perf/frankenredis
32351fa0168e5c4c7f83350dcab28e2eae0fc98aa8dbfe324c065786612b9df8  target-coralox-pass188-baseline/release-perf/fr-bench
```

Fresh P16/C50/n300k dashboard, best-of-3, selected SADD as the largest standard-command residual:

```text
cmd          redis         fr   redis/fr
set         773195     937500      0.82x  fr-faster
get         773195     949367      0.81x  fr-faster
incr        835654     828729      1.01x  FR-SLOWER
lpush       983606    1060070      0.93x  fr-faster
rpush       977198    1071428      0.91x  fr-faster
sadd        909090     864553      1.05x  FR-SLOWER
hset        802139     826446      0.97x  fr-faster
zadd        421940     492610      0.86x  fr-faster
spop        993377    1071428      0.93x  fr-faster
```

Focused SADD baseline using vendored `redis-benchmark`:

```text
SADD P16/C50/n300k: 810810.81 requests per second, p50=0.567 msec
```

## Profile Evidence

`/proc/sys/kernel/perf_event_paranoid` was `4`, so kernel `perf` sampling was unavailable. The pass used child-owned GDB and strace evidence instead.

GDB-owned SADD P16/C50/n5M sample:

```text
SADD: 918105.00 requests per second, p50=0.727 msec
main thread: epoll_wait via mio::Poll::poll
writer threads: parked on writer-queue receive/futex waits
```

No active parser, store, SADD-member, or reply-encoding frame was captured. That argues against another SADD storage/direct-reply/parser micro-lever.

Tracing-distorted SADD P16/C50/n500k run:

```text
SADD: 190403.66 requests per second, p50=4.071 msec

% time     seconds  usecs/call     calls    errors syscall
 80.01    1.953206      976603         2         2 futex
 13.40    0.327079          10     31251           sendto
  5.85    0.142902           4     31302           recvfrom
  0.73    0.017930          13      1360         1 epoll_wait
  0.00    0.000121          24         5           write
```

The futex time is idle/wait time from traced helper threads, not a production compute hotspot. The meaningful shape is still the expected P16 syscall floor: sendto/recvfrom batches plus epoll waits. Writer workers were parked, so the current inline flush path is already handling this profile.

## Isomorphism And Golden

No production source changed in this pass. Ordering, tie-breaking, floating-point behavior, RNG behavior, AOF/replication ordering, CLIENT REPLY suppression, and RESP output semantics are unchanged.

Deterministic SADD transcript hash against the baseline binary:

```text
ac08ea602c0a70f58e925f59d3544962c8a942d100827a1a48c9f76708026372  golden_sadd_output.resp
```

Transcript bytes:

```text
:0
:3
:1
:4
:1
:0
:5
+PONG
```

## Score And Route

No source lever was attempted. Score: `0.0` because the profile does not identify a source change that can clear the Score >= 2.0 keep gate.

Rejected route for this pass:

- SADD inline-small/member storage variants: pass174 already rejected and no store frame appeared here.
- Direct integer reply encoding: prior direct-reply families failed and no encoder frame appeared here.
- Output cursor or epoll_ctl reduction: pass187 already showed epoll_ctl is near floor.
- Per-command micro-batching: no evidence that command-level batching, rather than syscall/batch floor, is the current limiter.

Next route:

- Prefer a perf-capable worker or lower `perf_event_paranoid` environment to get userspace rows before another source edit.
- If SADD or INCR remains a true gap, attack a fundamentally different command-packet/arena/owned-I/O primitive only when fresh profile evidence shows parser argv allocation, packet metadata, or ownership transfer as the measured row.
- Do not repeat the current event-loop writer-policy family unless a new profile shows active writer work rather than parked workers.
