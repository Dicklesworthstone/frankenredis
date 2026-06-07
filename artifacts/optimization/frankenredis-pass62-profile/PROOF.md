# frankenredis-batch-reply-output-6w3tf Proof

Status: rejected.

## Target

Pass 62 reprofiled pushed `main` at `8f1912e32` after Pass 61 was closed.

- Baseline GET P16/300k: `391.9 ms +/- 7.2 ms`
- GET P16/1M profile run: `748527.39 ops/sec`, p50 `315us`, p95 `489us`,
  p99 `759us`, p999 `1378us`
- Server profile: 833 samples, 0 lost

Top flat samples:

- `__memmove_avx_unaligned_erms`: 8.80%
- `<fr_store::Value>::string_owned`: 7.39%
- `<fr_runtime::Runtime>::refresh_store_runtime_info_context`: 2.97%
- `fr_protocol::parse_command_args_borrowed_into`: 2.62%

The original route was batch-level reply/output movement. Reading the code
showed a broader data-plane movement lever: the client read buffer physically
drained parsed bytes after each buffered batch. That can call `memmove` when a
partial frame remains. The tested primitive was a sliding read-buffer cursor.

## Lever Tested

One lever was tested:

- Add `ClientConnection::read_start`.
- Parse from `read_start + consumed_total`.
- Use unread byte counts for query-buffer accounting, qbuf metrics, pause,
  blocked, and deferred-client checks.
- Advance the cursor instead of draining parsed bytes every batch.
- Compact only when the consumed prefix reaches 64 KiB.

The source hunk and the associated test assertion update were removed after
benchmarking because the lever failed the Score >= 2.0 keep threshold.

## Behavior Proof

Golden TCP transcript covered:

- `SET k v`
- `GET k`
- missing `GET`
- `PING`
- `QUIT`
- reply ordering through one pipelined transcript

SHA-256 matched exactly:

```text
6e249903a34f5e2ae279afa347011a38a7f049c3c3a8f9f2de75e45c3c7e29af  golden-baseline.resp
6e249903a34f5e2ae279afa347011a38a7f049c3c3a8f9f2de75e45c3c7e29af  golden-candidate.resp
```

Both outputs were 29 bytes, and `cmp` reported no byte differences.

Isomorphism notes:

- Ordering/tie-breaking: replies were byte-identical and in the same order; no
  ranked/set/list ordering logic changed.
- Floating point: N/A.
- RNG: N/A.
- RESP dialect: RESP2 transcript unchanged.
- Query-buffer contract: the candidate counted unread bytes for query-buffer
  limit and qbuf metrics to preserve the observable post-drain semantics.

Validation while the candidate was applied:

- `cargo fmt -p fr-server --check`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass62-check-target cargo check -p fr-server --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass62-test-target cargo test -p fr-server -- --nocapture`
  - Result: all unit tests and most integration tests passed; the unrelated
    `tcp_aof_restart_preserves_all_data` integration test failed with
    `AOF file was not created` and reproduced by itself on another worker.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass62-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass62-profile-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass62-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Baseline-only calibration:

- GET P16/300k: `391.9 ms +/- 7.2 ms`

Paired GET P16/300k:

- Baseline: `406.9 ms +/- 16.2 ms`
- Candidate: `409.5 ms +/- 10.7 ms`
- Baseline: `1.01x +/- 0.05x` faster than candidate

Reversed GET P16/1M confirmation:

- Candidate: `1.306 s +/- 0.041 s`
- Baseline: `1.284 s +/- 0.032 s`
- Baseline: `1.02x +/- 0.04x` faster than candidate

## Score

Score: `0 = Impact 0 x Confidence 1 / Effort 3`.

The sliding read-buffer cursor preserved behavior in the golden transcript but
was neutral/slower in both paired and longer reversed GET evidence. Production
source was restored to baseline; only this proof bundle and bead bookkeeping
are retained.

## Next Route

Do not retry read-buffer drain/cursor variants against this profile. The
profile still says data movement matters, but this specific movement was not
the limiting end-to-end cost. Reprofile before the next bead and attack a
different structural primitive: batch-level command dispatch/output fusion,
writev-style persistent reply fragments, or a key/value layout that removes
`Value::string_owned` without adding per-command server branching.
