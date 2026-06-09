# Pass100 Keep Proof: plain SET stream sidecar cleanup

## Target

- Bead thread: `frankenredis-ohsk5`.
- Current-head profile artifact:
  `artifacts/optimization/orangemouse-pass99-current-profile-20260609/`.
- Profile-backed hotspot, SET P16/1M:
  - `RandomState::hash_one::<&[u8]>`: `12.33%` self / `12.80%` children.
  - `core::hash::sip::Hasher<Sip13Rounds>::write`: `5.18%` self /
    `6.46%` children.
  - `Store::internal_entries_insert`: `1.07%` self / `2.99%` children.
  - The children report also showed std-hash stream sidecar removals, including
    `HashMap<Vec<u8>, (u64, u64), RandomState>::remove::<[u8]>`.
- Alien-graveyard primitive class: hot data-plane sidecar elimination. The
  lever removes redundant non-command sidecar probes from the tight SET path.

## Lever

`Store::set` used to call:

```text
self.stream_groups.remove(key.as_slice());
self.stream_last_ids.remove(key.as_slice());
```

for every plain SET, even when the existing key was not a stream. The canonical
old-value cleanup already lives in `internal_entries_insert`: if the replaced
entry was a stream and the new entry is not, it removes stream groups, stream
last-id state, entries-added state, and max-deleted-id state. This pass removes
only the two unconditional SET-side probes and adds a regression test for stream
overwrite cleanup through the shared insertion path.

## Baseline

Baseline build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH failed open locally because no admissible workers were available; the command
remained crate-scoped and used an isolated target dir.

One-sided baseline hyperfine, SET P16/300k:

- Baseline: `0.6845226818s +/- 0.05962397191894168`.

## Behavior Proof

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

- Ordering preserved: the only changed production behavior is avoiding two
  unconditional sidecar hash removals before insertion; the actual stream
  cleanup point remains inside the same `Store::set` call through
  `internal_entries_insert`.
- Tie-breaking preserved: no comparator, sorted set, BTree, SCAN, or RANDOMKEY
  ordering path changed.
- Floating-point preserved: no FP code touched.
- RNG preserved: no Redis-visible RNG state touched.
- Stream metadata preserved: focused test
  `set_stream_key_clears_stream_sidecars_on_overwrite` proves plain SET over a
  stream still clears stream groups, stream last-id, entries-added, and
  max-deleted-id sidecars.

## Validation

- `cargo fmt --check -p fr-store`: passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-check-target cargo check -p fr-store --all-targets`: passed after local RCH fail-open.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-test-target cargo test -p fr-store stream -- --nocapture`: passed after local RCH fail-open, `76` inline stream tests plus `8` stream metamorphic tests and `1` swapdb metamorphic test.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-test2-target cargo test -p fr-store set_stream_key_clears_stream_sidecars_on_overwrite -- --nocapture`: passed after local RCH fail-open.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-clippy2-target cargo clippy -p fr-store --all-targets -- -D warnings`: passed remotely on `vmi1227854`.
- `ubs crates/fr-store/src/lib.rs`: nonzero on broad pre-existing file-wide
  findings; embedded fmt, clippy, cargo check, and test-build sections were
  clean and no reported finding targeted the two-line production hunk.

Candidate release build:

```text
rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass100-stream-sidecar-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench
```

RCH failed open locally; crate-scoped isolated release build passed.

## Benchmark

Paired hyperfine, SET P16/300k, 8 runs:

- Baseline: `0.521057115895s +/- 0.028036587629830214`.
- Candidate: `0.4970074648950001s +/- 0.012653255791844233`.
- Summary: candidate `1.05x +/- 0.06` faster.

Reversed confirmation hyperfine, SET P16/1M, 6 runs:

- Candidate: `1.4439431463066665s +/- 0.03132960438177529`.
- Baseline: `1.4776175743066666s +/- 0.020869336653885455`.
- Summary: candidate `1.02x +/- 0.03` faster.

Last-run fr-bench 1M counters:

- Candidate: `736235.9924724788 ops/sec`, p50 `1042us`, p95 `1306us`, p99
  `1809us`.
- Baseline: `697607.2533027716 ops/sec`, p50 `1080us`, p95 `1475us`, p99
  `1847us`.

## Decision

Kept under the Score>=2.0 rule.

- Impact: `1`.
- Confidence: `2`.
- Effort: `1`.
- Score: `2.0`.

The win is modest but directly attacks a profiled std-hash sidecar path,
preserves stream overwrite semantics, and removes work from every plain SET.

## Next Profile Route

Re-profile the pushed main. Expected shifted targets are still SET keyspace
hashing/comparison and `refresh_store_runtime_info_context`; avoid repeating
small output-limit, try-flush, or SET stream-sidecar variants.
