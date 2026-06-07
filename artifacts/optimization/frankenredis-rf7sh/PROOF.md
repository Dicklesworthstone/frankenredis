# frankenredis-rf7sh rejection proof

## Target

- Bead: `frankenredis-rf7sh`
- Profile-backed hotspot: Pass 56 GET P16 server profile showed
  `Runtime::refresh_store_runtime_info_context` at `10.99%` under borrowed GET.
- Lever tested: skip the full store runtime-info context refresh in
  `Runtime::execute_plain_get_borrowed` while preserving generic-command refresh
  before `INFO` / `CLIENT` metadata observation.

## Baseline and candidate

- Fresh baseline source state: current shared tree, including the live peer
  `fr-command` ZINTERCARD edit, with only the GET refresh line restored for the
  release binary.
- Candidate source state: same shared tree plus one runtime lever, replacing the
  GET refresh with a no-refresh borrowed fast-path.
- Baseline build command:
  `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-fresh-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`
- Candidate build command:
  `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`
  - RCH had no admissible worker and fell open locally for this crate-scoped
    candidate build. `release-perf` does not set `target-cpu=native`; both
    benchmarked server binaries ran on the local host.

Binary hashes:

```text
90b735368fcf436486c62b257941f008d743f078170379bcd9291f5d3fb3ed7f  /tmp/codex-fr-rf7sh-fresh-baseline-target/release-perf/frankenredis
62f125561e86b6c9f5eb4aa689fd688985891bede5102b87c44d1b5020244bd9  /tmp/codex-fr-rf7sh-fresh-baseline-target/release-perf/fr-bench
75f189dcebbd73e76db5a6941ee9c1d3f3b3629e397fc75fcd6e289ab195df82  /tmp/codex-fr-rf7sh-candidate-target/release-perf/frankenredis
c80f40c833c4acf27057b613e45f6c13e8860ff02ebedca971823e66248fa9b3  /tmp/codex-fr-rf7sh-candidate-target/release-perf/fr-bench
```

## Isomorphism proof

- Ordering and tie-breaking: unchanged; the candidate only removed a metadata
  context refresh from a single-key GET fast path after the existing borrowed
  gate had accepted the command.
- Floating-point: not used.
- RNG: not used.
- Store mutation and reply semantics: unchanged; `Store::get` remained the data
  path for hit, miss, and wrongtype. Generic dispatch remained unchanged.
- Metadata observability: candidate tests proved generic `CLIENT INFO` and
  `INFO stats` recomputed deferred context after borrowed GET.
- Gate coverage: candidate tests proved GET still fell back for auth, ACL
  restrictions, non-db0, CLIENT NO-TOUCH, MULTI/EXEC, pubsub mode, client pause,
  maxmemory, AOF, replica state, tracking, monitor clients, and script nesting.

Golden TCP transcript:

- Commands: `PING`, `SET`, `GET` hit, `GET` miss, `LPUSH`, `GET` wrongtype,
  `MGET` ordered hit/miss, `DBSIZE`, `QUIT`.
- Baseline and candidate response bytes matched exactly.
- Golden SHA-256:

```text
8178da50e7685fddbaef1bf8cde80d9834f7c6d5bcace93a6a54cfc563f9e8a9  artifacts/optimization/frankenredis-rf7sh/golden-baseline.resp
8178da50e7685fddbaef1bf8cde80d9834f7c6d5bcace93a6a54cfc563f9e8a9  artifacts/optimization/frankenredis-rf7sh/golden-candidate.resp
```

## Validation

Candidate-source validation before rejection:

- `cargo fmt -p fr-runtime --check` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-runtime-check-target-2 cargo check -p fr-runtime --all-targets` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-runtime-clippy-target-2 cargo clippy -p fr-runtime --all-targets -- -D warnings` passed after reflowing an existing SCARD doc comment.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-runtime-test-target cargo test -p fr-runtime plain_get_borrowed -- --nocapture` passed: 4 matching tests.

Final rejected-source validation:

- `cargo fmt -p fr-runtime && cargo fmt -p fr-runtime --check` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-final-clippy-target cargo clippy -p fr-runtime --all-targets -- -D warnings` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-rf7sh-final-test-target cargo test -p fr-runtime plain_get_borrowed -- --nocapture` passed: 1 matching test.

## Benchmarks

All runs used local TCP servers, one `fr-bench` client binary, `--clients 50`,
`--pipeline 16`, `--workload get`, `--keyspace 10000`, and `--datasize 3`.

```text
baseline                0.5524629051533333  +/- 0.046594771426518404  300k paired
candidate               0.5924745196533333  +/- 0.07097059095563414   300k paired
candidate_1m            1.7459153073200002  +/- 0.20548826531651457   1M reversed
baseline_1m             2.298494508195      +/- 0.1629986673516757    1M reversed
baseline_1m_normal      1.6662322332        +/- 0.22607958214510626   1M normal
candidate_1m_normal     1.6040740309000001  +/- 0.0994816369757493    1M normal
candidate_300k_rev      0.6321003289733335  +/- 0.04073550412896688   300k reversed
baseline_300k_rev       0.5497683710566668  +/- 0.06798538274366887   300k reversed
```

Decision:

- Reject under Score >= 2.0.
- The candidate was slower at 300k in both orders.
- The 1M normal-order run was only `1.04x` faster and inside variance.
- The single strong 1M reversed run was not enough to overcome the paired and
  reversed 300k regressions.
- Production source hunk and candidate-only tests were removed before commit.

## Next target

Do not repeat runtime-info refresh micro-skips. Re-profile and attack the deeper
GET path now exposed by the prior profile: `Store::drop_if_expired`,
`__memcmp_avx2_movbe`, and key lookup/expiry checks. The next algorithmic
primitive should be a structural store fast-read path, e.g. a non-expiring
resident hot-key read map or versioned no-expiry lane that removes repeated
expiry/hash/memcmp work for default GET while preserving Redis expiry semantics.
Target ratio: `>=1.20x` on GET P16 before any keep.
