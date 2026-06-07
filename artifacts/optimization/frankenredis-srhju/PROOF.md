# frankenredis-srhju RANDOMKEY positional index proof

## Target

- Bead: `frankenredis-srhju`
- Hotspot: `Store::randomkey_in_db` cloned every selected-DB key through `keys_in_db` before sampling.
- Lever: maintain unordered per-DB physical-key slots plus a physical-key position map. Insert/remove/flush update the index; `randomkey_in_db` expires only selected-DB volatile keys, then samples a slot directly.

## Baseline

Remote calibration via `rch` on `ts1`:

```text
keys=50000 requests=10000 db=0 dbs=1 elapsed_seconds=119.480267495 ops_per_sec=83.696 checksum=9370081989761541920
```

This confirms the clone-all path scales with `keys * requests`.

## Behavior Proof

- Ordering/tie-breaking: `RANDOMKEY` has no ordering contract. `KEYS`/`SCAN` still use `ordered_keys`; the new slots are only for random sampling.
- RNG: unchanged for persistent-key workloads. Baseline and candidate call `next_rand` once per sampled key after the selected DB's volatile keys are expired.
- Floating point: not applicable.
- Expiry semantics: selected-DB volatile keys are still probed with `drop_if_expired` before sampling, preserving expired-key stats and notifications.
- Golden checksum, 10k keys / 2k calls / DB 0: baseline and candidate both produced `14880759090566317216`.

Checksum artifacts:

```text
644f523151beefec3e791a8e2ee51e7914ca05167250e592db4f8c794b7aed24  randomkey-10k-2k-baseline-checksum.txt
ec5f6a7907d33c52dd863de607494d3db7687043f098989b5eee4f3c7e95ec92  randomkey-10k-2k-candidate-checksum.txt
```

## Benchmarks

Paired hyperfine, 10k keys / 2k calls:

```text
baseline:  2.025 s +/- 0.423 s
candidate: 11.3 ms +/- 0.9 ms
delta:     179.45x +/- 39.89 faster
```

Reversed hyperfine, same workload:

```text
candidate: 11.6 ms +/- 2.5 ms
baseline:  1.869 s +/- 0.192 s
delta:     161.02x +/- 38.30 faster
```

Hyperfine artifact hashes:

```text
b637d78f9c9e3c1e102a490f41c0549da56d0010dae80bd272ff98cc0b64d8c8  randomkey-10k-2k-paired-hyperfine.json
8cc226fb0ed9e040f670ed1a5dccf8417b194d773261c9fec17bfc6d5800ca39  randomkey-10k-2k-reversed-hyperfine.json
```

## Validation

- `rch exec -- env CARGO_TARGET_DIR=target-srhju-candidate cargo test -p fr-store randomkey_in_db -- --nocapture`
- `rch exec -- env CARGO_TARGET_DIR=target-srhju-candidate-check cargo check -p fr-store --all-targets`
- `rch exec -- env CARGO_TARGET_DIR=target-srhju-candidate-clippy cargo clippy -p fr-store --all-targets -- -D warnings`
- `cargo fmt -p fr-store --check`
- `git diff --check -- crates/fr-store/src/lib.rs artifacts/optimization/frankenredis-srhju/randomkey_bench/src/main.rs`

## Score

Impact 5 x Confidence 5 / Effort 2 = 12.5. Keep.
