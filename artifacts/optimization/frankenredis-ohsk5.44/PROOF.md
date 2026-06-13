# HSET Exact Single-Field Packet Fast Path

## Target

- Bead: `frankenredis-ohsk5.45`.
- Profile-backed source: current HSET P16/C50 gap plus checked-in HSET profiles showing parser/materialization/classifier cost behind the larger store rows. The clearest prior profile recorded `parse_command_args_borrowed_into` at `3.11%` on HSET P16/1M.
- Live perf sampling note: kernel `perf_event_paranoid=4` blocked new `perf record` sampling in this container, so the keep/reject gate used RCH-built hyperfine evidence and the existing checked-in HSET profile rows.

## Lever

One production lever:

- `fr-server` recognizes strict canonical `HSET key field value` packets before generic borrowed multibulk parsing.
- The parser borrows key/field/value directly from `read_buf`, then calls the existing `Runtime::execute_plain_hset_borrowed` path.
- Multi-pair, malformed, limited, odd/empty, noncanonical, and unsupported inputs fall back to the existing generic path.

## Behavior Isomorphism

- HSET mutation semantics remain owned by `execute_plain_hset_borrowed`; this lever only changes how the strict single-field argv slices are obtained.
- Ordering is preserved: one packet is consumed, one reply is produced, and pipelined frames continue in read-buffer order.
- Tie-breaking, floating-point, and RNG behavior are untouched.
- Expiry, dirty counters, LFU/touch state, commandstats, slowlog, latency, active-expire, error counting, persistence propagation, and replication propagation stay inside the existing runtime/store path.

Golden raw RESP transcript:

- Input SHA256: `5d3dcda1af3702d9d225879f3e12044828e55a2313bb8036525b019f2ef50fec`
- Baseline output SHA256: `3f5382e1d53c4edd05021b634066f247064b42354910637d43216c957e455d2d`
- Candidate output SHA256: `3f5382e1d53c4edd05021b634066f247064b42354910637d43216c957e455d2d`
- Output bytes: `183`

The transcript covers canonical single-field HSET, duplicate overwrite, multi-pair HSET fallback, HGET/HGETALL ordering, wrong-type error, odd-arity fallback, and QUIT.

## Benchmarks

Baseline build:

- `CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass172-profile-target rch exec -- cargo build --release -p fr-server -p fr-bench --config 'profile.release.strip=false' --config 'profile.release.debug=1'`

Candidate build:

- `CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-44-candidate-target rch exec -- cargo build --release -p fr-server -p fr-bench`

Initial baseline HSET P16/C50/n1M:

- Baseline: `1.320 s +/- 0.071 s`

Noisy paired HSET P16/C50/n1M:

- Baseline: `1.553 s +/- 0.427 s`
- Candidate: `1.690 s +/- 0.168 s`
- Treated as routing evidence only because the baseline run was noisy.

Candidate-first HSET P16/C50/n2M:

- Candidate: `2.988 s +/- 0.105 s`
- Baseline: `3.484 s +/- 0.189 s`
- Candidate speedup: `1.17x +/- 0.08`

Baseline-first HSET P16/C50/n2M:

- Baseline: `3.042 s +/- 0.175 s`
- Candidate: `2.761 s +/- 0.220 s`
- Candidate speedup: `1.10x +/- 0.11`

Score: keep. Using the conservative n2M gate, impact `1.10`, confidence `0.82`, effort `0.35`, score `2.6`.

## Validation

- `cargo fmt --package fr-server -- --check`
- `git diff --check -- crates/fr-server/src/main.rs`
- `CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-44-test-target rch exec -- cargo test -p fr-server borrowed_plain_hset_packet_parser -- --nocapture`
- `CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-44-check-target rch exec -- cargo check -p fr-server --all-targets`
- `CARGO_TARGET_DIR=/data/tmp/frankenredis-ohsk5-44-candidate-target rch exec -- cargo build --release -p fr-server -p fr-bench`

Clippy caveat: `cargo clippy -p fr-server --all-targets -- -D warnings` is currently blocked by pre-existing lint debt outside this lever:

- `crates/fr-store/src/lib.rs:1382` and `1653`: `clippy::collapsible_if`
- `crates/fr-command/src/lib.rs:26969` and `26970`: `unused_assignments`
- `crates/fr-runtime/src/lib.rs:7739`, `9947`, and `10864`: `clippy::too_many_arguments`

This pass does not edit those files. Follow-up bead: `frankenredis-wc0i6`.
