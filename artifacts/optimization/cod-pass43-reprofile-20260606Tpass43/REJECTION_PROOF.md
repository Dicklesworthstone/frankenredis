# Pass 43 rejection: active-expire no-work bypass

Date: 2026-06-06
Bead: frankenredis-ohsk5
Agent: cod

## Profile-backed target

Current-main build used RCH, crate-scoped:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass43-profile-target cargo build --profile release-perf -p fr-server -p fr-bench
```

The scratch worktree needed `legacy_redis_code` resolved for the build script; the rerun used the same source tree and completed on worker `ts1`.

Baseline hyperfine, SET P16, 300k requests, 50 clients, keyspace 10000, datasize 3:

- Mean: 1.0860445034200001s
- Stddev: 0.18454004417329178s
- Direct run: 338274.3835964464 ops/sec, p50 2053us, p95 3563us, p99 4503us, total_time_ms 886

Fresh profile run, SET P16, 1M requests:

- Direct run: 306421.90857592685 ops/sec, p50 2285us, p95 4115us, p99 5683us, p999 14039us, total_time_ms 3263
- perf event: cycles:P
- Samples: 1529
- Lost samples: 0

Top symbols from `profile-set-p16-1m-report.txt`:

- 7.35%: SipHash `Hasher::write`
- 5.80%: `fr_runtime::Runtime::execute_dispatch`
- 3.64%: `fr_runtime::Runtime::execute_frame_internal`
- 2.82%: vdso time path
- 2.79%: `fr_runtime::Runtime::dispatch_with_client_context`
- 1.78%: `fr_store::Store::internal_entries_insert`
- 1.63%: `fr_protocol::parse_command_args_borrowed_into`
- 1.38%: `RandomState::hash_one::<&[u8]>`
- 1.29%: `fr_server::process_buffered_frames`
- 1.28%: `fr_command::AclUser::acl_permission_error_for_argv`
- 1.20%: `fr_command::set`

This target is profile-backed and still dominated by SET dispatch/store/hash/parser/write processing. No architectural ceiling is inferred.

## Lever tested

One retained benchmark lever was tested: an early return in `ServerState::run_active_expire_cycle` when `plan.sample_limit == 0`, avoiding `Instant::now()` and `Store::run_active_expire_cycle` on no-work cycles.

An earlier borrowed-args scratch reuse attempt failed `cargo check -p fr-server --all-targets` with borrow checker conflicts between references into `conn.read_buf` and mutable `conn` access. That hunk was reverted before benchmarking and is not retained.

## Validation

RCH checks and tests for the active-expire lever:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass43-expire-check-target cargo check -p fr-runtime --all-targets
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass43-expire-test-target cargo test -p fr-runtime active_expire -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass43-expire-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Focused tests passed: 3 unit tests and 2 admin tests.

Isomorphism proof:

- Ordering: no command execution, propagation, or reply ordering code changed.
- Tie-breaking: no data-structure iteration or ordering rules changed.
- Floating point: no floating-point code changed.
- RNG: no RNG path changed.
- Expiry semantics: for `sample_limit > 0`, the old path is unchanged. For `sample_limit == 0`, the candidate returned the same zero stats shape, advanced to `plan.next_db_index`, cleared the active key cursor, set stale percentage to zero, and performed no key eviction or command propagation.

Golden-output proof used a raw RESP transcript covering `PING`, persistent `SET`/`GET`, overwrite `SET`/`GET`, TTL-bearing `SET t x PX 100000`, `GET`, `DEL`, and `QUIT`.

- Baseline sha256: 85d55366e8c41933f6a48b9271d2daed6281e11cc445295b598a289b5d0f9144
- Candidate sha256: 85d55366e8c41933f6a48b9271d2daed6281e11cc445295b598a289b5d0f9144
- Transcript size: 52 bytes each

## Benchmark result

Paired hyperfine, SET P16, 300k requests, 50 clients, same scratch profile/candidate target dirs:

- Baseline: 1.34781990068s +/- 0.1149557286526074s
- Candidate: 1.39126265688s +/- 0.1298975988844659s
- Hyperfine summary: baseline ran 1.03x +/- 0.13 faster than candidate
- Direct baseline: 275361.8164316385 ops/sec, p99 5995us, total_time_ms 1089
- Direct candidate: 249299.65598018622 ops/sec, p99 9407us, total_time_ms 1203

Score: less than 2.0. The lever is rejected and no production source hunk is retained.

## Next primitive

Do not repeat the command metadata packet, RESP memchr line scan, runtime tracking hash swap, borrowed SET post-write guard, borrowed-args ref reuse, or active-expire no-work guard.

Next pass should attack a structurally different primitive:

- Parser/dispatch fusion with span-owned scratch or a direct borrowed dispatch API designed around Rust's borrow rules, targeting less parser/argv/hash churn in the SET P16 path.
- Or output batching/write-path allocation and copy removal, targeting the `process_buffered_frames` and reply emission portion of the same profile.
