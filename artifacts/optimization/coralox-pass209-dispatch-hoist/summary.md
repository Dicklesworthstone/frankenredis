# Pass 209 Dispatch Hoist Summary

Bead: `frankenredis-jp05u`

Source lever: hoist exact keyless borrowed `COMMAND COUNT` and `DBSIZE` checks near the front of `parse_borrowed_multibulk_action`, before the long borrowed recognizer chain. Generic fallback, arity errors, selected-DB gating, command stats, and reply bytes are unchanged.

Rebase note: after this evidence was collected, `origin/main` advanced to `1ffbe19d7` (`perf(fr-server): O(1) first-byte dispatch for borrowed command fast paths`). That remote first-byte dispatcher already routes `COMMAND COUNT` in the `b'C'` bucket and `DBSIZE` in the `b'D'` bucket, so the rebased pass commit records evidence and bead closeout without adding a second `fr-server` source delta.

Baseline:
- Current source: `f07def66b9100822dfd0e6a30d63e423d5f8c6d5`
- Baseline binary SHA256: `fb419289d5090eb3589e0fc565baffb02f5cbf0b62cd021e85c2152dcc90f1a0`
- Redis oracle SHA256: `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`
- `DBSIZE`: Redis `1020408.19 req/s`, FrankenRedis `952380.94 req/s`
- `COMMAND COUNT`: Redis `993377.50 req/s`, FrankenRedis `937500.00 req/s`

Candidate:
- Candidate binary SHA256: `835127e724b1692fb41860435ea977c623bc535326cc2106f5074f44c33815bf`
- First candidate run: `DBSIZE` `1030927.81 req/s`, `COMMAND COUNT` `914634.12 req/s`
- Paired confirmation, 3 x 300k requests, P16/C50:
  - `COMMAND COUNT` mean throughput: `933218.02 -> 974245.44 req/s` (`1.044x`)
  - `COMMAND COUNT` mean p50: `0.636 -> 0.476 ms`
  - `DBSIZE` mean throughput: `1000666.21 -> 968851.15 req/s` (`0.968x`, noisy)
  - `DBSIZE` mean p50: `0.458 -> 0.420 ms`

Behavior proof:
- Request SHA256: `18443b0fc845b1879fc3344fcd2f9042690f3c7288fb22543e88f53b036231ab`
- Redis response SHA256: `36baae7f0daabf1cb0e8dbf4e7c8f9bbc03cce3c04ddfc14df50a106817d7a54`
- Baseline response SHA256: `36baae7f0daabf1cb0e8dbf4e7c8f9bbc03cce3c04ddfc14df50a106817d7a54`
- Candidate response SHA256: `36baae7f0daabf1cb0e8dbf4e7c8f9bbc03cce3c04ddfc14df50a106817d7a54`
- `baseline_candidate_cmp`: `match`
- `redis_candidate_cmp`: `match`

Isomorphism notes:
- Only exact borrowed forms `COMMAND COUNT` and `DBSIZE` move earlier in the dispatch order.
- `COMMAND COUNT` arity/subcommand variants still fall through to generic dispatch.
- `DBSIZE` remains gated to selected DB 0 in the borrowed path; selected DB 1 falls through to generic dispatch.
- Ordering, tie-breaking, floating-point, RNG, and persistence surfaces are not touched.

Gates:
- RCH `cargo test -j 1 -p fr-runtime plain_command_count_borrowed_matches_generic -- --nocapture`
- RCH `cargo test -j 1 -p fr-runtime plain_dbsize_borrowed_matches_generic -- --nocapture`
- RCH `cargo check -j 1 -p fr-server --all-targets`
- RCH `cargo clippy -j 1 -p fr-server --all-targets -- -D warnings`
- `cargo fmt -p fr-server -- --check`
- `git diff --check`
- `ubs crates/fr-server/src/main.rs` remains nonzero on pre-existing whole-file inventory; clippy/check/fmt sections are clean.

Score: `1.044 * 0.90 / 0.35 = 2.68`. Keep.
