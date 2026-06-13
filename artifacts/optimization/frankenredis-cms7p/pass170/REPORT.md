# Pass 170 - frankenredis-cms7p no-ship report

Bead: `frankenredis-cms7p` (`[perf] Skip list-push lazy-expiry lookup when no expiries exist`).

Target: pass169 GDB-as-parent LPUSH P16/C50 sampling hit `Store::drop_if_expired -> Store::lpush -> Runtime::execute_plain_keyed_values_write_borrowed -> parse_borrowed_multibulk_action`, making the empty-expiry lazy-reap probe the next measurable one-lever candidate.

Baseline:
- LPUSH P16/C50/n300k: `612611.05 ops/sec`, p50 `1105us`, p95 `1409us`, p99 `5859us`.
- Hyperfine LPUSH P16/C50/n1M: `2.728s +/- 0.661s`.

Candidate:
- Gate `Store::lpush` and `Store::rpush` `drop_if_expired` behind `expires_count != 0`.
- Preserve the existing lazy-expiry path whenever any expiry metadata exists.

Behavior proof:
- Golden input SHA256: `c05d3bd93a2b51ea00e0acbc9f3ff77642c929c4bc5bac981281d2b5bce4a1b2`.
- Current output SHA256: `5c269b8508946fe15a294b09e4d28549ca013b905520ff2ac5bebe99c3961f81`.
- Candidate output SHA256: `5c269b8508946fe15a294b09e4d28549ca013b905520ff2ac5bebe99c3961f81`.
- `cmp` confirmed byte-identical current/candidate output.
- Ordering/tie-breaking/floating-point/RNG: unchanged. The trial only skipped an empty expiry-sidecar probe before the same list mutation. List element order, integer replies, wrongtype behavior, dirty accounting, floating-point paths, RNG paths, and tie-breaking paths were not altered.

Validation while applied:
- RCH `cargo test -p fr-store --lib list_push_reaps_expired_key_before_recreate -- --nocapture`: passed.
- RCH `cargo check -p fr-store --lib`: passed.
- `cargo fmt --check --package fr-store`: passed.

Known unrelated blockers:
- RCH all-target test command for `fr-store` failed in pre-existing `crates/fr-store/tests/metamorphic_numeric.rs` because `getset` is called with `Vec<u8>` where the current API expects borrowed bytes.
- RCH `cargo clippy -p fr-store --lib -- -D warnings` failed on pre-existing `collapsible_if` lints at `crates/fr-store/src/lib.rs:1380` and `:1651`.

Candidate benchmark:
- LPUSH P16/C50/n300k: `615301.33 ops/sec`, p50 `1056us`, p95 `1739us`, p99 `5787us`.
- Hyperfine LPUSH P16/C50/n1M: `2.683s +/- 0.817s`.

Decision:
- Reject. The candidate improved n300k throughput by only `+0.44%` and hyperfine by `+1.7%`, both inside run noise.
- Score: Impact `0.2` x Confidence `2.0` / Effort `0.5` = `0.8`, below the `2.0` keep gate.
- Production source hunk and test removed before commit.

Next route: do not repeat list-expiry/hash-probe micro-tuning. Re-profile and attack a deeper read/syscall/event-loop batching or command-packet primitive with fresh baseline and golden proof.
