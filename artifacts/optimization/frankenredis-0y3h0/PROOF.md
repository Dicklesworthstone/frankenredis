# frankenredis-0y3h0 proof

## Target

- Bead: `frankenredis-0y3h0`
- Profile-backed hotspot: after `frankenredis-825jg`, SETEX/PSETEX P16/1M moved active-expire miss scans out of the lead slot and exposed TTL write-index costs:
  `BTreeMap<Vec<u8>, SetValZST>::insert`, `__memcmp_avx2_movbe`, `Store::internal_entries_insert`, and `Store::update_expiry_deadline`.
- Lever tested: lazy materialization of the volatile-key sorted expiry index so long-TTL writes avoid per-write `BTreeMap<Vec<u8>, _>` insertion and key-comparison cost until a deadline is actually due.

The isolated candidate was tested against the already-sidecar baseline. During the run, the current branch advanced to `740e8b3fd` / later `8ba384797`; that source already includes this lazy-index family as part of the larger kept `frankenredis-825jg` sidecar route. No extra source hunk is retained for this rejected incremental bead.

## Behavior Isomorphism

- Golden transcript artifact: `golden-compare.json`
- Baseline SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Candidate SHA-256: `dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`
- Bytes: 992 baseline, 992 candidate
- Equality: true

The SETEX/PSETEX/PTTL/PERSIST transcript bytes are identical. Redis-visible ordering, DB selection, lazy-expiry semantics, active-expire due-key fallback, invalid-TTL fallback, tie-breaking, floating-point behavior, and RNG behavior are unchanged. The lever only delays maintenance of an internal expiry index; when a deadline is due, the existing deterministic sorted-key path is rebuilt and consumed.

## Validation

While the candidate was applied:

- `cargo fmt -p fr-store --check`
- RCH `cargo check -p fr-store --all-targets`
- RCH `cargo test -p fr-store active_expire -- --nocapture`
- RCH `cargo test -p fr-store volatile_key -- --nocapture`
- RCH `cargo clippy -p fr-store --all-targets -- -D warnings`

All focused validations passed.

## Benchmarks

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-0y3h0-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1153651
```

Candidate build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-0y3h0-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
worker: vmi1153651
```

Standalone baseline, P16/1M:

- Baseline: `4.536s +/- 0.020s`
- Artifact: `0y3h0-baseline-setex-p16-1m-hyperfine.json`

Paired P16/1M:

- Baseline: `4.4939486706s +/- 0.0455341426s`
- Candidate: `4.5090424037s +/- 0.0394800889s`
- Ratio: baseline `1.00x +/- 0.01` faster
- Artifact: `0y3h0-setex-p16-1m-paired-hyperfine.json`

Reversed P16/1M:

- Candidate: `4.5241005177s +/- 0.1071446601s`
- Baseline: `4.4645548397s +/- 0.0395763023s`
- Ratio: baseline `1.01x +/- 0.03` faster
- Artifact: `0y3h0-setex-p16-1m-reversed-hyperfine.json`

## Decision

Reject/supersede under the Score>=2.0 keep gate.

- Impact: 0
- Confidence: 4
- Effort: 2
- Score: 0.0

The isolated lazy sorted-index increment did not produce a real same-worker win over the sidecar baseline. Do not retry this exact lazy volatile-key materialization family. The next optimization pass must re-profile current HEAD after the kept `frankenredis-825jg`/`740e8b3fd` route and attack a different shifted primitive, likely command/runtime/output cost or a deeper store key-layout primitive only if fresh profile evidence puts it on top.
