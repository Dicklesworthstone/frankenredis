# Pass 187 Evidence Report: current SADD syscall floor

Bead: `frankenredis-ohsk5.53`

## Scope

No ready `[perf]` child bead was available after pass 186, so this pass
re-profiled current `main` and filed a short-lived evidence child under
`frankenredis-ohsk5`.

No production source lever was attempted or retained.

## Baseline

RCH build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-coralox-pass181-20260613T2204Z/target-coralox-pass187-baseline cargo build --profile release-perf -p fr-server -p fr-bench
```

Worker: `vmi1227854`.

Current binary hashes:

```text
920e482a50647a4ddf89e7dccfaffdf08dfb6a470f063040f353f935dad41730  target-coralox-pass187-baseline/release-perf/frankenredis
1d66909a3b7e07cfd2614bbf6dd2c6128c78e30a4c5a10ade729aa96b3804fd0  target-coralox-pass187-baseline/release-perf/fr-bench
```

## Dashboard

P16/C50/n300k best-of-3 vs vendored Redis:

```text
set    redis 854700   fr 925925   redis/fr 0.92x  fr-faster
get    redis 874635   fr 937500   redis/fr 0.93x  fr-faster
incr   redis 872093   fr 882352   redis/fr 0.99x  fr-faster
lpush  redis 958466   fr 964630   redis/fr 0.99x  fr-faster
rpush  redis 983606   fr 1003344  redis/fr 0.98x  fr-faster
sadd   redis 949367   fr 906344   redis/fr 1.05x  FR-SLOWER
hset   redis 804289   fr 785340   redis/fr 1.02x  FR-SLOWER
zadd   redis 414937   fr 514579   redis/fr 0.81x  fr-faster
spop   redis 1038062  fr 1010101  redis/fr 1.03x  FR-SLOWER
```

The largest measured standard-command residual was SADD at `1.05x`
Redis/FrankenRedis.

## Profile Evidence

`perf` is blocked on this host:

```text
/proc/sys/kernel/perf_event_paranoid = 4
```

Fallback profile used a GDB-owned child process and a SADD P16/C50/n5M
vendored `redis-benchmark` load.

Both captured main-thread samples landed in `epoll_wait`; writer threads were
waiting on their queues. No active store, parser, command-dispatch, or reply
encoding frame was captured.

`strace -f -c` on SADD P16/C50/n300k is throughput-distorted, but useful for
syscall shape:

```text
sendto      39.07%  18754 calls
epoll_wait  31.45%  11439 calls
recvfrom    16.76%  18808 calls
read         5.67%   1128 calls
openat       3.94%   1124 calls
epoll_ctl    0.15%    110 calls
```

The syscall counts are consistent with the already-reduced `epoll_ctl` path and
with one send/recv group per P16 benchmark batch. The profile does not justify
another SADD member-storage, inline-small, direct-reply, or single-command
parser micro-lever.

## Isomorphism

- Ordering preserved: yes. No production source changed.
- Tie-breaking unchanged: yes. No production source changed.
- Floating-point: N/A.
- RNG seeds: unchanged. The only random input was benchmark key selection.
- Golden output: no candidate output exists because no source lever was
  attempted. Existing current-main command semantics are unchanged by this
  evidence-only pass.

## Decision

Reject source edit for this pass. Score `0.0` because no profile-backed source
lever cleared the eligibility gate.

Next route: attack a deeper command-packet/event-loop/owned IO primitive with a
fresh profile and proof bundle. Do not repeat inline-small SADD storage,
integer/direct-reply encoding, output-buffer cursor, `epoll_ctl` reduction, or
per-command micro-batching variants.
