# frankenredis-1cbca rejection: span argv scratch

Bead: `frankenredis-1cbca`

Lever tested: replace per-command borrowed argv `Vec<&[u8]>` allocation in the strict multibulk server path with a reusable span vector plus transient borrowed slices.

Profile-backed target:
- Baseline profile artifact: `artifacts/optimization/frankenredis-1cbca/baseline/get-p16-1m-server-perf-flat.txt`
- Relevant rows: `Value::string_owned` 10.59%, `__memmove_avx_unaligned_erms` 7.19%, `Runtime::refresh_store_runtime_info_context` 6.17%, `parse_command_args_borrowed_into` 1.13%, `process_buffered_frames` 1.09%.
- Literal `writev` was not pursued because `fr-server` already coalesces replies into one contiguous `write_buf` before flushing; the profile pointed at parser/allocation work instead.

Baseline:
- Build: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1cbca-baseline-target cargo build --profile release-perf -p fr-server -p fr-bench`
- Hyperfine baseline artifact: `artifacts/optimization/frankenredis-1cbca/baseline/get-p16-300k-hyperfine.json`
- Baseline mean: 502.9 ms +/- 15.4 ms for GET pipeline=16, 300k requests.

Validation:
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1cbca-protocol-target cargo test -p fr-protocol parse_command_arg_spans_into -- --nocapture`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1cbca-check-target cargo check -p fr-server --all-targets`: pass.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1cbca-server-test-target cargo test -p fr-server process_buffered_frames -- --nocapture`: pass.
- Golden raw RESP SHA-256: baseline `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`, candidate `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`, equal true.
- Isomorphism: parser validation, command ordering, reply bytes, tie-breaking, floating point, and RNG behavior are unchanged. The candidate only changed temporary argument metadata storage before dispatch; no command semantics or response ordering changed.

Paired result:
- Artifact: `artifacts/optimization/frankenredis-1cbca/candidate/get-p16-300k-paired-hyperfine.json`
- Baseline: 510.0 ms +/- 11.4 ms.
- Candidate: 504.5 ms +/- 12.1 ms.
- Hyperfine summary: candidate 1.01 +/- 0.03x faster.

Decision:
- Rejected. The measured delta is inside noise and does not meet Score >= 2.0.
- Production code changes were reverted; evidence artifacts are retained.
- Next deeper route: parser/event-loop structural work, not another per-command Vec micro-tweak. Candidate targets include zero-copy RESP frame metadata packets that carry command id + argument spans into dispatch, or event-loop batched read/process scheduling that amortizes clocks, runtime info refresh, and parser setup over larger request batches.
