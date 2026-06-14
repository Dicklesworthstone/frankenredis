# Pass 194 Report - inline PING fast path

Bead: `frankenredis-ohsk5.61`
Base: `c437e9695`

## Target Selection

Fresh current-main release-perf binaries were built with RCH on a clean detached
worktree:

- `frankenredis`: `04b0faa9f8af5b6ecbc67f08ee357f6ddd185761dc788c5caf35a027d6bd2795`
- `fr-bench`: `a0957bb86156273e76aded1e3f950fd010d8a4e25caf1c3343e3f3c99a072069`

The standard Redis adjacency sweep selected `PING_INLINE` as the only confirmed
large front-end residual:

| Command | Redis req/s | FrankenRedis req/s | Redis/FR | Verdict |
| --- | ---: | ---: | ---: | --- |
| PING_INLINE | 1,090,909.12 | 804,289.56 | 1.356x | FR slower |
| PING_MBULK | 1,079,136.75 | 1,123,595.50 | 0.960x | FR faster |
| GET | 813,008.19 | 931,677.00 | 0.873x | FR faster |

## Lever

One source lever was kept: recognize exact uppercase no-argument inline
`PING\r\n` and `PING\n` before the generic inline parser, then reuse the
existing borrowed encoded PING reply path. Lowercase `ping`, PING with an
argument, incomplete frames, and RESP multibulk PING still use the existing
generic paths.

Candidate2 release-perf binaries:

- `frankenredis`: `1bd716df687d353fba5fa97cb9efdd303751d914174122398b8e58a585a36f66`
- `fr-bench`: `d1b80e863dd3814b128568bbad412d2406af746fde9694d768f658a49eb1621b`

## Behavior Proof

Raw TCP replay against the vendored Redis oracle matched byte-for-byte:

| Case | SHA256 |
| --- | --- |
| `PING\r\n` | `64c2f2c744321d052076467905a0561f91e9a6de4e84441addbcc549cd71095c` |
| `PING\n` | `64c2f2c744321d052076467905a0561f91e9a6de4e84441addbcc549cd71095c` |
| `ping\n` | `64c2f2c744321d052076467905a0561f91e9a6de4e84441addbcc549cd71095c` |
| `PING hi\r\n` | `a675d657d7468ec34cc2dc6e47048fc1c124dd28cbfdea7a7d144bd90fffdeb8` |
| mixed inline sequence | `cf657700633c3a27d82f79db480c8351b5aea76ad5cc7669d03cdaa8d7b60800` |

Ordering and tie-breaking: unchanged. The fast path only consumes one complete
no-argument inline PING frame and appends the same encoded response that the
borrowed multibulk PING path already emits. Floating point and RNG: not
applicable.

## Benchmark Delta

Paired P16/C50/n800k, same worker, baseline first:

| Command | Baseline req/s | Candidate req/s | Candidate/Baseline |
| --- | ---: | ---: | ---: |
| PING_INLINE | 850,159.44 | 1,037,613.44 | 1.220x |
| PING_MBULK | 1,112,656.50 | 1,049,868.75 | 0.944x |
| GET | 997,506.25 | 1,037,613.44 | 1.040x |

Reversed order:

| Command | Baseline req/s | Candidate req/s | Candidate/Baseline |
| --- | ---: | ---: | ---: |
| PING_INLINE | 840,336.12 | 1,010,101.00 | 1.202x |
| PING_MBULK | 1,052,631.62 | 1,089,918.25 | 1.035x |
| GET | 1,049,868.75 | 1,065,246.38 | 1.015x |

Decision: keep. PING_INLINE improved by 20.2-22.0 percent. Adjacent PING_MBULK
and GET did not show a stable regression after reversing order.

Score: `1.21 impact * 0.85 confidence / 0.50 effort = 2.06`.

## Gates

- `cargo fmt -p fr-server -- --check`: pass.
- `rch exec -- cargo check -j 1 -p fr-server --all-targets`: pass.
- `rch exec -- cargo test -j 1 -p fr-server --bin frankenredis tests::inline_plain_ping_noarg_fast_path_recognizes_exact_wire_shape_only -- --exact`: pass.
- `ubs crates/fr-server/src/main.rs`: nonzero from pre-existing whole-file
  findings; no new changed-line finding in the inline PING helper or fast path.
- `rch exec -- cargo clippy -j 1 -p fr-server --all-targets -- -D warnings`:
  pass.
