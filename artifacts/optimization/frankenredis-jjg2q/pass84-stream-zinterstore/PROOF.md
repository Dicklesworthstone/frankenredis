# Pass 85 - Streaming ZINTERSTORE kept

Bead: `frankenredis-jjg2q`
Bundle directory: `pass84-stream-zinterstore`

## Profile Target

The bead's profile-backed compute scan showed residual sorted-set algebra gaps
against Redis 7.2.4 on 20k-member zsets:

- `ZINTERSTORE`: 1.14x slower after the prior foldhash accumulator fix.
- `ZDIFFSTORE`: 1.21x slower.
- `ZINTERCARD`: 1.31x slower.
- `ZUNIONSTORE`: already faster than Redis, kept as a guard case only.

Alien-graveyard / artifact match: replace materialize-then-prune set algebra
with a streaming survivor primitive. The old `Store::zinterstore` cloned the
smallest input into a `HashMap<Vec<u8>, f64>` and then retained survivors. The
kept lever streams the smallest input, probes the other inputs, and inserts only
surviving members directly into the destination `SortedSet`.

## Lever

One production lever in `crates/fr-store/src/lib.rs`:

- Add `ZSetAlgebraInput` as a borrowed adapter over sorted-set and set inputs.
- Preserve the existing touch / LFU / RNG pass before read-only streaming.
- Stream members from the minimum-cardinality input, probe all other inputs,
  aggregate scores in the original key order, and insert only survivors into the
  destination sorted set.
- Remove the intermediate intersection `HashMap` materialization.

No command parsing, command dispatch, reply encoding, Redis-visible ordering,
score aggregation formulas, invalid-input precedence, or destination update
rules changed.

## Builds And Validation

Candidate validation was run in the clean scratch candidate worktree containing
the `fr-store` streaming patch:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-jjg2q-candidate-target-check cargo check -p fr-store --all-targets
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-jjg2q-candidate-target-test cargo test -p fr-store zinterstore_streaming_preserves_dest_source_and_duplicate_inputs -- --nocapture
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-jjg2q-candidate-target-clippy cargo clippy -p fr-store --all-targets -- -D warnings
```

All passed. `cargo fmt -p fr-store -- --check` passed in the shared tree after
formatting.

`ubs crates/fr-store/src/lib.rs` returned nonzero on existing file-wide legacy
inventory (panic/unwrap/security heuristics across the large file), but its
embedded formatting, clippy, cargo check, and test-build gates were clean.

Release-perf binaries:

```text
baseline frankenredis:  f69e6e9af5b752683b88b4902b0a2ca0e9f7617ca33be22bd69f918721fdc7c7
candidate frankenredis: 581b0b4ebd70c943101f39b193016e091e701c5a5908936182f6cf4ac001a3af
```

## Isomorphism Proof

Golden transcript compared baseline, candidate, and Redis 7.2.4:

```text
sha256: 037aabb4ce082bcfdf01d7c00795a5d0bace12034e3bd95b32f788450d3fa712
equal: true
```

Coverage:

- Destination equals source with duplicate inputs.
- Score tie ordering.
- Set input treated as score `1.0`.
- Missing source removes/keeps destination exactly like Redis.
- Wrong-type source returns the same WRONGTYPE error.

Ordering preserved: final result is stored in `SortedSet`, so observed `ZRANGE`
ordering remains score/member ordered. Tie-breaking is unchanged because
`ScoreMember` ordering is unchanged.

Floating-point preserved: each member's score applies `normalize_weighted_score`
and `aggregate_scores` in the same original input-key order as the old retain
loop. No new approximations or reordered reductions were introduced.

RNG preserved: LFU/touch bookkeeping still runs in the same input-key order and
consumes the same `next_rand()` samples before read-only streaming.

## Benchmarks

Workload: 20k-member zsets with overlap, median of 5 repeats, 400 operations per
case, release-perf server binaries, baseline/candidate/Redis on local loopback.

Full matrix:

| Case | Baseline us/op | Candidate us/op | Redis us/op | Candidate vs Baseline | Reply equal |
| --- | ---: | ---: | ---: | ---: | --- |
| `ZDIFFSTORE d 2 za zb` | 3841.991 | 3837.932 | 3167.708 | 1.001x | true |
| `ZINTERSTORE d 2 za zb` | 3901.439 | 2348.868 | 3219.139 | 1.661x | true |
| `ZINTERSTORE d 3 za zb zc` | 2819.230 | 1502.952 | 2248.548 | 1.876x | true |
| `ZINTERCARD 2 za zb` | 1224.317 | 1244.245 | 847.612 | 0.984x | true |
| `ZUNIONSTORE d 2 za zb` | 10289.633 | 10050.848 | 15532.665 | 1.024x | true |

Reversed-label confirmation swapped the script's baseline/candidate labels. The
ratio inverted for the targeted cases:

| Case | Label A us/op | Label B us/op | Label B vs Label A |
| --- | ---: | ---: | ---: |
| `ZINTERSTORE d 2 za zb` | 2228.047 | 3559.335 | 0.626x |
| `ZINTERSTORE d 3 za zb zc` | 1482.238 | 2771.323 | 0.535x |

Paired hyperfine artifacts with baseline/candidate core assignments swapped also
agree with the keep:

- core4 baseline / core2 candidate: baseline `2.515090815s`, candidate
  `1.620386173s`.
- core2 baseline / core4 candidate: baseline `2.692606907s`, candidate
  `1.744846282s`.

## Decision

Keep. Score = `Impact 4 * Confidence 4 / Effort 2 = 8.0`, above the required
Score>=2.0 gate. The targeted ZINTERSTORE gap is closed and candidate is faster
than Redis on the measured 2-key and 3-key ZINTERSTORE cases while preserving
goldens.

Follow-up route: create a separate profile-backed bead for the remaining
ZDIFFSTORE and ZINTERCARD accumulator gaps. Do not broaden this commit beyond
the one kept streaming ZINTERSTORE lever.
