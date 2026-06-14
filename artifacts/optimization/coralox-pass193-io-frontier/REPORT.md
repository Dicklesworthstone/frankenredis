# Pass 193: PING_INLINE Epoll Frontier Evidence

Bead: `frankenredis-ohsk5.60`
Base: `ad4bb2d0cc8a582c14dd4eb3ee2f824c7cb534b2`
Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-pass193-target CARGO_BUILD_JOBS=1 cargo build -j 1 --profile release-perf -p fr-server -p fr-bench`
Worker: `vmi1293453`

## Baseline binaries

```text
97f1bfc1dbeeac2965931ff3c59681cbc99787c6765d9b6122e6f5570b6abda0  frankenredis
d7a4412cab72e9a2c53490eb27c5d65b553da2d85faeb28eaece9aef602ff0ea  fr-bench
```

## P16/C50 adjacency baseline

Fresh unique-port best-of-3, `redis-benchmark -t ping,get -n 500000 -c 50 -P 16`.

```text
cmd          redis req/s   fr req/s      redis/fr
PING_INLINE   950570.31    862069.00       1.103x  FR-SLOWER
PING_MBULK   1043841.31   1082251.00       0.965x  fr-faster
GET          1043841.31   1098901.12       0.950x  fr-faster
```

Only inline PING remains slower, and it is a small 1.10x residual. Adjacent multibulk PING and GET are already faster in FrankenRedis, so this is not a broad parser, command-packet, store, or reply-encoding gap.

## Syscall profile

`perf_event_paranoid=4` blocks kernel perf in this environment, so pass193 used `strace -f -c` syscall attribution on the FrankenRedis server.

For 300k PING_INLINE requests at P16/C50:

```text
sendto      18751
recvfrom    18802
epoll_wait  11649
epoll_ctl     104
```

For 300k PING_MBULK requests at P16/C50:

```text
sendto      18751
recvfrom    18802
epoll_wait   6723
epoll_ctl     104
```

For 300k GET requests at P16/C50:

```text
sendto      18751
recvfrom    18802
epoll_wait   6361
epoll_ctl     104
```

The shape is already close to one send and one receive per P16 batch. PING_INLINE has materially more `epoll_wait` calls than PING_MBULK/GET, but the trace does not identify a concrete safe-Rust source hunk in parser, runtime, store, or output buffering that plausibly scores >=2.0.

## Golden parity

Raw TCP replay against FrankenRedis and Redis matched exactly.

```text
inline PING sha256:        64c2f2c744321d052076467905a0561f91e9a6de4e84441addbcc549cd71095c
mixed PING/SET/GET sha256: 8e7caddc39803a7fbbd886e8644c11a50c738a41e026bca7b2a324ea66992ee7
malformed close sha256:    e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
```

The mixed replay bytes are:

```text
+PONG\r\n+OK\r\n$1\r\nv\r\n
```

## Decision

No production source change attempted.

Score estimate for a feature-gated io_uring/reactor wedge from this evidence:

```text
Impact 1.10 * Confidence 0.60 / Effort 5.0 = 0.13
```

That fails the keep gate. Pass193 should close evidence-only and route deeper only after a fresh profile names a larger implementation surface such as a confirmed event-loop scheduling row, queue wakeup imbalance, command-packet allocation row, or a feature-gated reactor prototype with much higher measured upside.

