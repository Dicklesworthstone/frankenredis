# 2026-06-20 cod-a pubsub fanout direct encoder

Bead: `frankenredis-ohsk5`
Agent: `BlackThrush` / assignee `cod-a`
Target dir: `/data/projects/.rch-targets/frankenredis-cod-a`

## Lever

The kept hunk removes the intermediate `RespFrame` allocation from hot pubsub
delivery in `fr-server`. `fr-command` now exposes
`encode_pubsub_message_for_protocol_into`, which writes the exact RESP2/RESP3
wire bytes directly into the client output buffer for `message`, `pmessage`,
`smessage`, and client-tracking `invalidate` pushes.

This is a cache/allocation hot-path lever: fewer transient aggregate frames,
fewer nested vectors, and less branchy frame re-walk per delivered subscriber
message.

## Binary hashes

Control build:

- `frankenredis`: `0017a7ffa385769fdbc17da740b0fd897e454c8e1f619792ddc03dd703d7e263`
- `fr-bench`: `773081236a0925c42892af37ba7cad8562bcaf56048ac9c1060570650a122930`
- Redis 7.2.4: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`

Rejected pending-client Vec candidate:

- `frankenredis`: `2cd5bad50fda7576bccb5778a844bd42e90509af60609ad6e819ea8fbf837659`
- `fr-bench`: `723c1b6207eb299444670baae78ffe9d21f896ca018647d9957f88b4432aff64`

Kept direct encoder candidate:

- `frankenredis`: `fa5d801f602241ca6a916d40b76c9c913c711251c47a622dcdefea42d34bf9f3`
- `fr-bench`: `9505c01d3d7208ea96de30348bae30fbd9e7b891d3d9b13990b55f75ea52ba08`

## Head-to-head pubsub evidence

Custom fanout harness reports delivered subscriber-messages per second. The
control binary is the saved pre-hunk release binary; the candidate binary is the
direct-encoder release binary; Redis is vendored Redis 7.2.4.

| artifact | topology | candidate/control | candidate/redis | control/redis | verdict |
|---|---|---:|---:|---:|---|
| `candidate_control_pubsub_fanout_32x4000_v2.txt` | 32 subscribers, 4000 messages, pipe 32, trials 7 | 0.9963 | 0.9575 | 0.9610 | reject pending-client Vec hunk |
| `direct_encoder_pubsub_fanout_32x4000.txt` | 32 subscribers, 4000 messages, pipe 32, trials 7 | 1.0614 | 0.9967 | 0.9390 | keep direct encoder; primary gate |
| `direct_encoder_pubsub_fanout_32x4000_confirm.txt` | 32 subscribers, 4000 messages, pipe 32, trials 5 | 1.0150 | 0.9411 | 0.9272 | confirm modest win; Redis-relative still below |
| `direct_encoder_pubsub_fanout_64x3000_confirm.txt` | 64 subscribers, 3000 messages, pipe 32, trials 5 | 1.0242 | 0.9770 | 0.9539 | confirm modest win; gap narrows |

The first `candidate_control_pubsub_fanout_32x4000.txt` run is a discarded
harness attempt: a byte-by-byte subscriber parser failed delivery-completeness
checks. It is kept as negative harness evidence only. The valid v2 buffered
parser showed the pending-client Vec hunk at `0.9963x` candidate/control, so
that source hunk was reverted.

## Crate bench evidence

The literal requested `cargo bench --release -p fr-bench` was attempted through
`rch` and failed because this Cargo toolchain does not accept `--release` for
`cargo bench` (`unexpected argument '--release'`). `cargo bench` already uses
the optimized bench profile.

The valid per-crate bench command passed after building `fr-server` on the same
remote worker and pinning `FR_SERVER_BIN`:

```
rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a \
  FR_SERVER_BIN=/data/projects/frankenredis-cod-a-pubsub-20260620/.rch-target-hz2-pool-dd746d3bf1c8315d4f2585352f439b4e/release/frankenredis \
  cargo bench -p fr-bench
```

Exit: 0 on remote `hz2`. The crate bench is a broad smoke/context run, not the
pubsub keep gate. Notable midpoint ratios from the Criterion output were:

| bench cell | redis midpoint | frankenredis midpoint | fr/redis |
|---|---:|---:|---:|
| `exists8_all_hit` | 165.60 us | 145.24 us | 1.1402 |
| `exists8_half_hit` | 134.14 us | 128.68 us | 1.0424 |
| `exists8_duplicates` | 158.87 us | 149.64 us | 1.0617 |
| `quicklist2_packed_restore` | 74.076 us | 124.84 us | 0.5934 |
| `SINTERSTORE` | 955.64 us | 537.30 us | 1.7786 |
| `SDIFFSTORE` | 852.72 us | 441.94 us | 1.9295 |
| `SUNIONSTORE` | 8.1998 ms | 6.9051 ms | 1.1875 |

## Gates

- `cargo fmt --check -p fr-command -p fr-server`: passed locally.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo check -p fr-command -p fr-server --all-targets`: passed on `vmi1152480`.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo test -p fr-command direct_pubsub_encoder_matches_frame_encoder_bytes -- --nocapture`: passed on `vmi1152480`.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo clippy -p fr-command -p fr-server --all-targets -- -D warnings`: passed on `vmi1152480`.
- `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a cargo test -p fr-conformance -- --nocapture`: passed on `vmi1167313` with 194 + 8 + 3 + 5 + 4 + 3 + 5 + 9 + 2 + 3 + 2 + 1 + 3 + 99 tests all ok.

## Decision

Keep the direct pubsub encoder. It is not a complete Redis domination claim:
the primary 32-subscriber gate nearly closes the Redis gap (`0.9390x` control to
`0.9967x` candidate), while confirmations remain below Redis (`0.9411x` and
`0.9770x`). The same-control median gain repeated across confirmation shapes,
so this is a measured keep.

Reject and do not ship the pending-client `HashSet` to `Vec` hunk. It measured
`0.9963x` candidate/control and did not move the Redis-relative result.
