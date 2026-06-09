# frankenredis-ohsk5.3 rejection

## Target

- Bead: `frankenredis-ohsk5.3`
- Workload: `fr-bench` SET, 50 clients, 1M or 300k requests, pipeline 16,
  keyspace 10000, datasize 3
- Profile evidence: current SET P16 profile showed parser/allocator work still
  present after earlier ohsk5 store and ACL micro-levers were rejected.

## Lever Tested

Fixed-array borrowed RESP argv parsing for the strict multibulk server hot path.
The candidate parsed small argv lists into stack-backed `[&[u8]; N]` storage and
fell back to the existing heap-backed borrowed parser when argc exceeded the
inline capacity.

## Build Gate

- Baseline/current release-perf build: passed with `rch exec -- cargo build
  --profile release-perf -p fr-server -p fr-bench`
- Fixed-array candidate release-perf build: passed with `rch exec -- cargo build
  --profile release-perf -p fr-server -p fr-bench`

## Benchmarks

Short 300k gate:

- Baseline/current: `0.6044477477s +/- 0.0694798882`
- Candidate fixed-array parser: `0.5875140492s +/- 0.0360236156`
- Directional delta: about `1.03x`, too small/noisy to keep alone.

Paired 1M confirmation:

- Baseline/current: `1.6897074351s +/- 0.0673438776`
- Candidate fixed-array parser: `2.2928995451s +/- 0.5250344364`
- Hyperfine summary: baseline was `1.36x +/- 0.32` faster.
- Last-run throughput: baseline `586888.58 ops/sec`, candidate
  `358922.16 ops/sec`.

## Isomorphism

Behavior was not carried to a full golden-output proof because the paired perf
gate rejected the candidate before any keep decision. The parser storage lever
was intended to preserve argv order, null/empty multibulk handling, parse error
wording, fallback behavior, reply bytes, RNG, expiry ordering, blocking semantics,
pubsub drain ordering, and output-limit handling.

## Decision

Reject under the Score>=2.0 rule.

- Impact: `0` after the paired 1M regression
- Confidence: `4`
- Effort: `2`
- Score: `0`

The fixed-array parser source hunk was removed. Evidence files are retained in
this directory for auditability.

## Next Route

Stop parser-storage micro-levers. The next profile-backed ohsk5 child should
attack a different primitive class, preferably command-batch/arena execution or
reply/output batching if a fresh profile keeps per-command materialization or
write-side cost high.
