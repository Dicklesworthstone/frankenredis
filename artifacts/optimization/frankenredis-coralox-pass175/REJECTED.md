# frankenredis-ohsk5.47 rejected: HSET direct integer reply encoding

## Target

- Bead: `frankenredis-ohsk5.47`
- Current-main routing sweep, P16/C50/n300k:
  - HSET: FrankenRedis `739151.47 ops/sec`, Redis `862417.45 ops/sec`
  - Redis/fr: `1.1668x`
  - p99: FrankenRedis `4291us`, Redis `1721us`
- Host sampling constraint: `perf_event_paranoid=4`, so this pass used the
  current command-level residual plus checked-in HSET profile rows.
- Candidate lever: on the strict canonical `HSET key field value` packet path,
  encode the integer reply directly into the connection output buffer instead
  of constructing/generic-encoding `RespFrame::Integer`.

## Baseline

- Built current baseline with `rch`:
  - `CARGO_TARGET_DIR=/data/tmp/frankenredis-pass175-current-target`
  - command: `cargo build --profile release-perf -p fr-server -p fr-bench`
- Independent HSET P16/C50/n1M hyperfine:
  - baseline: `1.113805793s +/- 0.049536697s`

## Behavior Proof

- Focused parser tests passed with the candidate applied:
  - `cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- Raw TCP golden transcript:
  - input SHA256: `34b9b9f44b9f8c7ffdbed708c15625cc9559c50aeec9d2269ec30698f1675d77`
  - baseline output SHA256: `14b04ac8046c6b9a24bc304e7f147ef84a50fa415a1979e775012cc93607e8b9`
  - candidate output SHA256: `14b04ac8046c6b9a24bc304e7f147ef84a50fa415a1979e775012cc93607e8b9`
- Isomorphism:
  - Ordering/tie-breaking: the packet path still processed one command in input
    order and drained pub/sub after the reply path.
  - Error semantics: wrongtype/error replies kept generic `RespFrame` encoding.
  - CLIENT REPLY: candidate checked the same suppression state before writing.
  - Floating point: no FP path touched.
  - RNG: no RNG/LFU sampling path touched.

## Re-benchmark

- Paired HSET P16/C50/n1M hyperfine:
  - baseline: `1.090722972s +/- 0.022625418s`
  - candidate: `1.145226757s +/- 0.084521325s`
  - summary: baseline `1.05 +/- 0.08x` faster
- Score:
  - `0 impact * 4 confidence / 1 effort = 0`
  - Fails the required `Score>=2.0` keep gate.

## Decision

- Rejected.
- Production source hunk was removed before commit.
- Evidence retained in this directory.

## Next Route

Do not repeat direct integer output for HSET/LPUSH-style integer replies. The
next HSET attempt should move to a deeper primitive: parser arena/region reuse
across a readable batch, or a store-layout/key-comparison primitive with fresh
profile support.
