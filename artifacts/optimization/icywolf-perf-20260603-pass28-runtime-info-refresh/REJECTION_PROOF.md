# Pass 28 Rejection Proof: Runtime INFO Demand Refresh

Bead: `frankenredis-runtime-info-refresh-v6ltr`

## Profile-Backed Target

Clean baseline commit: `3bc563e52`

Pass-28 profile evidence before source edits:

- SET pipeline=16 direct baseline: `276,738.05 ops/sec`, p50 `2627 us`, p95 `4159 us`, p99 `5083 us`.
- Hyperfine baseline: `1.7025s +/- 0.0255s` over 8 runs.
- Server strace: `sendto` 31,250 calls / `48.63%`, `recvfrom` 62,550 calls / `35.23%`, `epoll_wait` 1,968 calls / `14.31%`.
- GDB sample hit `Runtime::refresh_store_runtime_info_context` at `crates/fr-runtime/src/lib.rs:4860` during ordinary SET dispatch.

## Candidate Lever

Move `refresh_store_runtime_info_context()` out of the ordinary command hot path and run it only for `INFO`, preserving the original refresh-before-session-snapshot ordering for INFO commands.

The candidate touched only `crates/fr-runtime/src/lib.rs` in a detached clean worktree:

`/data/projects/frankenredis_icywolf_pass28_runtimeinfo_candidate_20260603T1911Z`

## Measurements

Direct SET pipeline=16, 50 clients, 500k requests:

- Baseline: `289,196.01 ops/sec`, p50 `2553 us`, p95 `3683 us`, p99 `4499 us`.
- Candidate: `286,910.17 ops/sec`, p50 `2581 us`, p95 `3735 us`, p99 `4475 us`.

Paired hyperfine, 500k requests, 8 runs:

- Baseline: `1.746s +/- 0.068s`.
- Candidate: `1.673s +/- 0.042s`.
- Candidate appeared `1.04x +/- 0.05x` faster, but the direct run conflicted.

Long paired hyperfine tie-breaker, 2M requests, 5 runs:

- Baseline: `7.693s +/- 0.190s`.
- Candidate: `7.835s +/- 0.131s`.
- Baseline ran `1.02x +/- 0.03x` faster.

## Verdict

Reject. The longer run reversed the short-run signal, so the candidate does not clear the Score `>= 2.0` keep gate.

## Isomorphism

- Ordering preserved: final tree retains no source change.
- Tie-breaking unchanged: final tree retains no source change.
- Floating-point: N/A.
- RNG: N/A.
- Golden output: performance gate failed before a behavior-changing commit; final tree is byte-identical to the pre-candidate runtime source (`git diff -- crates/fr-runtime/src/lib.rs` is empty after source removal).

## Next Primitive

Attack the response/socket path with a deeper response segment queue or submission-queue-style batching primitive for the `sendto` / `recvfrom` / `epoll_wait` cluster. Do not continue tuning runtime-info refresh.
