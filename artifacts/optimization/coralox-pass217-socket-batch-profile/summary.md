# Pass217 high-pipeline socket-batch profile

Bead: `frankenredis-diprk`

Current source: `773a3a47db7b3038bebe65bc29bb4cb5aea3ff96`

RCH-built binaries:

- `frankenredis`: `048e6abc6307f79261f7a3ba6b82c8d807ed1dde0d5fb8cd1e300534c0056d16`
- `fr-bench`: `7274b476cce9b14df303e534ec67b3a9849f5a2c33fa022880f010dfa9c05934`
- Redis `redis-benchmark`: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`
- Redis `redis-server`: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

## Redis-relative timing

Workload: P64/C200/n300k, three reps per command, same local host, vendored Redis 7.2.4.

| Command | Redis median req/s | FrankenRedis median req/s | FR / Redis |
| --- | ---: | ---: | ---: |
| PING_INLINE | 2,419,613.00 | 3,261,217.50 | 1.348x |
| SET | 2,083,555.62 | 2,439,284.50 | 1.171x |
| GET | 2,307,938.50 | 3,125,333.25 | 1.354x |

The high-pipeline socket-batch target is invalidated for source work: no tested row is below Redis, and no row clears the Score>=2.0 implementation gate for an io_uring or event-loop rewrite.

## Syscall fallback profile

`perf_event_paranoid=4` blocks kernel perf. The fallback `strace -f -c` probe confirms the P64 path is already batched near the pipeline size.

FrankenRedis PING_INLINE P64/n300k under strace:

- Throughput: `824263.75 requests per second`
- Time: `sendto 52.41%`, `recvfrom 24.00%`, `epoll_wait 2.04%`
- Calls: `sendto 4689`, `recvfrom 4891`, `epoll_wait 52`

FrankenRedis SET P64/n300k under strace:

- Throughput: `737179.38 requests per second`
- Time: `sendto 52.86%`, `recvfrom 22.73%`, `epoll_wait 2.14%`
- Calls: `sendto 4689`, `recvfrom 4891`, `epoll_wait 54`

## Decision

No source lever was attempted. The alien-graveyard io_uring/SQ batching primitive remains a future route only if a fresh profile shows a stable below-gate Redis-relative row plus syscall attribution that cannot be explained by already-batched P64 send/recv counts.

Behavior proof is no-op isomorphism: source, ordering, tie-breaking, floating-point, RNG, expiry, persistence, replication, command stats, and reply bytes are unchanged.
