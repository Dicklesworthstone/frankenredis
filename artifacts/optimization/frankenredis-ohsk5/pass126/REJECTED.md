# frankenredis-ohsk5.23 pass126 rejection

Target: GET/P16/C50 command path after the pass125 dedicated GET histogram slot keep.

Profile-backed hotspot:
- `artifacts/optimization/frankenredis-ohsk5/pass126/profile/perf-get-p16-c50-3m-nochildren.txt`
- Top relevant rows included `[vdso]`/`clock_gettime`, `Store::drop_if_expired`, `Runtime::execute_plain_get_borrowed_into`, `process_buffered_frames`, `parse_command_args_borrowed_into`, and `encode_bulk_string_slice`.

Rejected lever:
- Use the expiry-deadline index in `Store::record_keyspace_lookup` to skip `drop_if_expired` when no TTL deadline can be due at `now_ms`.
- Source hunk was removed after benchmarking because it did not clear the keep threshold.

Baseline:
- GET/P16/C50/1M current: `755.7 ms +/- 28.5 ms`, 10 runs.
- Artifact: `baseline/baseline-get-p16-c50-1m-hyperfine.txt`.

Behavior proof while candidate was under test:
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p126_check cargo check -p fr-store --all-targets`: pass; unrelated pre-existing test warnings only.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p126_test cargo test -p fr-store record_keyspace_lookup_deadline_guard_preserves_lazy_expiry_boundary -- --nocapture`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p126_test cargo test -p fr-store record_keyspace_lookup_drops_redundant_contains_key_ab -- --nocapture`: pass, existing A/B `1.37x`.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_fr_ohsk5_p126_clippy cargo clippy -p fr-store --lib -- -D warnings`: pass.
- `cargo fmt -p fr-store --check`: blocked by broad pre-existing formatting drift in `crates/fr-store/src/lib.rs`; the candidate-owned assertion was manually matched to rustfmt before rejection.
- Golden raw RESP replay: current and candidate both `18465` bytes, sha256 `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`, request sha256 `1d60f9d79b6207fdca3ad00da7a27f36736a4ea704b0714a94f72dbcfe3cd7d1`.
- Ordering/tie-breaking/floating-point/RNG isomorphism: the lever only changed the pre-mutation key-existence probe when the expiry index proves no deadline is due. No command ordering, tie-breaking, floating-point, or RNG paths were touched. The due-expiry branch retained the original `drop_if_expired` path for deletion, propagation, notifications, and stats.

Benchmark result:
- Paired, current first:
  - current: `850.7 ms +/- 55.5 ms`
  - candidate: `844.3 ms +/- 39.5 ms`
  - candidate: `1.01x +/- 0.08`
- Reversed, candidate first:
  - candidate: `822.0 ms +/- 46.8 ms`
  - current: `826.8 ms +/- 25.8 ms`
  - candidate: `1.01x +/- 0.07`

Score:
- Impact `0.1` x Confidence `0.3` / Effort `1.0` = `0.03`.
- Rejected because Score `< 2.0`.

Next route:
- Do not continue the expiry-deadline guard micro-family.
- Use the same profile to attack a deeper GET primitive next: fused borrowed GET request handling around `process_buffered_frames` / `parse_command_args_borrowed_into` / `execute_plain_get_borrowed_into`, with the target ratio at least `1.10x` on GET/P16/C50/1M and the same golden sha256 proof.
