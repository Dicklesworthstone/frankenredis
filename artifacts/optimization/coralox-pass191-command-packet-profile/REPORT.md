# Pass 191 profile report: command-packet / output-ownership route

Bead: `frankenredis-ohsk5.58`

Decision: evidence-only closeout. No source change was attempted.

## Baseline

RCH built the current source context with:

```text
env CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-pass191-baseline cargo build --profile release-perf -p fr-server -p fr-bench
```

Worker: `vmi1156319`

Binary SHA256:

```text
379678c3cdb788ec66d170a9bc5fcb1fadc7476ab1cd5a0b744425815c7840d7  target-coralox-pass191-baseline/release-perf/frankenredis
d8a19cd9a83739f4f03a4c13e948a54840a96aef00e7a79620063abc83846e8a  target-coralox-pass191-baseline/release-perf/fr-bench
```

Fresh P16/C50/n300k dashboard:

```text
cmd          redis         fr   redis/fr
set         789473     872093      0.91x  fr-faster
get         824175     977198      0.84x  fr-faster
incr        931677     847457      1.10x  FR-SLOWER
lpush       974026     993377      0.98x  fr-faster
rpush      1030927    1010101      1.02x  FR-SLOWER
sadd        887573     990099      0.90x  fr-faster
hset        833333     914634      0.91x  fr-faster
zadd        434153     482315      0.90x  fr-faster
spop       1048951    1060070      0.99x  fr-faster
```

## Profile

`perf_event_paranoid=4` blocked kernel perf. GDB sampling during an `INCR` P16/C50 workload landed in:

```text
std::net::tcp::TcpStream::write
mio::net::tcp::stream::TcpStream::write
try_flush() at crates/fr-server/src/main.rs:422
drive_client_output() at crates/fr-server/src/main.rs:5704
handle_readable() at crates/fr-server/src/main.rs:1972
main() at crates/fr-server/src/main.rs:1279
```

Writer workers were parked on the writer-channel receive path, so the fresh row points at inline output flushing rather than a worker-pool bottleneck.

The smaller 30k syscall count showed output already coalesced at roughly one send per pipeline batch:

```text
% time     seconds  usecs/call     calls    errors syscall
 70.12    0.097585          13      7400         1 epoll_wait
 12.64    0.017583          23       746           read
 11.79    0.016413           8      1900           sendto
  4.87    0.006774           3      1950           recvfrom
  0.51    0.000710           6       102           epoll_ctl
  0.07    0.000096          19         5           write
```

## Behavior Proof

No production source changed. Baseline raw RESP golden covered `INCR`, repeated integer increments, overflow error, non-integer error, and `QUIT`.

Golden SHA256:

```text
7383278c60dc04f688698ae875a8a199aabcd212b8d71b771b282011d8997cfd  artifacts/optimization/coralox-pass191-command-packet-profile/baseline_golden.resp
```

Ordering/tie-breaking/floating-point/RNG: no source change, and the sampled command path is serial per client with no FP or RNG behavior.

## Routing

Do not repeat the pass190 INCR HLL-cache invalidation family, and do not ship a speculative direct-encode/drain micro-lever from this pass. The current residual is small (`1.10x`) and output writes are already coalesced at pipeline granularity.

Next eligible source pass should require a fresh row that names a larger primitive: cross-client writer ownership, pending-output buffer movement, event-loop wake/registration overhead, slab/arena command packets, zero-copy RESP frame scanning, or branchless borrowed-command dispatch.
