# Pass99 Rejection Proof: process_buffered_frames output-limit cache

## Target

- Profile-backed workload: current-main SET/GET/mixed P16/1M profiles in
  `artifacts/optimization/orangemouse-pass99-current-profile-20260609/`.
- Hot rows motivating the lever:
  - SET P16/1M: `Runtime::pubsub_sub_count` `1.37%` children / `1.10%` self,
    `Runtime::is_pubsub_client` `1.35%` children, and
    `RandomState::hash_one::<&u64>` `1.35%` around output-limit classification.
  - MIXED P16/1M: `process_buffered_frames` `2.26%` self and
    `pubsub_sub_count` `0.47%` children.
- Alien-graveyard primitive class: batch metadata capsule / event-loop state
  caching. The tested one-lever slice cached the per-client output hard limit
  inside `process_buffered_frames` and refreshed it after generic command paths.

## Baseline

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass99-limit-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH failed open locally because no admissible workers were available, but the
command remained crate-scoped and used an isolated target dir.

One-sided baseline hyperfine, SET P16/300k:

- Baseline: `0.8040008413050002s +/- 0.04149591672424094`

## Candidate

Lever tested in `crates/fr-server/src/main.rs` while applied:

- initialize `output_buffer_limit` once before the buffered-frame loop;
- use the cached value for borrowed fast replies and the loop pre-check;
- refresh after generic command execution paths that can change client class.

The source hunk was removed after the benchmark failed the keep gate.

Candidate validation while applied:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass99-limit-candidate-check-target cargo check -p fr-server --all-targets
```

Passed remotely on `vmi1227854`.

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass99-limit-candidate-test-target cargo test -p fr-server output_buffer_limit -- --nocapture
```

Passed locally after RCH failed open: `2 passed`.

Candidate release build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass99-limit-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH failed open locally; crate-scoped isolated release build passed.

## Golden Output

Golden comparator:

```text
python3 artifacts/optimization/frankenredis-6tsou.1/candidate/resp_golden_compare.py ...
```

The raw TCP RESP transcript covered PING, SET/GET, GETSET, DEL, MSET/MGET,
INCR, GETDEL, and missing-key reads.

- Baseline SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Candidate SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Equal: `true`

Isomorphism notes:

- Ordering preserved: the candidate only changed how often the same output hard
  limit was recomputed during a buffered-frame pass; command execution and reply
  append points were unchanged.
- Tie-breaking unchanged: no ordered data structure or comparator changed.
- Floating-point unchanged: no FP code touched.
- RNG unchanged: no Redis-visible RNG state touched.
- Output bytes unchanged: golden transcript SHA matched exactly.

## Benchmark

Paired hyperfine, SET P16/300k, 8 runs:

- Baseline: `0.542407996045s +/- 0.016636492696768125`
- Candidate: `0.53842154292s +/- 0.016450700210140928`
- Summary: candidate `1.01x +/- 0.04` faster.

## Decision

Rejected under the Score>=2.0 rule.

- Impact: `0.5`
- Confidence: `2`
- Effort: `1`
- Score: `1.0`

No production source hunk is retained.

## Next Primitive

Do not repeat small output-limit cache variants. The deeper next target is a
per-readable batch metadata capsule for `fr-server` that carries command kind,
client id/class, RESP version, and output-limit state through parsing,
dispatch, and reply emission as one packet. Target at least `1.15x` on SET or
MIXED P16/1M before a keep gate.
