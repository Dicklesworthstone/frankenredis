# Pass 57: `frankenredis-mcklt` GET context-refresh skip rejected

- Target profile: `artifacts/optimization/cod-pass56-profile-20260607T0512Z/current-get-p16-1m-server-perf-report.txt` showed `Runtime::refresh_store_runtime_info_context` at 10.99% flat in the GET P16/1M server profile after the rejected `Store::get` lookup-fusion pass.
- Lever tested: remove the full `refresh_store_runtime_info_context()` call from `Runtime::execute_plain_get_borrowed`, relying on the existing conservative GET gate and later generic `INFO`/`CLIENT INFO` refreshes for observable metadata.
- Source result: rejected; the runtime source hunk and GET-specific proof tests were removed after corrected benchmark confirmation. No GET context-refresh skip remains.

## Behavior Proof While Candidate Was Applied

- Ordering: GET remains a single-key read with active-expire before `Store::get` and lazy-expire propagation after the read; no ordering surface changes.
- Tie-breaking: N/A.
- Floating point: N/A.
- RNG: unchanged; `Store::get` LFU path and any internal RNG use remained identical.
- Observable metadata: focused runtime tests showed borrowed GET could defer dispatch-context refresh while generic `CLIENT INFO` and `INFO stats` recomputed before observation.
- Golden raw RESP output: baseline == candidate == Redis 7.2.4, SHA-256 `e7292197fb7e31019ceecf7045b46c201ac8610c06ff49df22a79ae7c9a7047f`.
- Golden `redis-cli` output: baseline == candidate == Redis 7.2.4, SHA-256 `28eb566251907a91724a87d6a8de23f0aa7219f233ca2d2b790744b0b4120465`.

## Validation While Candidate Was Applied

- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-mcklt-runtime-test-target cargo test -p fr-runtime plain_get_borrowed_ -- --nocapture`: passed, 4 tests.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-mcklt-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench`: passed.
- `cargo fmt -p fr-runtime --check`: passed.
- `git diff --check`: passed.

## Benchmarks

Corrected candidate runs rebuilt `/tmp/codex-fr-mcklt-candidate-target/release-perf/frankenredis` after restoring accidental SET/INCR edits and applying only the GET refresh skip.

| Workload | Baseline | Candidate | Result |
| --- | ---: | ---: | --- |
| GET P16/300k paired | `0.523410s +/- 0.054541s` | `0.504330s +/- 0.043446s` | candidate `1.04x +/- 0.14x`, noisy |
| GET P16/1M reversed | `2.045277s +/- 0.465879s` | `1.578747s +/- 0.030280s` | candidate `1.30x +/- 0.30x`, baseline too noisy |
| GET P16/1M paired | `1.587381s +/- 0.063740s` | `1.597678s +/- 0.077215s` | baseline `1.01x +/- 0.06x`, tied |

## Decision

- Score: 0.5 = Impact 1 x Confidence 1 / Effort 2.
- Keep threshold: 2.0.
- Decision: reject. The corrected decisive same-host 1M paired run is tied/slightly negative, so this is not a real win.

## Next Route

Do not retry GET dispatch-context refresh removal. The deeper target remains the residual profile class around `__memcmp_avx2_movbe`, `Store::drop_if_expired`, and time sampling in the borrowed GET loop. The next pass should attack a structurally different primitive: cache/expiry certificate for non-volatile DB reads, parser command-name classification that avoids repeated memcmp, or a batched time-read strategy with exact expiry semantics.
