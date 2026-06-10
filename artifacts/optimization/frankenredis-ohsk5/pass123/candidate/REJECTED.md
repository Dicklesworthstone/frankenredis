# Pass123 Rejection: Writer Queue Sharding

Bead: `frankenredis-ohsk5.20`

## Profile-backed target

Current-main GET/P16/C50 profile remained kernel/syscall heavy:

- Baseline hyperfine GET/P16/C50/1M: `895.9 ms +/- 48.8 ms`
- Profile GET/P16/C50/3M: `1,172,512.67 ops/sec`, p50 `640us`, p95 `963us`, p99 `1290us`
- `perf` flat top rows: `[vdso]`/time symbols, kernel rows, `Store::get_string_bytes` `1.49%`, `execute_plain_get_borrowed_into` `1.40%`, `parse_command_args_borrowed_into` `1.30%`, `CommandHistogramTracker::record_canonical_with_kind` `1.07%`
- `strace -f -c` GET/P16/C50/300k: `futex` 48,941 calls / 50.97%, `sendto` 18,790 / 17.05%, `epoll_ctl` 37,735 / 9.22%, `write` 18,795 / 8.75%, `recvfrom` 18,880 / 7.17%, `epoll_wait` 14,158 / 5.10%

## Candidate

Replace the single shared writer job receiver guarded by `Arc<Mutex<mpsc::Receiver<WriterJob>>>` with per-worker bounded `mpsc::SyncSender` shards probed round-robin from the main event loop. This targets the traced `futex`/writer-handoff surface while preserving:

- one in-flight writer job per client (`writer_in_flight_bytes` gate unchanged), so per-client reply ordering is unchanged;
- encoded reply bytes unchanged (`flush_writer_job`, completion handling, and `write_buf` ownership unchanged);
- cross-client completion ordering remains non-contractual as before;
- no floating-point, RNG, command tie-breaking, expiry, parser, or store semantics touched.

## Behavior proof

Golden TCP transcript over mixed `PING`/`SET`/`GET`/`QUIT` plus 2049 GET replies matched exactly:

- request sha256: `fcc1b082f9f35022a506bb7f29973ab484631322e61267e3e528416e2b27e9a4`
- baseline response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- candidate response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- response bytes: `18465` baseline and candidate

## Benchmark result

Rejected because the candidate was slower in both same-window orders:

- Paired: baseline `940.2 ms +/- 39.2 ms`, candidate `974.5 ms +/- 51.8 ms`; baseline `1.04x +/- 0.07` faster.
- Reversed: candidate `942.8 ms +/- 24.3 ms`, baseline `915.2 ms +/- 80.3 ms`; baseline `1.03x +/- 0.09` faster.

Score: below `2.0`; source hunk removed.

## Route

Queue sharding did not move the wall clock despite the futex trace. The next pass should skip queue-topology microlevers and attack a deeper primitive: a true writer-owned per-client outbox that keeps stream ownership and coalesces queued chunks before completion, or pivot to command accounting/layout if the fresh profile still keeps `CommandHistogramTracker`/time sampling visible.
