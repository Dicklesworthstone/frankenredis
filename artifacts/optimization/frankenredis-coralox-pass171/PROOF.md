# Pass 171 HSET Commandstats Direct Bucket Rejection

## Target

- Bead: HSET commandstats direct histogram bucket trial.
- Profile-backed route: current-main P16/C50 dashboard selected HSET as the largest measured standard-row residual, `redis 816326 req/s` vs `fr 732600 req/s` (`redis/fr 1.114x`).
- Prior evidence: `CommandHistogramTracker::record_canonical_with_kind` already documents generic commandstats map lookup as visible on HSET while GET/SET/LPUSH/RPUSH/SADD use direct buckets.

## Lever

One source lever was tested and removed after rejection:

- Add an `hset` direct `CommandHistogram` bucket to `CommandHistogramTracker`.
- Route canonical `"hset"` accounting through that bucket in `record_canonical_with_kind`, `get`, `all`, and `reset`.

No production source hunk is kept in this commit.

## Behavior Isomorphism

- HSET semantics are unchanged: the trial touched only commandstats storage, not command routing, hash mutation, reply encoding, key lookup, expiry, persistence, replication, RNG, floating-point, ordering, or tie-breaking.
- Histogram semantics are unchanged: canonical command name remains `"hset"`, command kind remains `Write`, count/total/min/max/p50/p95/p99 buckets are the same `CommandHistogram` implementation, and `COMMANDSTATS`/`INFO` enumeration keeps existing sorted name order.
- Focused fr-store histogram test passed while the hunk was applied.

Golden raw RESP transcript:

- Input SHA256: `feb22bd30fe7bd48d50ac54544cf3557cb64bacbd44bb7c7be86703d62dd0a97`
- Baseline output SHA256: `1ee447fae351f3e7224f7ab64368d3bdb7d3f7e15455ec3df5100a256fd3a7ac`
- Candidate output SHA256: `1ee447fae351f3e7224f7ab64368d3bdb7d3f7e15455ec3df5100a256fd3a7ac`
- Output bytes: `29`

## Benchmarks

Baseline build:

- `CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass171-target rch exec -- cargo build --release -p fr-server -p fr-bench`

Candidate build:

- `CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass171-candidate-release rch exec -- cargo build --release -p fr-server -p fr-bench`

Initial HSET P16/C50/n500k baseline:

- Hyperfine mean: `1.3508402325542856 s`
- fr-bench last-run throughput: `388460.7733491992 ops/sec`

Paired HSET P16/C50/n1M:

- Baseline: `2.709 s +/- 0.132 s`
- Candidate: `2.611 s +/- 0.193 s`
- Ratio: candidate `1.04x +/- 0.09`, too noisy for a keep.

Reversed HSET P16/C50/n2M:

- Candidate first: `5.028 s +/- 0.211 s`
- Baseline second: `5.052 s +/- 0.221 s`
- Ratio: candidate `1.00x +/- 0.06`, tied.

## Decision

Rejected. Impact is not stable enough to clear Score >= 2.0, so the source hunk was removed before closeout.

Next route: stop commandstats direct-bucket microlevers for HSET and attack a profile-backed batch-shape, parser arena, or output/event-loop primitive.
