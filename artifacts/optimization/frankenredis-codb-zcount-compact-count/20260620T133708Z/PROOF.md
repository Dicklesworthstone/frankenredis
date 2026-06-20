# cod-b ZCOUNT Compact Count Rejection

Date: 2026-06-20
Issue: `frankenredis-gu5nf`
Base commit: `8f71926895dd9eb9d52569f22954e5954df7bbe3`
Target dir: `/data/projects/.rch-targets/frankenredis-cod-b`

## Candidate

Rejected patch: `zcount_compact_count_candidate.patch`

The candidate changed cold compact full-zset `ZCOUNT` from filtering the
score-bounded slice to returning `window.len()` when all entries were actual
members, with a fallback for injected sentinel entries.

## Binaries

| binary | sha256 |
|---|---|
| control `frankenredis` | `28bfaadf5f4abf0ab07d784572d16fdc8f8bfc5e4724719fb18ea92f70e4991f` |
| candidate `frankenredis` | `32dfc7e30ef2d4791cd721724050dab9f29aa788731cc9b3b724949ab62e8d2a` |
| Redis 7.2.4 server | `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7` |

The control/candidate binaries were retained locally under this artifact
directory but are not committed because the ledger records their hashes and the
text benchmark outputs are sufficient for review.

## Results

| gate | command | ratio | verdict |
|---|---|---:|---|
| control vs Redis 7.2.4 broad | `zcount` | 0.63 | target loss confirmed |
| candidate vs control broad | `zcount` | 1.03 | neutral |
| candidate vs control focused, pipe 5000, trials 21 | `zcount` | 0.982 | rejected |
| candidate vs Redis 7.2.4 broad | `zcount` | 0.65 | still below parity |

Full broad rows are in:

- `control_vs_redis_broad.txt`
- `candidate_vs_control_broad.txt`
- `candidate_vs_control_zcount_focused.txt`
- `candidate_vs_redis_broad.txt`

## Verification

- Candidate focused correctness:
  `cargo test -p fr-store score_bound_count -- --nocapture`
  passed. rch sync timed out and the test ran locally.
- Candidate release build:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo build --release -p fr-server -p fr-bench`
  passed on `vmi1149989`.
- Final source conformance after revert:
  `rch exec -- env CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-b cargo test -p fr-conformance -- --nocapture`
  passed on `hz2`.

Decision: no source kept. Do not retry this exact compact-slice count shortcut
without a fresh profile showing the sentinel-filter scan is the bottleneck.
