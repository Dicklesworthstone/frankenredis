# frankenredis-pe47p proof

## Target

- Bead: `frankenredis-pe47p`
- Lever: borrowed SETEX/PSETEX dispatch packet for exact RESP multibulk commands.
- Baseline commit: `2015d595e`
- Baseline binary: `/data/projects/frankenredis/target-cod-pe47p-baseline-rch/release-perf/frankenredis`
- Candidate binary: `/data/projects/frankenredis/target-cod-pe47p-candidate-rch/release-perf/frankenredis`

## Profile-backed hotspot

Baseline SETEX/PSETEX P16/1M profile still showed owned argv and generic command
machinery in the server hot path:

- `Runtime::execute_frame_internal`
- `frankenredis::process_buffered_frames`
- `Runtime::dispatch_with_client_context`
- `fr_command::command_key_indexes`
- `fr_protocol::parse_command_args_borrowed_into`
- `fr_command::classify_command`
- `fr_command::command_table_index`
- `copy_borrowed_argv_into_scratch`
- allocator/drop rows around argv materialization

## Isomorphism proof

The candidate only admitted exact four-argument `SETEX key seconds value` and
`PSETEX key milliseconds value` packets under the existing conservative default
write gates. Non-default states, ACL/auth, client tracking, AOF/replication,
pub/sub, transaction, notifications, monitor, maxmemory, TTL parse failures, and
relative-time overflow all deferred to canonical dispatch.

For admitted commands, the candidate used the same `Store::set(key, value,
Some(px), now_ms)` operation, the same simple-string OK reply, the same active
expire/lazy-expire propagation order, the same write count, and the same
slowlog/latency/threat argv bytes. Command order is unchanged because the server
processes one complete frame in the same buffer order. No tie-breaking,
floating-point, or RNG behavior is touched.

Focused runtime tests passed:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-pe47p-check-rch cargo test -p fr-runtime plain_expiring_set_borrowed -- --nocapture
```

Crate-scoped check passed:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-pe47p-check-rch cargo check -p fr-runtime -p fr-server --all-targets
```

RCH fell open locally because the detached scratch worktree was outside the
canonical `/data/projects` root; this matched the baseline build path.
`cargo fmt -p fr-runtime -p fr-server --check` remains blocked by pre-existing
unrelated formatting drift in legacy runtime/server test blocks, so no
formatting-only source hunk is retained.

## Golden output

Artifact: `artifacts/optimization/frankenredis-pe47p/golden-compare.json`

```json
{
  "baseline_bytes": 992,
  "baseline_sha256": "dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7",
  "candidate_bytes": 992,
  "candidate_sha256": "dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7",
  "equal": true
}
```

## Benchmarks

Standalone baseline:

- `4.558392316s +/- 0.057746816s`

Paired hyperfine:

- Baseline: `4.778845982s +/- 0.128489766s`
- Candidate: `4.738717570s +/- 0.184968090s`
- Summary: candidate `1.01x +/- 0.05` faster

Reversed hyperfine:

- Candidate: `4.661270144s +/- 0.135306781s`
- Baseline: `4.656130580s +/- 0.139617828s`
- Summary: baseline `1.00x +/- 0.04` faster

Artifacts:

- `artifacts/optimization/frankenredis-pe47p/paired-setex-p16-1m-hyperfine.json`
- `artifacts/optimization/frankenredis-pe47p/reversed-setex-p16-1m-hyperfine.json`

## Decision

Reject under Score>=2.0. Score: `0.0`.

No production source hunk is retained. Next route: attack a class-level batched
zero-copy parser/output arena or parser-to-dispatch command metadata packet that
removes owned argv materialization and repeated metadata work across commands.
