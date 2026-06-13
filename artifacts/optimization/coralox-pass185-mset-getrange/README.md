# Pass 185 - rejected direct-encoded GETRANGE bulk reply

Bead: `frankenredis-5gisf`

Target: profile-backed `GETRANGE bigstr 0 -1` residual from the pass185 broad
big-command dashboard. Current main measured Redis `1.397ms` vs FrankenRedis
`1.603ms` for a 1MB GETRANGE row (`1.15x` Redis/FR). Focused current-main
baseline with a fresh server and 1MB `bigstr` measured `704.5ms +/- 46.2ms`
for `redis-benchmark -n 1000 -c 1 -P 1 -q getrange bigstr 0 -1`.

Candidate rejected: expose `Store::getrange` bytes as a borrowed slice when the
value is string-backed, then let the borrowed server path encode the bulk string
directly into the client output buffer. This removed the intermediate
`RespFrame::BulkString(Vec<u8>)` clone for large string GETRANGE replies.

Behavior proof while applied:
- Raw RESP input SHA256:
  `9ba7015e6ecbac2b9bf887dd9c8110f12489bfb231579496af699c2a170393b0`
- Baseline and candidate raw output SHA256:
  `ecad1fd42ed14c8ee7d52827603f459ffa6ee8632476ee410d773f771ad9a8ee`
- Transcript covered successful full/partial ranges, negative ranges,
  negative inverted empty range, clamped fully-negative range, missing key,
  wrong-type error, integer-encoded value, bad-integer fallback, `CLIENT REPLY
  SKIP`, and `QUIT`.
- Ordering, type-check precedence, clamp semantics, suppression semantics,
  floating-point behavior, score tie-breaking, and RNG behavior were unchanged.

Validation while applied:
- RCH `cargo check -p fr-server --all-targets` passed on `vmi1227854`.
- RCH `cargo test -p fr-runtime plain_getrange_borrowed -- --nocapture` passed
  on `vmi1149989` (`3` tests passed). The run also surfaced an existing
  unrelated test-target `unused_mut` warning at `crates/fr-runtime/src/lib.rs`.
- RCH `cargo clippy -p fr-runtime --lib -- -D warnings` passed on `vmi1149989`.
- RCH `cargo clippy -p fr-server --bin frankenredis -- -D warnings` passed on
  `vmi1149989`.
- RCH `cargo clippy -p fr-store --lib -- -D warnings` passed on `vmi1227854`.
- RCH `cargo build --release -p fr-server` built the candidate on
  `vmi1153651`.

Benchmark:
- Paired hyperfine, fresh server per run, 1MB `bigstr`,
  `redis-benchmark -n 1000 -c 1 -P 1 -q getrange bigstr 0 -1`.
- Baseline: `678.9ms +/- 30.3ms`.
- Candidate: `662.9ms +/- 31.4ms`.
- Ratio: candidate `1.02x +/- 0.07` faster.

Score: Impact `0.4` x Confidence `1.0` / Effort `1.0` = `0.4`; reject.
Production source and candidate-only test were removed before commit. Next
route should not repeat GETRANGE reply-copy micro-work; attack a deeper
multi-argument command-packet / parser-arena primitive for MSET or another
freshly profiled hotspot.

Verdict: REJECTED / NO PRODUCTION SOURCE KEPT.
