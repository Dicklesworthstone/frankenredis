# Pass124 Rejection: Writer-Owned Outbox

Bead: `frankenredis-ohsk5.21`

## Profile-backed target

Current main `eeb828903` still showed a writer/syscall-heavy GET/P16/C50 surface:

- Baseline GET/P16/C50/1M: `879.1 ms +/- 16.0 ms`.
- Profile GET/P16/C50/3M: `1,417,678.19 ops/sec`, p50 `511us`, p95 `832us`, p99 `1148us`.
- `strace -f -c` GET/P16/C50/100k: `futex` 15,253 calls / 51.43%, `sendto` 6,290 / 16.69%, `epoll_ctl` 12,735 / 9.08%, `write` 6,295 / 8.90%, `recvfrom` 6,380 / 6.93%, `epoll_wait` 4,748 / 4.98%.

## Candidate

The candidate moved writer ownership into token-sharded writer workers:

- writer workers retained cloned stream state by token;
- main-loop output could append while prior writer bytes remained in flight;
- writer workers drained queued chunks with `write_vectored`;
- a blocked writer state preserved FIFO ordering across partial writes;
- explicit close jobs cleaned cached writer streams.

Isomorphism argument: command execution stayed serial on the main thread; workers received already-encoded reply bytes only. Per-client FIFO order was preserved by token-sharded queues and `writer_in_flight_bytes` accounting. No parser, store, expiry, commandstats ordering, floating-point, RNG, or tie-breaking semantics changed.

## Behavior proof

Golden TCP transcript over mixed `PING`/`SET`/`GET`/`QUIT` plus 2049 GET replies matched exactly:

- request sha256: `a317957fa776cb442a4cc0d6274ff0f5b4396369aed1d88f79d8746e84b16190`
- baseline response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- candidate response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- response bytes: `18465` baseline and candidate

Focused gates passed while the candidate was applied:

- `cargo fmt -p fr-server -- --check`
- `cargo check -p fr-server --all-targets`
- `cargo clippy -p fr-server --all-targets -- -D warnings`
- release-perf `fr-server`/`fr-bench` build

## Benchmark result

Rejected because the candidate was slower in both orders:

- Paired: baseline `794.3 ms +/- 52.7 ms`, candidate `853.4 ms +/- 31.6 ms`; baseline `1.07x +/- 0.08` faster.
- Reversed: candidate `905.4 ms +/- 23.0 ms`, baseline `836.1 ms +/- 43.6 ms`; baseline `1.08x +/- 0.06` faster.

Score: below `2.0`; source hunk removed.

## Route

Do not repeat writer queue topology, writer ownership, wake coalescing, tiny sync flush, worker-count fanout, direct parser shortcuts, or output-buffer capacity reuse. The next pass should pivot to the non-writer rows still visible in the profile: command accounting/layout (`CommandHistogramTracker::record_canonical_with_kind`), time sampling/vdso rows, or a broader commandstats batching primitive that preserves INFO/commandstats/slowlog/errorstats semantics.
