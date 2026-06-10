# Pass115 Rejection: Server-Only Profile And Writer Fanout

Bead: `frankenredis-h0714`
Commit base: `bc1fd5a8e`

## Target

Pass114's combined profile included both server writer and `fr-bench` sender
samples. This pass first isolated server CPU by attaching `perf record` to the
server PID while `fr-bench` drove SET/P16/C50/3M.

Baseline SET/P16/C50/1M:

- `746.2 ms +/- 45.7 ms`.

Server-only profile:

- SET/P16/C50/3M: `1130956.8810836659 ops/sec`.
- p50 `636us`, p95 `1129us`, p99 `1892us`.
- 6353 perf samples, 0 lost.
- `fr-writer-1` `__send`: `20.69%` self.
- `fr-writer-0` `__send`: `19.34%` self.
- `fr_store::canonical_string_value_from_slice`: `10.14%` self.

## Lever Tested

Candidate: increase `WRITER_POOL_WORKERS` from `2` to `4`.

Reasoning: the server-only top rows were two writer threads spending most CPU in
`__send`. More worker fanout preserves the existing one-in-flight-job-per-client
rule while testing whether the workload is limited by writer-thread parallelism.

The candidate source hunk was removed after benchmarking.

## Isomorphism Proof

Baseline and candidate raw TCP RESP transcripts matched exactly.

- Golden sha256: `27fde3960948e19fe73956e617c34b409981279579471ef94feb6af1ebe6e30e`.
- Covered mixed-case direct SET, GET, SET NX fallback, pipelined SET replies,
  CLIENT REPLY OFF suppression, CLIENT REPLY ON, PING, and GET after suppressed
  write.

The no-diff rejection preserves reply bytes, per-client ordering, cross-client
semantic independence, expiry, key policy, floating-point behavior, RNG
behavior, object encoding, and digest semantics.

## Performance

Paired SET/P16/C50/1M:

- Baseline: `820.3 ms +/- 58.1 ms`.
- Candidate: `809.1 ms +/- 36.1 ms`.
- Candidate was only `1.01x +/- 0.08` faster.

Reversed SET/P16/C50/1M:

- Candidate: `820.7 ms +/- 36.5 ms`.
- Baseline: `784.9 ms +/- 44.5 ms`.
- Baseline was `1.05x +/- 0.08` faster.

## Decision

Reject. Writer fanout does not clear the `Score>=2.0` gate and is contradicted
by reversed order. The `__send` rows are real server cost, but simply adding
writer workers is not the primitive.

Score: `0.5 < 2.0`.

## Next Route

Do not repeat tiny sync flush or writer worker-count tuning. The next
profile-backed route should attack the next server user-space row,
`fr_store::canonical_string_value_from_slice` at `10.14%` self, but it must avoid
the previously rejected integer-parser micro-family. Use an alien-artifact
algorithmic value-normalization primitive or route deeper into representation
metadata instead of another branch-only parse tweak.
