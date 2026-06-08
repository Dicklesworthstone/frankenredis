# frankenredis-ferss rejection proof

## Target

- Bead: `frankenredis-ferss`
- Profile-backed hotspot: `artifacts/optimization/frankenredis-pass75-current-profile`
- Workload: alternating `SETEX` / `PSETEX`, 1,000,000 requests, 50 clients,
  pipeline 16, keyspace 1,000,000, value size 3.
- Candidate lever: replace the final `command_key_indexes` linear
  `COMMAND_TABLE` fallback scan with the existing O(1) `command_table_index`
  metadata lookup.

Profile basis:

- `command_table_index`: 1.74% flat / 3.21% children
- `command_key_indexes`: 1.19% flat / 2.26% children
- nearby command metadata/hash rows: `acl_command_selectors_for_argv`,
  `RandomState::hash_one`, `SipHasher::write`

## Baseline

Build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/frankenredis/target-cod-ferss-baseline-rch cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH used local fallback because no worker was admissible.

Standalone baseline hyperfine:

```text
4.5454567429 s +/- 0.0783406468 s
```

## Behavior Proof

Golden transcript:

- valid `SETEX` and `PSETEX`
- `PERSIST` expiry-state proof
- lower/mixed-case commands
- invalid TTL fallback
- non-DB0 behavior
- `MULTI`/`EXEC` fallback

SHA-256:

```text
baseline  dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
candidate dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7  992 bytes
```

Isomorphism:

- Ordering: preserved. The candidate only changed how the same command-table row
  is found for the final fallback path; returned key-index order was unchanged.
- Tie-breaking: preserved. `command_table_index` keeps the first command-table
  occurrence, matching the old linear `.find()` semantics.
- Invalid input: preserved. `command_key_indexes` still returned `[]` for
  invalid UTF-8 before reaching fallback metadata lookup.
- Floating point: N/A.
- RNG: unchanged.
- Pub/Sub channel handling: preserved by the bespoke branch before fallback.

Validation while candidate was applied:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-ferss-candidate-f97b7897c-20260608T1350/target-cod-ferss-candidate-rch cargo build --profile release-perf -p fr-server -p fr-bench
rch exec -- cargo check -p fr-command --all-targets
rch exec -- cargo test -p fr-command command_key_indexes -- --nocapture
```

Candidate source and binary were isolated in a detached scratch worktree at
`f97b7897c`, with only the `fr-command` hunk applied. The candidate release
build used RCH local fallback; `cargo check -p fr-command --all-targets` passed
on `vmi1167313`; the focused `command_key_indexes` test subset passed via RCH
local fallback. `cargo fmt -p fr-command --check` was not clean because the
shared tree already contained unrelated formatting drift in `fr-command` /
`lua_eval`; the candidate hunk was manually formatted and the unrelated drift
was left untouched.

Subagent audit pass: `Aristotle` independently checked the invariants and found
no behavior difference for the final-fallback replacement shape.

## Benchmarks

Paired hyperfine:

- baseline: `4.5643100625 s +/- 0.0355956222 s`
- candidate: `4.6220507328 s +/- 0.0424682809 s`
- summary: baseline `1.01x +/- 0.01` faster than candidate

Reversed-order hyperfine:

- candidate: `4.6922354507 s +/- 0.0707127332 s`
- baseline: `4.6464875006 s +/- 0.1031725512 s`
- summary: baseline `1.01x +/- 0.03` faster than candidate

Score:

- Impact 0, Confidence 5, Effort 1 => `0.0`
- Decision: reject. No production source hunk retained.

## Next Route

Do not retry fixed-command fallback lookup as a standalone lever. The next
profile-backed command-metadata primitive must remove repeated metadata/hash
work as a class, such as a single per-frame command metadata packet threaded
through key extraction, ACL selector lookup, arity/classification, dispatch, and
propagation rewrite, while preserving Redis command ordering and all bespoke key
spec branches.
