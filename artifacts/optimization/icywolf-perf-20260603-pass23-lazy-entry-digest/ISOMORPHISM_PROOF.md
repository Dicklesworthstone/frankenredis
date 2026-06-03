# frankenredis-gu5nf.25 Isomorphism Proof

## Change

`Store::internal_entries_insert` now marks the whole-store digest stale after a
whole-entry insert/replace instead of eagerly hashing the old and new entry
state on the SET hot path.

This is one runtime lever: lazy digest invalidation for whole-entry store
updates. The accompanying test-only edits are mechanical proof hygiene: rustfmt
wraps four existing assertion calls, and the Redis d2string oracle test allows
two intentional floating-point literal lints so `cargo clippy -p fr-store
--all-targets -- -D warnings` can verify the crate.

## Profile-Backed Target

Fresh isolated rch baseline from `frankenredis-gu5nf.23`:

- SET pipeline=16 direct: `228143.06 ops/sec`, p50 `3247 us`, p95 `4839 us`,
  p99 `5891 us`.
- Hyperfine: `352.171 ms +/- 12.489 ms`.
- GDB sample on 1M SET pipeline=16 hit `entry_state_digest` / `fnv1a_update`
  through `Store::internal_entries_insert`, plus store hash lookup work.

## Performance Result

Short paired SET pipeline=16 hyperfine, 100k requests, 12 runs:

- Baseline: `356.9 ms +/- 14.8 ms`.
- Candidate: `347.9 ms +/- 9.2 ms`.
- Delta: candidate `1.03x +/- 0.05x`.

Short paired direct last run:

- Baseline: `274959.49 ops/sec`, p50 `2747 us`, p95 `3645 us`, p99 `3977 us`.
- Candidate: `291275.23 ops/sec`, p50 `2587 us`, p95 `3415 us`, p99 `3799 us`.

Long paired SET pipeline=16 hyperfine, 500k requests, 8 runs:

- Baseline: `1.795 s +/- 0.045 s`.
- Candidate: `1.719 s +/- 0.048 s`.
- Delta: candidate `1.04x +/- 0.04x`.

Long paired direct last run:

- Baseline: `267659.73 ops/sec`, p50 `2843 us`, p95 `3781 us`, p99 `4391 us`.
- Candidate: `283102.49 ops/sec`, p50 `2651 us`, p95 `3703 us`, p99 `4443 us`.

Campaign score: `Impact 2.5 * Confidence 0.85 / Effort 1.0 = 2.125`.
Score keep gate: PASS (`>= 2.0`).

## Isomorphism Obligations

- Ordering preserved: yes. Key insertion still uses the same `ordered_keys`
  branch before `entries.insert`; no iteration order or dispatch order changed.
- Tie-breaking unchanged: yes. No sorted-set, stream-ID, or key-selection
  comparison logic changed.
- Floating-point: N/A for runtime lever. The only floating-point source touched
  by this pass is a test-only lint allowance around existing Redis d2string
  oracle literals.
- RNG seeds: N/A. No random source or seed changed.
- Mutation counts preserved: yes. Replacement still copies old entry
  `modification_count.wrapping_add(1)` before insertion.
- Key/expiry counters preserved: yes. `db_key_counts`, `db_expires_counts`,
  stream side-metadata cleanup, and key removal side effects remain in the same
  branches as before.
- Digest observable preserved: yes. Existing `state_digest()` recomputes from
  `state_digest_full_scan()` whenever `digest_stale` is true; the change only
  moves hashing off the mutation path and onto the already-supported digest
  query path.

## Golden Output Proof

Raw RESP trace:

1. `SET k v1`
2. `GET k`
3. `DEBUG DIGEST`
4. `SET k v2`
5. `GET k`
6. `DEBUG DIGEST`
7. `SET other x`
8. `DEBUG DIGEST-VALUE k other`

Baseline and candidate transcripts are byte-identical:

```text
7ed2b55f462682fe0c7e097ad4395dee8ea4ac76be65d45daf4c8d68c1af919e  golden-baseline.resp
7ed2b55f462682fe0c7e097ad4395dee8ea4ac76be65d45daf4c8d68c1af919e  golden-candidate.resp
sha256sum -c golden-resp.sha256: OK
```

## Validation

- `rch exec -- cargo test -p fr-store whole_entry_insert_marks_digest_stale_until_state_digest_recompute -- --nocapture`: PASS.
- `rch exec -- cargo test -p fr-store state_digest_matches_full_scan_after_direct_mutation_paths -- --nocapture`: PASS.
- `rch exec -- cargo test -p fr-store state_digest_stays_stale_after_incremental_update_follows_direct_mutation -- --nocapture`: PASS.
- `rch exec -- cargo fmt -p fr-store --check`: PASS.
- `rch exec -- cargo check -p fr-store --all-targets`: PASS.
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`: PASS.
