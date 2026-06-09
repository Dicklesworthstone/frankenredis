# frankenredis-ohsk5.4 rejection

## Target

- Bead: `frankenredis-ohsk5.4`
- Workload: `fr-bench` HSET, 50 clients, pipeline 16, keyspace 10000,
  datasize 3
- Fresh profile: `artifacts/optimization/orangemouse-pass93-current-20260609/hset-p16-profile/`
- Current profile throughput: `493551.83 ops/sec`

Top self rows from the fresh HSET P16/1M profile:

- `Store::internal_entry`: `8.98%`
- `Runtime::refresh_store_runtime_info_context`: `4.45%`
- `PackedStrMap::locate`: `3.92%`
- `parse_command_args_borrowed_into`: `3.11%`
- `HashFieldMap::insert`: `2.77%`
- foldhash `Vec<u8>` key hashing: `2.26%`
- `Store::hset`: `2.13%`
- `__memcmp_avx2_movbe`: `1.68%`
- `execute_plain_hset_borrowed`: `1.65%`

## Lever Tested

Borrowed HSET mutation capsule for the runtime borrowed fast path. The candidate
added a `Store::hset_borrowed` path and switched `execute_plain_hset_borrowed`
to call it, avoiding the generic `Store::internal_entry` path for borrowed HSET.

This deliberately did not change generic HSET or packed field lookup semantics.

## Behavior Proof While Applied

- RCH `cargo test -p fr-store hset_borrowed_matches_generic_hset_observables -- --nocapture`
  passed.
- The focused test compared generic HSET and borrowed HSET over new-field,
  overwrite, insertion order, LFU/object frequency, hash encoding promotion,
  dirty count, digest mutation count, and wrong-type behavior.

The candidate was rejected by the perf gate before a full TCP golden transcript
was needed for a keep decision.

## Benchmarks

Baseline before edit:

- HSET P16/300k: `0.5983109633s +/- 0.0143103008`

Paired 300k gate:

- Baseline: `0.5880823822s +/- 0.0290218240`
- Candidate: `0.5508260319s +/- 0.0284958077`
- Hyperfine summary: candidate `1.07x +/- 0.08` faster

Reversed 1M confirmation:

- Candidate: `1.7757010856s +/- 0.1240665869`
- Baseline: `1.7038294932s +/- 0.0385267296`
- Hyperfine summary: baseline `1.04x +/- 0.08` faster

## Decision

Reject under the Score>=2.0 rule. The 300k gate was only a small directional
win and the reversed 1M confirmation flipped against the candidate.

- Impact: `0`
- Confidence: `4`
- Effort: `2`
- Score: `0`

The production source hunk and candidate-only test were removed. Evidence files
are retained in this directory and the fresh profile directory for auditability.

## Next Route

Stop HSET mutation-capsule micro-levers. The next pass should go deeper into a
safe-Rust keyspace/fingerprint layout or batch command packet only after a fresh
profile confirms the relevant class:

- keyspace/fingerprint layout if key hashing, memcmp, and `internal_entry` /
  `drop_if_expired` remain dominant across multiple P16 workloads;
- batch command packet if broad dispatch/parser/runtime metadata dominates
  collectively across SET/GET/HSET.
