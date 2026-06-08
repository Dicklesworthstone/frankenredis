# Pass 86 - ZDIFFSTORE survivor-pairs accumulator kept

Bead: `frankenredis-1jhwe`
Follow-up bead for remaining ZINTERCARD gap: `frankenredis-u2r0c`

## Profile Target

Current-main baseline was built via RCH with the repo's `release-perf` profile:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-1jhwe-baseline-target-release cargo build -p fr-server --profile release-perf
baseline frankenredis sha256: 7976d4f54507840f01a45d93dc3411d07c156b9ee1123746c2f01088a9761380
redis-server sha256:       e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7
```

Baseline current/current/Redis matrix on 20k-member zsets:

| Case | Current A us/op | Current B us/op | Redis us/op | Reply equal |
| --- | ---: | ---: | ---: | --- |
| `ZDIFFSTORE d 2 za zb` | 3786.022 | 3584.780 | 2989.289 | true |
| `ZINTERCARD 2 za zb` | 1206.124 | 1232.117 | 809.202 | true |

Focused `perf record` against current-main `zdiff2` captured 3159 samples, zero
lost. Top rows:

- `BTreeMap`/`ScoreMember` search+insert path: 17.28%.
- `__memcmp_avx2_movbe`: 8.79%.
- `Store::zget_members_with_scores`: 5.97%.
- `Store::zget_score_or_set_member`: 5.21%.
- `__memmove_avx_unaligned_erms`: 5.06%.
- `foldhash::RandomState::hash_one`: 3.95%.

The profile and baseline point at the ZDIFFSTORE survivor accumulator and final
destination build path, not command parsing or server I/O.

## Lever

One production lever:

- Replace `ZDIFFSTORE`'s temporary `HashMap<Vec<u8>, f64>` survivor accumulator
  with `Vec<(Vec<u8>, f64)>`.
- Add `Store::store_sorted_set_from_pairs`, mirroring
  `Store::store_sorted_set` destination removal, stream metadata cleanup, empty
  result deletion, and dirty increment behavior.
- Keep source validation, `zget_members_with_scores`, and every
  `zget_score_or_set_member` probe in the same order as before.

No `ZINTERCARD` production path changed; it is tracked separately in
`frankenredis-u2r0c`.

## Isomorphism Proof

Golden transcript across baseline, candidate, and Redis 7.2.4:

```text
sha256: cf52308d9c843e2ca35b0dea182415de5f4da2ddeb94511e8240e70563baca9e
equal: true
```

Golden coverage:

- `ZDIFFSTORE` destination equals first source.
- Empty result removes destination.
- Set source operands are scored as membership only.
- Missing source keeps all first-source members.
- Wrong-type source returns the same error.
- `ZINTERCARD LIMIT` guard remains byte-identical.

Ordering preserved: final storage still inserts into `SortedSet`, so externally
observed zset ordering remains score ascending with member byte-lex tie-breaks.

Floating-point preserved: ZDIFFSTORE copies first-source scores unchanged; there
is no score aggregation or reduction in this command.

Keyspace stats, LFU/touch, and RNG preserved: the source validation pass,
`zget_members_with_scores(keys[0])`, and all `zget_score_or_set_member` calls
remain in the same order. The new helper runs only after all reads/probes have
finished and does not call `next_rand`.

Destination semantics preserved: destination removal, stream side metadata
cleanup, empty-result behavior, and dirty increments match the old
`store_sorted_set` helper.

## Validation

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-1jhwe-candidate-target-check cargo check -p fr-command -p fr-store --all-targets
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-1jhwe-candidate-target-test cargo test -p fr-command zdiffstore_preserves_dest_source_scores_and_order -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-1jhwe-candidate-target-clippy cargo clippy -p fr-command -p fr-store --all-targets -- -D warnings
```

All passed.

`cargo fmt -p fr-command -p fr-store -- --check` reports pre-existing rustfmt
drift in old `fr-command/src/lib.rs` and `fr-command/src/lua_eval.rs` lines
outside this patch. The patch-specific hunk is clean under the RCH/UBS embedded
format gate.

`ubs crates/fr-command/src/lib.rs crates/fr-store/src/lib.rs` returned nonzero
on existing file-wide inventories. Its embedded formatting, clippy, cargo check,
and test-build gates were clean.

Candidate release-perf build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-1jhwe-candidate-target-release cargo build -p fr-server --profile release-perf
candidate frankenredis sha256: d9d26ae47d25cced90d64e6dfb3d930a41d8a067f8ea17d96b60b26930db9669
```

## Benchmarks

Workload: 20k-member zsets with overlap, 500 operations per repeat, median of 7
repeats, release-perf server binaries on local loopback.

Primary matrix:

| Case | Baseline us/op | Candidate us/op | Redis us/op | Candidate vs Baseline | Reply equal |
| --- | ---: | ---: | ---: | ---: | --- |
| `ZDIFFSTORE d 2 za zb` | 3844.412 | 2769.177 | 2892.640 | 1.388x | true |
| `ZINTERCARD 2 za zb` | 1247.487 | 1193.199 | 1094.562 | 1.045x | true |

Reversed-label confirmation:

| Case | Optimized label us/op | Current-main label us/op | Redis us/op | Current-main vs Optimized |
| --- | ---: | ---: | ---: | ---: |
| `ZDIFFSTORE d 2 za zb` | 3282.976 | 4022.973 | 3076.526 | 0.816x |
| `ZINTERCARD 2 za zb` | 1241.792 | 1258.784 | 824.334 | 0.987x |

Target hyperfine:

```text
baseline zdiff2:  2.072 s +/- 0.179 s
candidate zdiff2: 1.468 s +/- 0.079 s
candidate ran 1.41 +/- 0.14 times faster
```

## Decision

Keep. Score = `Impact 4 * Confidence 4 / Effort 2 = 8.0`, above the required
Score>=2.0 gate.

The targeted `ZDIFFSTORE` gap is closed for this workload: candidate is 1.388x
faster than current-main and 1.045x faster than Redis in the primary matrix,
with swapped-label and hyperfine confirmation. The remaining ZINTERCARD gap is
filed as `frankenredis-u2r0c` for the next profile-backed one-lever pass.
