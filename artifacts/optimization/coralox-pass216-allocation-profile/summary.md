# Pass216 allocation-profile closeout

Bead: `frankenredis-ru15t`

Current source: `b6215ebf703a55352303f8d377d91ebe81e818b6`

RCH-built binaries:

- `frankenredis`: `6a7e44681838532ffa1ee8024841c86c1e2f87a4d9ad51f9b01b3d88f6ba9216`
- `fr-bench`: `70a59418c59c475f395ba2537c473dda1f8424b08463a1fec2bfb10b7bb5f6eb`
- Redis `redis-benchmark`: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`
- Redis `redis-server`: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

## Allocation probe

Mimalloc release stats emitted no allocation lines; `mimalloc-lines.txt` is empty with sha256 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`. This matches the mimalloc documentation note that stats are available in debug builds. Therefore no allocation count or allocation stack scaled with request count, and no parser/argv/command-packet storage lever is profile-backed by this pass.

## Syscall fallback profile

`perf_event_paranoid=4` blocks kernel perf. The fallback `strace -f -c` probe attributes the current binary to socket I/O rather than allocator/parser storage.

PING_INLINE P16/n300k under strace:

- Throughput: `216606.50 requests per second`
- Time: `sendto 42.98%`, `epoll_wait 27.68%`, `recvfrom 18.94%`
- Calls: `sendto 18751`, `recvfrom 18803`, `epoll_wait 11570`

SET P16/n300k under strace:

- Throughput: `136425.66 requests per second`
- Time: `sendto 55.39%`, `recvfrom 23.43%`, `epoll_wait 15.04%`
- Calls: `sendto 18751`, `recvfrom 18803`, `epoll_wait 6108`

## Decision

No source lever was attempted. The region/slab command-packet hypothesis fails the Score>=2.0 gate here because the pass did not produce a stable below-gate timing row or a scalable allocation stack/count. Behavior proof is no-op isomorphism: source, ordering, tie-breaking, floating-point, RNG, expiry, persistence, replication, command stats, and reply bytes are unchanged.

Next route: require a debug-allocation build or stack-capable profile before editing parser/command-packet storage; otherwise pursue epoll/socket-batch attribution or another fresh ready `[perf]` bead with a stable below-gate row.
