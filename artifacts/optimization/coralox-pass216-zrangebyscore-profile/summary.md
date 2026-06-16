# Pass 216 ZRANGEBYSCORE Confirmation

## Scope

- Bead: `frankenredis-abkxq`
- Source head: `b6215ebf703a55352303f8d377d91ebe81e818b6`
- RCH worker: `vmi1156319`
- Build command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- FrankenRedis binary sha256: `981ac3cb0046e8ae7ae24e6ecd802fcbc3c299e5e2a0f5fb012d6ca29a87740d`
- `fr-bench` binary sha256: `b55e9ca0e1daba93da826250cb5663f6abd9a3059c994b4ba6c22b9d5bd5d51b`
- Redis server oracle sha256: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
- Redis benchmark oracle sha256: `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`

## Baseline

Routing evidence from `coralox-pass215-residual-profile` showed
`ZRANGEBYSCORE bigz 100 200` at `fr/redis=1.22x`. This pass rebuilt current
head and reran that row on fresh Redis and FrankenRedis servers with identical
200k-member sorted sets.

Focused confirmation result:

- Trials: 11
- Iterations per trial: 8
- Redis median: `2.655845 ms`
- FrankenRedis median: `2.786539 ms`
- Median `fr/redis`: `1.049210x`
- Mean `fr/redis`: `1.057136x`

The focused row did not reproduce as a source-worthy performance target.

## Behavior Proof

No production source changed.

Raw RESP output matched exactly:

- `ZRANGEBYSCORE bigz 100 200`: sha256
  `f2d67728d4add2f623ead9e0219b5ab5e8bfbe9b594fba3bb484f4d250ed405c`,
  length `26267` bytes on both engines.
- `ZRANGEBYSCORE bigz 100 100 WITHSCORES`: sha256
  `fa4578201e1e9005e0622aa99591e2b65b45e474268c75902c608483a29fcd17`,
  length `454` bytes on both engines.

Isomorphism: ordering and duplicate-score tie behavior match Redis for the
measured range; no floating-point, RNG, expiry, persistence, replication, dirty
count, or command-stats path changed because no source changed.

`perf_event_paranoid=4` blocks kernel perf attribution. Two `strace -c` attempts
completed the workload but did not flush a syscall counter table on termination,
so no syscall summary is used as evidence.

## Decision

Close `frankenredis-abkxq` evidence-only. The routing row fell from `1.22x` to
about `1.05x` on focused current-head confirmation, which is not enough to
justify a one-lever source child under the Score >= 2.0 rule.

Next route: stop ZRANGEBYSCORE micro-tuning without a fresh below-gate row.
Use the next pass to profile a stronger alien primitive, preferably a
profile-capable event-loop/batched-I/O or region/slab command-storage target
that names a dominant data-plane cost before source work.
