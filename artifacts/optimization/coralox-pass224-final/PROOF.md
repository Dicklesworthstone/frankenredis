# pass224 final correction: expiry side dictionary

## Scope

Finish the `frankenredis-uhthd` in-flight expiry-side-dict lever and close the
`frankenredis-isa2w` regression: moving absolute key expiries out of `Entry`
must preserve deadlines when keys are copied or relocated.

## Fix

- `COPY` now reinserts the duplicated destination through
  `internal_entries_insert_with_expiry` using the source key's deadline.
- `MOVE` inherits the fix through `copy_no_stat`.
- `swap_prefixes` and `SWAPDB` harvest a key's deadline before removal and
  reinsert it with the moved entry.
- Whole-store `flushdb` clears the side expiry dictionary.

## Performance Evidence

Original pass224 baseline and candidate:

- 1M persistent-key RSS baseline: `252284 KiB`, `258.338816 B/key`
- 1M persistent-key RSS candidate: `235880 KiB`, `241.541120 B/key`
- Delta: `-16404 KiB`, `-16.797696 B/key`, `6.50%` less RSS
- 300k fresh SET load baseline: `5.0276876074s +/- 0.1061673865`
- 300k fresh SET load candidate: `4.9913070783s +/- 0.1596413223`

Corrected candidate after TTL propagation fix:

- 1M persistent-key RSS: `235884 KiB`, `241.545216 B/key`
- 300k fresh SET load: `5.14999062054s +/- 0.20362007783`

The corrected RSS result is effectively identical to the kept candidate
(`+4 KiB` over a 1M-key run); the timing result stays within the noisy local
fresh-server envelope.

## Behavior Proof

- Ordering/tie-breaking: key ordering, sorted SCAN, and random-key slot ordering
  are unchanged; only side-dict deadline propagation changed.
- Floating-point: no floating-point path touched.
- RNG: no RNG path touched.
- Expiry semantics: COPY, MOVE, SWAPDB, prefix swap, and flush now preserve or
  clear the side-dict deadline in the same places the old inline `Entry`
  deadline did.
- Golden from original pass224: baseline and candidate RESP transcript both
  sha256 `bccabd2ef4549002069013eda03d215ffd639122ec06003a741205c5e06204ca`.

## Gates

- `cargo fmt -p fr-store -- --check`
- `cargo check -j1 -p fr-store --all-targets`
- `cargo test -j1 -p fr-store copy -- --nocapture`
- `cargo test -j1 -p fr-store swapdb -- --nocapture`
- `cargo clippy -j1 -p fr-store --all-targets -- -D warnings`
- `cargo build -j1 -p fr-server --profile release-perf` using
  `CARGO_TARGET_DIR=target-coralox-pass224-final`
- `python3 scripts/ttl_semantics_differ.py 27901 27902`
- `python3 scripts/move_swapdb_expiry_gate.py 27901 27902`
- `python3 scripts/copy_command_differ.py 27901 27902`
