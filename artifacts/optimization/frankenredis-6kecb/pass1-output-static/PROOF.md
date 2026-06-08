# frankenredis-6kecb pass 1 rejection proof

## Target

- Repeated-skill pass: 1/5, `Parser-to-dispatch packet`
- Bead: `frankenredis-6kecb`
- Source basis: `0b210f0d84b561d74d8cc0a4c29573c9daff452f`
- Candidate lever: static `+OK\r\n` encoding in the shared client reply encoder.

## Profile basis

The same-day `frankenredis-6kecb` SETEX/PSETEX P16/1M profile showed the
current post-store-shift runtime pipeline costs:

- `RandomState::hash_one::<&[u8]>`: 1.69% self
- `Runtime::refresh_store_runtime_info_context`: 0.81% self
- `Runtime::execute_frame_internal`: 0.77% self
- `fr_command::rewrite_relative_expire_for_propagation`: 0.66% self
- `fr_protocol::parse_command_args_borrowed_into`: 0.61% self
- `Runtime::dispatch_with_client_context`: 0.60% self
- `frankenredis::process_buffered_frames`: 0.53% self
- `fr_command::command_table_index`: 0.53% self
- `fr_command::command_key_indexes`: 0.53% self

This pass tested a broad output-path micro-lever for the common simple status
reply emitted by SETEX/PSETEX and many other write commands. It is not a
SETEX/PSETEX-specific borrowed branch.

## Baseline

RCH baseline build from clean detached source:

```text
git worktree add --detach /data/projects/.scratch/frankenredis-6kecb-baseline-0b210f0d8-20260608T1514 0b210f0d8
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6kecb-pass1-baseline-target2 cargo build --profile release-perf -p fr-server
worker: vmi1227854
```

Baseline binary SHA-256:

```text
34551c026acf3637b52393db7f7b3d22119c627e413ea58433054d943f73a290
```

Standalone current-HEAD baseline before candidate:

- `4.431621675840001s +/- 0.03502924781499856s`

## Candidate

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-6kecb-pass1-okstatic-candidate-target cargo build --profile release-perf -p fr-server
worker: vmi1227854
```

Candidate binary SHA-256:

```text
7bcc5316055bbbcb478db2e5770d9451960bf7555d65e81ee88ac8718fca4d34
```

The source hunk was removed after benchmark rejection. No production code is
retained from this lever.

## Behavior proof

Golden comparator:

```text
python3 artifacts/optimization/frankenredis-svgvb/setex_golden_compare.py 26951 26952 artifacts/optimization/frankenredis-6kecb/pass1-output-static/golden-compare.json
```

Golden result:

```json
{
  "baseline_bytes": 992,
  "baseline_sha256": "dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7",
  "candidate_bytes": 992,
  "candidate_sha256": "dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7",
  "equal": true
}
```

Isomorphism:

- Ordering: unchanged. The candidate only changed encoding of an already-built
  `RespFrame::SimpleString("OK")`; command execution and buffering order stayed
  identical.
- Tie-breaking: N/A.
- Floating-point: N/A.
- RNG: unchanged.
- Error and non-OK replies: unchanged; only the exact `OK` simple string used
  static bytes while the generic encoder still handled every other frame.
- RESP2/RESP3: unchanged for `OK`; RESP3 simple strings use the same wire bytes.

## Benchmarks

Paired SETEX/PSETEX P16/1M, baseline first:

- Baseline: `4.452678600314285s +/- 0.05503998601236914s`
- Candidate: `4.4174800416s +/- 0.07855304506602873s`
- Hyperfine summary: candidate `1.01x +/- 0.02` faster

The reversed run was attempted, but both `/tmp` build artifacts had been
reclaimed before the first reversed warmup could start. Because the paired
comparison was only a 1.01x micro-win and below the campaign keep gate, the
lever was rejected rather than rebuilt for a same-family confirmation run.

## Decision

Reject under the Score>=2.0 gate.

- Impact: 1
- Confidence: 4
- Effort: 1
- Score: 0.0 because measured impact is below the real-win threshold.

Next pass must not repeat static simple-status output micro-tuning. Route to a
deeper parser-to-dispatch or batched packet primitive that removes repeated argv
materialization or command metadata work as a class.
