# frankenredis-u2r0c pass 87: ZINTERCARD min-card streaming rejection

- Bead: `frankenredis-u2r0c`
- Skill loop: `/repeatedly-apply-skill` over `/extreme-software-optimization`
- Target: profile-backed `ZINTERCARD 2 za zb` on 20k-member sorted sets
- Decision: reject. Source hunk removed because the measured gain was small,
  failed reversed-label confirmation, and did not meet Score>=2.0.

## Baseline and profile

- Baseline binary:
  `/data/projects/.scratch/frankenredis-u2r0c-baseline-target-release/release-perf/frankenredis`
- Baseline binary SHA-256:
  `29b9c937d1044d66a3ea531b7712de766396c4fdcccd9ebf74d4806959ac48ab`
- Redis oracle SHA-256:
  `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
- Candidate binary:
  `/data/projects/.scratch/frankenredis-u2r0c-candidate-target-release/release-perf/frankenredis`
- Candidate binary SHA-256:
  `0ab02e0cbbb54468eb7e33ed7cbd42e337a7b30ecf6eee81cdcfa9dc4e65f6f7`

Current-vs-current-vs-Redis baseline selected the target:

- Baseline/current label: `1304.857 us/op`
- Second current label: `1338.873 us/op`
- Redis 7.2.4 oracle: `888.231 us/op`
- Replies equal: `true`

Perf profile on current main captured 2024 samples with zero lost. Top rows:

- `__memcmp_avx2_movbe`: `19.23%`
- `Store::zget_score_or_set_member`: `13.40%`
- `Store::drop_if_expired`: `13.30%`
- `Store::zget_members_with_scores`: `12.83%`

## Lever tested

The rejected lever replaced command-side first-source materialization and
per-member `zget_score_or_set_member` probes with a store-side
`zintercard_count` primitive:

- Validate all ZINTERCARD sources up front.
- Select the minimum-cardinality source.
- Iterate that source directly without materializing scores.
- Probe borrowed set/sorted-set inputs.
- Preserve source key lookup/touch/LFU behavior as a source-level operation.

This was intentionally broader than a member-only clone reduction, but still
part of the same ZINTERCARD streaming-count family. It did not produce a
credible win and was removed.

## Validation while applied

- RCH/local fail-open `cargo check -p fr-command -p fr-store --all-targets`
  passed.
- RCH/local fail-open
  `cargo test -p fr-command zintercard_member_only_path_preserves_set_first_source_and_limit -- --nocapture`
  passed.
- RCH `cargo clippy -p fr-command -p fr-store --all-targets -- -D warnings`
  passed on worker `ovh-a`.
- `cargo fmt -p fr-command -p fr-store -- --check` still reports pre-existing
  rustfmt drift in old `fr-command`/`lua_eval` lines outside this pass; no
  formatter rewrite was run.

## Isomorphism proof

Golden comparator:
`artifacts/optimization/frankenredis-u2r0c/pass87-zintercard/zintercard_golden.py`

Baseline, candidate, and Redis matched exactly:

- Equal: `true`
- SHA-256:
  `8d9d201533ad80e81ac1b52b487049557cd90752a24347fd998e359214d5970d`

Covered cases:

- Basic `ZINTERCARD 2 za zb`
- `LIMIT 1`
- Set as first source with `LIMIT 1`
- Missing source
- Wrong-type first source
- Wrong-type later source
- Negative LIMIT
- Bad LIMIT token

Ordering and tie-breaking remain irrelevant to the returned cardinality but
were preserved for observable replies. No floating-point aggregation is exposed
by `ZINTERCARD`. RNG/LFU/touch behavior was considered in the candidate design,
but because the source hunk was rejected and removed, final main retains the
pre-pass call ordering exactly.

## Benchmarks

Primary matrix:

- Baseline: `1185.736 us/op`
- Candidate: `1142.913 us/op`
- Redis: `861.228 us/op`
- Candidate vs baseline: `1.037x`
- Reply equal: `true`

Reversed-label matrix, with candidate source on the script's baseline port and
current-main source on the script's candidate port:

- Candidate-source label: `1314.628 us/op`
- Current-main label: `1285.970 us/op`
- Script `candidate_vs_baseline`: `1.022x`, meaning the original current-main
  source won after labels were swapped.
- Reply equal: `true`

Hyperfine:

- Current main: `1.103 s +/- 0.064 s`
- Candidate: `1.073 s +/- 0.051 s`
- Candidate summary: `1.03x +/- 0.08x`

The small one-sided edge is inside noise and fails direct reversal. Score:
`Impact 1 * Confidence 1 / Effort 2 = 0.5`, below the keep threshold.

## Next route

Do not retry this min-card streaming-count family as-is. The deeper follow-up
is `frankenredis-u2r0c.1`: build and prove a source-level validation/probe
capsule that attacks the repeated per-member `drop_if_expired`, hash, stats,
and `zget_score_or_set_member` costs while making Redis-observable keyspace
stats an explicit golden surface. Target ratio before keep: at least `1.20x`
same-worker with reversed-label confirmation.
