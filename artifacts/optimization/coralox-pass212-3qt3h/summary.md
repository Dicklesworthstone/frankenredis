# Pass 212 Summary - `frankenredis-3qt3h`

## Decision

Rejected. The per-connection reusable large-SET read slab preserved behavior but did
not clear the performance gate. The source hunk was removed; no production code
change remains.

## Target

`[perf] Large SET reusable read slab`

Profile-backed evidence from the bead: large `SET 262144B` still showed more
FrankenRedis `recvfrom` calls than Redis, while large `GET` was already faster.
The candidate primitive read the direct owned-SET continuation into a lazy
per-connection initialized slab, then appended to the pre-capacity value Vec.

## Baseline

Current pushed main `31798d0b8e1ab201392c2badcbb60727ce3c00a5` built with:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-coralox-pass212-3qt3h-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Focused Redis-relative gate:

```text
SET 262144B: fr=13130 op/s, redis=14682 op/s, ratio=0.89x
SET 1048576B: fr=3264 op/s, redis=3325 op/s, ratio=0.98x
GET rows: all faster than Redis, 1.68x-2.60x on large rows
```

## Candidate

Candidate built with:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-coralox-pass212-3qt3h-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Redis-relative gate was mixed:

```text
SET 262144B: fr=12807 op/s, redis=17738 op/s, ratio=0.72x
SET 1048576B: fr=4931 op/s, redis=4070 op/s, ratio=1.21x
```

Paired baseline-vs-candidate hyperfine:

```text
SET 262144B, 6000 requests: baseline 1.885s +/- 0.035s, candidate 1.969s +/- 0.096s
Result: baseline 1.04x +/- 0.05 faster

SET 1048576B, 2500 requests: baseline 5.472s +/- 1.792s, candidate 3.709s +/- 0.244s
Result: candidate 1.48x +/- 0.49 faster
```

The primary target row regressed, and the 1MiB win was high-variance. With the
target-row regression, confidence penalty, and no clean geomean keep, Score < 2.0.

## Behavior Proof

Raw split large-SET RESP proof matched Redis, baseline, and candidate byte-for-byte:

```text
request_sha256 d507f6a6c6958d7f175f9332aa104de75859f6be99f3557d5c041f42451216f5
redis_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
baseline_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
candidate_response_sha256 ee784c22fa4ad7cd4432d1ee0e83c41a2e30b7e148a1e5e06b702c7af25d505a
```

Isomorphism: command ordering, RESP boundaries, stored bytes, trailer validation,
query-buffer accounting, replies, expiry/propagation/dirty semantics, floating
point, tie-breaking, and RNG paths were unchanged.

## Next Route

Do not retry the reusable initialized slab for the 256KiB row without new profile
evidence. The mixed result points toward a size-specific deeper primitive: a safe
zero-copy/less-copy bulk ingress design, batched socket read scheduling, or a
different owned-value construction strategy rather than moving bytes through an
extra slab copy.
