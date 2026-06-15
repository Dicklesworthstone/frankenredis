# Rejected: reusable large SET value buffer

Bead: `frankenredis-ssgbf`

Lever tested: return the replaced heap `Vec<u8>` from owned plain `SET` and keep it on the client connection for reuse by the next compatible partial large-SET read.

Decision: reject. The change preserved behavior, but the clean `origin/main` benchmark did not clear the keep gate and regressed the targeted absolute SET throughput.

## Baseline

Base: `origin/main` at `ed834a116`.

Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-n3uyd-baseline cargo build --profile release-perf -p fr-server -p fr-bench`

Binary hashes:

```text
fb419289d5090eb3589e0fc565baffb02f5cbf0b62cd021e85c2152dcc90f1a0  frankenredis baseline
52495b66b87e4d8ffe14d248f087da3dc98339cd4a97b549e3e10ce93e206242  fr-bench baseline
e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7  redis-server oracle
```

Large-value gate:

```text
SET 64KiB   40509 op/s, 1.30x vs redis
SET 256KiB  13032 op/s, 0.74x vs redis
SET 1MiB     3307 op/s, 0.75x vs redis
```

## Candidate

Build: `rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-coralox-n3uyd-candidate cargo build --profile release-perf -p fr-server -p fr-bench`

Binary hashes:

```text
97e5848b12214efb1bc184da091930e0a8588dc0808e08ec465174b99554b16e  frankenredis candidate
8b1f012f0667c4bf61fb9735dc0581c5498aa4cd16f3dd864f3a12e47e22b3cc  fr-bench candidate
```

Large-value gate:

```text
SET 64KiB   29814 op/s, 1.17x vs redis
SET 256KiB   8972 op/s, 0.87x vs redis
SET 1MiB     2059 op/s, 0.58x vs redis
```

Candidate vs baseline absolute throughput:

```text
SET 64KiB   0.74x
SET 256KiB  0.69x
SET 1MiB    0.62x
```

Score is below zero for the targeted rows, so the source hunk is not kept.

## Behavior Proof

Split-frame raw RESP replay matched Redis, baseline, and candidate exactly.

```text
request_sha256: 363039aca5be4f5881f36ef1eb1931b7fe9c8211c2f3e480a9643f85cfd2e358
response_sha256: d5535640fcd28b4c0fdf3c6b634c8d6ad55ca530dbb73803648e979a08aef61d
response_len: 393326
commands: 14
large_set_sizes: [196608, 65536, 131072]
```

Isomorphism: the attempted source change only reuses an internal `Vec<u8>` capacity after a successful owned plain `SET`; Redis-visible replies, key bytes, value bytes, object encoding, dirty count, expiry clearing, propagation ordering, command ordering, tie-breaking, floating-point behavior, and RNG behavior are unchanged by construction. Malformed or gated owned SETs still fall back to the existing generic path.

Focused gates passed via `rch`:

```text
cargo test -j 1 -p fr-store set_plain_owned_reusing_buffer_returns_replaced_heap_string -- --nocapture
cargo test -j 1 -p fr-server large_plain_set_read_start -- --nocapture
cargo test -j 1 -p fr-runtime plain_set_owned_fast_path -- --nocapture
```
