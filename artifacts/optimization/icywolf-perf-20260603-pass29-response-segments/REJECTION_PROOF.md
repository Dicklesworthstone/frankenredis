# Pass 29 Rejection Proof - Response Segment Queue

## Target

- Bead: `frankenredis-response-segment-ok-gbypt`
- Profile-backed hotspot: SET pipeline=16 response path after pass28. Direct baseline was `285522.74 ops/sec`, p99 `5015 us`; baseline hyperfine was `1.689s +/-0.051s`. Prior strace/GDB evidence kept the target on the `sendto` / `recvfrom` / `epoll_wait` cluster plus `encode_client_reply` for the `+OK\r\n` response stream.
- Alien primitive attempted: submission-queue style response segmentation. The candidate queued RESP2 `+OK\r\n` as static segments and flushed them with safe `write_vectored`, materializing before any non-OK response bytes.

## Candidate Gate

- Source edit was built remotely with `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.
- Main-tree compile gate before A/B: `rch exec -- cargo check -p fr-server --all-targets` passed.
- Source hunk was removed after rejection; final `git diff -- crates/fr-server/src/main.rs --exit-code` passed.

## Performance

- Fresh direct baseline: `287244.61 ops/sec`, p50 `2531 us`, p95 `3963 us`, p99 `4623 us`.
- Fresh direct candidate: `232391.03 ops/sec`, p50 `3337 us`, p95 `4491 us`, p99 `5259 us`.
- Paired hyperfine baseline: `1.660s +/-0.028s`.
- Paired hyperfine candidate: `1.772s +/-0.049s`.
- Verdict: baseline ran `1.07x +/-0.03x` faster than the candidate. Score is below the `2.0` keep gate; source is rejected.

## Behavior Proof

- Authoritative golden trace files: `baseline-golden5.resp` and `candidate-golden5.resp`.
- Golden command stream covered mixed ordered replies: `SET` (`+OK`), `GET`, `INCR`, `GET`, `MGET`, `DEL`, `PING`.
- Golden SHA-256 matched byte-for-byte: `514f74a29375f5a12274a3712dbe212a679488837cc46771446846c7ab5543e6`.
- Earlier `*-golden.resp`, `*-golden2.resp`, `*-golden3.resp`, and `*-golden4.resp` are retained only as discarded harness attempts: shell `$` expansion, EOF/half-close behavior, and one state-polluted retry. They are not the accepted parity proof.
- Ordering/tie-breaking: candidate materialized pending OK segments before non-OK frames, preserving response order in the accepted golden trace.
- Floating-point/RNG: not involved.
- Final tree behavior: no candidate source is retained, so the committed code path is identical to the pushed baseline.

## Next Attack

The response-segment `write_vectored` model regressed because it traded contiguous reply coalescing for scatter/gather syscall setup. The next profile-backed primitive should move deeper than response encoding: zero-copy RESP frame scanning or event-loop/read batching for the `recvfrom` side, rather than another `+OK` representation tweak.
