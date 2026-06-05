# frankenredis-hje76 rejection proof: RDB streaming CRC

## Profile-backed target

- Bead: `frankenredis-hje76` (`[perf] fr-persist stream RDB CRC during encode`).
- Profile target: `fr-persist` RDB multidb encode harness, `--dbs 4096 --entries-per-db 2 --iters 500`.
- Profile evidence: `baseline-perf-report.txt` kept `encode_rdb_internal` as the top sampled symbol, with RDB encode work and Redis CRC still in the output path.
- Phase evidence: `baseline-phase-profile.txt` recorded `encode_ns=314891524` for 1000 profile iterations over the synthetic multidb corpus.

## Lever tested

One production lever was tested and rejected: fold Redis CRC64 while appending RDB bytes, so `encode_rdb_internal` no longer performs a second full-buffer `crc64_redis(&buf)` pass before writing the trailer.

The candidate used a single writer wrapper around the existing `Vec<u8>` append path. It preserved the existing RDB record order, DB/key sorting, expiry encoding, score bytes, and checksum little-endian trailer. No floating-point arithmetic, tie-breaking, or RNG path was changed.

## Behavior proof

Candidate and baseline golden RDB bytes matched exactly:

```text
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95  baseline-golden.rdb
9c0b4109f8ece11500fd11e9d559c1c70cade75ca87a60ff78b890e9fc627e95  candidate-golden.rdb
```

`rch exec -- env CARGO_TARGET_DIR=target-icywolf-pass49-harness-candidate cargo build --release --manifest-path artifacts/optimization/crimsonfalcon-perf-20260602/fr-persist-rdb-multidb/Cargo.toml` passed before candidate measurement.

`rch exec -- env CARGO_TARGET_DIR=target-icywolf-pass49-candidate-test cargo test -p fr-persist crc64 --profile release-perf -- --nocapture` passed while the candidate was applied, including the candidate-only split-boundary streaming CRC proof.

## Benchmark result

Paired hyperfine, 10 runs, from `paired-hyperfine-iters500.json`:

```text
baseline:  173.88607798 ms +/- 13.41396014 ms
candidate: 176.47740908 ms +/-  3.57023095 ms
ratio:       baseline ran 1.01x +/- 0.08 faster
```

This does not clear the campaign keep gate. Score is below 2.0 because impact is negative in the current paired run.

## Decision

Rejected. The production source hunk was removed, and no `fr-persist` source change is retained from this lever.

## Next primitive

Do not continue micro-tuning CRC streaming for this profile. The next deeper target is an RDB emit planner: preserve `(db, key)` output ordering while reducing global sort pressure and allocation/copy pressure in `encode_rdb_internal`. Target ratio: at least 1.15x on the same RDB multidb harness before considering the lever keepable.
