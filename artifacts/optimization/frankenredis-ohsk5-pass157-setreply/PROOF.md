# frankenredis-ohsk5 pass157: rejected direct borrowed SET OK encoding

Date: 2026-06-13
Agent: CoralOx

## Profile-backed target

The active ohsk5 route has shifted away from exact packet parser micro-levers
and toward parser/output allocation. Local `perf` sampling was unavailable in
this worktree (`perf_event_paranoid=4`, zero-byte perf data), so this pass used
the existing ohsk5 profile and bead evidence for the borrowed SET/output hot
path. A direct borrowed INCR packet parser was tried first and rejected before
shipping because the candidate was slower than its same-shape baseline.

Candidate lever: successful borrowed `SET key value` writes the static RESP
simple string reply directly into the connection write buffer as `+OK\r\n`.
The state mutation remained in the same runtime core used by the existing
`RespFrame::SimpleString("OK")` path while the candidate was applied.

## Implementation

One candidate lever:

- Add `Runtime::execute_plain_set_borrowed_into(..., out: &mut Vec<u8>)`.
- Factor the existing borrowed SET side effects into
  `execute_plain_set_borrowed_core`.
- In the strict borrowed SET server path, route successful packets to
  `FastEncodedReply` after appending `+OK\r\n` directly.

This would remove one hot-path `String`/`RespFrame` reply allocation and the
later reply encoder dispatch for the borrowed SET fast path.

## Baseline and benchmark

Release builds were crate-scoped through `rch`.

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-baseline-target cargo build --release -p fr-server
worker: vmi1293453
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-setreply-candidate-target cargo build --release -p fr-server
worker: vmi1227854
```

Focused SET P16/c50/n500k persistent-server hyperfine:

```text
baseline:  585.5 ms +/- 42.9 ms
candidate: 565.6 ms +/- 32.8 ms
delta:     1.04x faster
```

Focused throughput samples:

```text
baseline:  874125.81 requests/s
candidate: 938086.31 requests/s
delta:     1.07x faster
```

Paired same-window SET P16/c50/n1M hyperfine:

```text
baseline:  1.258 s +/- 0.036 s
candidate: 1.216 s +/- 0.050 s
delta:     1.03x +/- 0.05 faster
```

Longer paired same-window SET P16/c50/n2M hyperfine:

```text
baseline:  2.398 s +/- 0.125 s
candidate: 2.253 s +/- 0.116 s
delta:     1.06x +/- 0.08 faster
```

Original pre-rebase score:

```text
Impact 2.5 * Confidence 0.85 / Effort 1.0 = 2.13
Initial decision before rebase: keep
```

Rebased same-parent confirmation after origin/main advanced to `f21e3e4fe`:

```text
baseline:  2.262 s +/- 0.045 s
candidate: 2.239 s +/- 0.073 s
delta:     1.01x +/- 0.04 faster
```

Final score after rebased confirmation:

```text
Impact 0.5 * Confidence 0.8 / Effort 1.0 = 0.4
Decision: reject
```

## Golden output

Golden transcript covered normal SET/GET plus `CLIENT REPLY SKIP` suppression:

```text
SET k v
GET k
CLIENT REPLY SKIP
SET skip x
GET skip
QUIT
```

Baseline and candidate TCP transcripts were byte-identical.

```text
sha256 baseline:  af20b8b573a3639fae434d5a10101f42f725b733952dfa98a81cc5a646792479
sha256 candidate: af20b8b573a3639fae434d5a10101f42f725b733952dfa98a81cc5a646792479
```

Escaped transcript:

```text
+OK\r\n
$1\r\n
v\r\n
$1\r\n
x\r\n
+OK\r\n
```

## Isomorphism proof

- Ordering preserved: yes. The reply bytes are appended to the same
  `conn.write_buf` at the same successful borrowed SET point, before the
  existing pubsub drain and output-limit handling.
- Tie-breaking unchanged: not applicable. SET has no tie-breaking path.
- Floating-point unchanged: not applicable.
- RNG unchanged: not applicable.
- Side effects unchanged: yes. `execute_plain_set_borrowed` and the direct
  encoder both call `execute_plain_set_borrowed_core`, so key mutation,
  expiry/dirty propagation, command accounting, slowlog, latency, AOF/repl
  gates, and fallback behavior remain shared.
- Reply suppression unchanged: yes. The direct encoder checks the same
  `suppress_current_network_reply()` state; the golden transcript proves
  `CLIENT REPLY SKIP` suppresses the SET reply and leaves later replies intact.
- RESP bytes unchanged: yes. The canonical simple string OK reply is exactly
  `+OK\r\n` in RESP2/RESP3.

## Gates

Passed:

```text
cargo fmt -p fr-runtime -p fr-server -- --check
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-setreply-test-target cargo test -p fr-runtime plain_set_borrowed_into_encodes_ok_and_honors_reply_suppression -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-setreply-candidate-target cargo build --release -p fr-server
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-setreply-clippy-target2 cargo clippy -p fr-server -p fr-runtime --all-targets -- -D warnings
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-pass157-setreply-check2-target cargo check -p fr-server -p fr-runtime --all-targets
```

`ubs crates/fr-runtime/src/lib.rs crates/fr-server/src/main.rs` returned
nonzero on broad pre-existing file-wide findings, while its embedded fmt,
check, test-build, and clippy stages were clean.

## Final decision

Reject under Score>=2.0 after the rebased same-parent confirmation tied. The
production source hunk was removed before push. Evidence was retained here and
the child bead `frankenredis-ohsk5.40` was closed as rejected.
