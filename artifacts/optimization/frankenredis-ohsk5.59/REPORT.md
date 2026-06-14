# Pass 192 extended command sweep

Bead: `frankenredis-ohsk5.59`

Decision: evidence-only closeout. No source change was attempted.

## Baseline

RCH built current `origin/main` (`34c50e5baa1fbf8aa440df0d875e21f82ef71a76`) with:

```text
env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-59-current-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Worker: `vmi1152480`

Binary SHA256:

```text
2c35ff37ad7f124c2cd9716ce7f3c952a72e7ac660464697117f8ba52b20f989  /data/tmp/frankenredis-ohsk5-59-current-target/release-perf/frankenredis
22e33f48d4d2a0430b77bdc8a0d59e511c05cacb8d6627d1c742133230462277  /data/tmp/frankenredis-ohsk5-59-current-target/release-perf/fr-bench
```

## Extended Sweep

P16/C50/n300k best-of-3 command sweep:

```text
cmd          redis         fr   redis/fr
ping_inline    1075268     884955      1.22x  FR-SLOWER
ping_mbulk    1071428    1030927      1.04x  FR-SLOWER
set         712589     909090      0.78x  fr-faster
get         785340     872093      0.90x  fr-faster
incr        773195     606060      1.28x  FR-SLOWER
lpush       569259     441826      1.29x  FR-SLOWER
rpush       636942     436046      1.46x  FR-SLOWER
lpop        874635     977198      0.90x  fr-faster
rpop        980392     986842      0.99x  fr-faster
sadd        877192     857142      1.02x  FR-SLOWER
hset        802139     757575      1.06x  FR-SLOWER
spop        911854    1006711      0.91x  fr-faster
zadd        363196     439238      0.83x  fr-faster
zpopmin    1041666     983606      1.06x  FR-SLOWER
lrange_100    1038062     925925      1.12x  FR-SLOWER
lrange_300     974026     983606      0.99x  fr-faster
lrange_500    1013513    1098901      0.92x  fr-faster
lrange_600     934579     990099      0.94x  fr-faster
mset        187617     257510      0.73x  fr-faster
```

The multi-command script reuses the same server for all rows, so the large
list-push rows were treated as routing hints only. Fresh-server confirmations
invalidated them as source targets:

```text
rpush       redis 994035   fr 1046025   redis/fr 0.95x  fr-faster
lpush       redis 968992   fr 1089324   redis/fr 0.89x  fr-faster
incr        redis 905797   fr 925925    redis/fr 0.98x  fr-faster
ping_inline redis 1018329  fr 854700    redis/fr 1.19x  FR-SLOWER
```

## Profile

`perf_event_paranoid=4` blocked kernel perf.

Child-owned GDB sampling for both `RPUSH` and `PING_INLINE` landed on the same
shape: main thread in `epoll_wait`, writer workers parked on the writer-channel
receive path. The `RPUSH` GDB run measured FrankenRedis at `998801.44` req/s,
which confirms the extended-sweep `RPUSH` row was a shared-state artifact rather
than a fresh-server hot path.

PING_INLINE syscall profile under tracing:

```text
% time     seconds  usecs/call     calls    errors syscall
 80.11    1.316069      658034         2         2 futex
 12.39    0.203511          10     18751           sendto
  5.12    0.084102           4     18802           recvfrom
  1.51    0.024740          14      1765         1 epoll_wait
  0.05    0.000754           7       104           epoll_ctl
```

The send/recv counts are already near one syscall per P16 batch for 300k
requests. No parser, command-packet, allocation, branch-dispatch, or store frame
was captured as the dominant row.

## Behavior Proof

No production source changed.

Baseline inline transcript:

```text
PING\r\nQUIT\r\n -> +PONG\r\n+OK\r\n
```

Golden SHA256:

```text
9a6fe8bf0985c259d20c7b4667ac38a43c6a64dfe4ba494c016f0cde83893918  artifacts/optimization/frankenredis-ohsk5.59/baseline_inline_ping.resp
```

Ordering/tie-breaking/floating-point/RNG: unchanged because no source changed;
the confirmed residual has no FP or RNG behavior.

## Routing

Reject speculative parser or command-packet edits from this pass. The only
confirmed residual is `PING_INLINE` at `1.19x` Redis/FR, and the available
profile evidence names the epoll/send/recv boundary rather than a Rust hot
function. The next credible no-gaps primitive must first prove that a deeper
reactor/I/O model can improve this frontier without changing serial reply order.

Alien-graveyard candidates:

- `io_uring` submission/completion queues: only proceed after an explicit
  feasibility profile that compares epoll vs batched submission, records
  SQ/CQ utilization, and documents automatic epoll fallback for unsupported or
  restricted kernels.
- Region/slab command packets: only proceed if a fresh profile names allocation,
  command-packet ownership, or pending-output movement rather than the syscall
  floor.

Recommendation contract for the next bead:

```text
Change: profile-backed epoll-vs-io_uring reactor feasibility for PING_INLINE/P16
Hotspot evidence: PING_INLINE isolated redis/fr 1.19x; GDB epoll_wait; strace sendto=18751 recvfrom=18802 for n300k
Mapped graveyard sections: io_uring batching (§15.8), region/slab buffer ownership (§5.10)
EV score: 4 impact * 3 confidence * 3 reuse / (4 effort * 3 friction) = 3.0
Priority tier: A
Adoption wedge: feature-gated experimental reactor with epoll default
Budgeted mode: bounded SQ/CQ ring, registered buffer pool, epoll fallback on setup failure or queue exhaustion
Expected-loss model: minimize throughput gap + p99 tail while charging security/support friction for io_uring availability
Calibration + fallback trigger: fallback when io_uring_setup denied, CQE errors exceed zero, or p99 regresses
Isomorphism proof plan: golden inline RESP transcript, mixed PING/SET/GET serial-order replay, connection-close replay
p50/p95/p99 before/after target: PING_INLINE >=1.10x faster than current FR and no adjacent PING_MBULK/GET regression
Primary failure risk + countermeasure: buffer lifetime/ordering bugs; use slab indices and CQE ownership ledger
Rollback: feature flag off, epoll path unchanged
Baseline comparator: current epoll release-perf binary above
```
