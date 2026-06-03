# frankenredis-0grtm: fr-server write-interest hot path

## Profile-backed target

Post-`90810a8eb` SET p16 profiling showed the current bottleneck in the
syscall/event-loop path. Baseline strace for 500,000 SET requests, 50 clients,
pipeline 16:

- `epoll_wait`: 10,212 calls, 42.46% traced time
- `sendto`: 31,250 calls, 27.84% traced time
- `recvfrom`: 31,301 calls, 11.49% traced time
- `epoll_ctl`: 31,404 calls, 9.19% traced time

Code inspection found that `process_buffered_frames` inserts the token into
`write_tokens` before `handle_readable` attempts its immediate nonblocking
flush. A successful hot-path flush then sees synthetic "prior" write interest
and calls `reregister(..., Interest::READABLE)`, producing an `epoll_ctl` call
per request batch.

## One lever

Capture `had_write_interest` before dispatching buffered frames. The normal
successful immediate-flush path still removes the token after flushing, but it
only reregisters READABLE if WRITABLE was armed before this read event. Partial
writes, budget exhaustion, QUIT replies, deferred clients, blocked-client
unblocks, pub/sub delivery, monitor delivery, and replication delivery keep
their existing write-arm behavior.

## Benchmark

Harness: `fr-bench` against `frankenredis`, 500,000 SET requests, 50 clients,
pipeline 16, keyspace 10,000, datasize 3. Baseline and candidate were built via
`rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.

- Baseline hyperfine: `1.624 s +/- 0.041 s`
- Candidate hyperfine: `1.571 s +/- 0.062 s`
- Speedup: `1.034x`
- Candidate syscall check: `epoll_ctl` dropped from 31,404 calls to 154 calls
  on the same 500,000 SET p16 traced workload.

Score: `Impact 2.4 * Confidence 0.9 / Effort 0.9 = 2.4`; keep gate passes.

## Behavior proof

Golden raw RESP trace sent one pipelined byte stream:

1. `SET icy0grtm:k1 v1`
2. `GET icy0grtm:k1`
3. `INCR icy0grtm:n1`
4. `MGET icy0grtm:k1 icy0grtm:n1`
5. `PING`
6. `QUIT`

Baseline and candidate reply bytes are identical:

`3b95e455b5c2fc4f6ba1633bb0c94601d9ae74d66ceaf57642a68ff5067b15b7`

Ordering is preserved because the change only affects whether an already
drained socket is reregistered after the replies have been encoded and flushed.
Tie-breaking is unchanged: client tokens, event iteration order, blocking
unblock order, pub/sub delivery order, monitor delivery order, and replication
delivery order are untouched. Floating-point and RNG behavior are not involved.

## Artifacts

- `baseline-set-p16-hyperfine.txt`
- `candidate-set-p16-hyperfine.txt`
- `baseline-strace-set-p16.txt`
- `candidate-strace-set-p16.txt`
- `golden-baseline.resp`
- `golden-candidate.resp`
- `golden-resp.sha256`
