# Pass 125 KEEP: dedicated GET command histogram slot

Bead: `frankenredis-ohsk5.22`
Agent: `TealOtter`
Commit target: keep

## Profile-backed target

Fresh current-main GET/P16/C50 evidence before editing:

- Baseline hyperfine GET/P16/C50/1M: `783.7 ms +/- 42.2 ms`.
- Server-only GET/P16/C50/3M profile: `1,382,159.018 ops/sec`, p50 `527us`, p95 `811us`, p99 `1126us`, p999 `2205us`.
- `CommandHistogramTracker::record_canonical_with_kind` remained visible after the writer-ownership rejections, while the post-writer profile had moved enough cost away from pure syscall topology to justify an accounting-layout lever.

## Lever

Add a dedicated `get: Option<CommandHistogram>` slot to `CommandHistogramTracker`, matching the existing dedicated `set` slot.

This avoids the per-GET generic `HashMap::entry(command.to_string())` path in command accounting while preserving the public commandstats surface:

- `record_canonical_with_kind("get", ...)` records into the dedicated slot.
- `get("GET")` and `get("get")` still resolve through lowercase canonicalization.
- `all()` still returns command names sorted lexicographically.
- `reset([])` and `reset(["GET"])` retain the same count/reset semantics.

## Behavior proof

Golden TCP transcript was byte-identical:

- Request sha256: `1d60f9d79b6207fdca3ad00da7a27f36736a4ea704b0714a94f72dbcfe3cd7d1`
- Baseline response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- Candidate response sha256: `6c61663c963aa6031093aa8691fec4a07a8542c43c78b1bbdc5ce9b26cc14c3b`
- Response bytes: `18465`
- Pipeline GET replies: `2049`

Isomorphism notes:

- Command execution, key lookup, expiration policy, reply bytes, ordering, and side effects are unchanged.
- No floating-point or RNG behavior is introduced or changed.
- INFO commandstats ordering is preserved because `all()` still sorts after collecting specialized and generic slots.
- Counter/reset semantics are covered by the focused `fr-store` command histogram test and the `fr-runtime` commandstats test.

## Gates

Passed:

- `rch exec -- cargo test -p fr-store command_histogram_record_canonicalizes_and_reports_ab_ratio -- --nocapture`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo clippy -p fr-store --lib -- -D warnings`
- `rch exec -- cargo test -p fr-runtime info_commandstats_emits_per_command_call_counts -- --nocapture`
- `rch exec -- cargo build -p fr-server -p fr-bench --profile release-perf`
- `git diff --check -- crates/fr-store/src/lib.rs`

Recorded pre-existing blockers outside this hunk:

- `cargo clippy -p fr-store --all-targets -- -D warnings` fails on unrelated test-helper unused-import/unused-mut lints in existing `fr-store` tests.
- `cargo fmt -p fr-store -- --check` reports broad pre-existing rustfmt drift in older sorted-set/test code. The kept hunk has no whitespace errors, and no broad formatting churn is included in this commit.
- `ubs $(git diff --name-only --cached)` scanned the single changed Rust file and returned nonzero on broad pre-existing whole-file findings such as unwrap/panic/test-helper/perf-smell reports outside the histogram hunk. The full report is stored at `validation/ubs-staged.txt`; no UBS finding points to the kept lines.

## Benchmark result

Paired GET/P16/C50/1M:

- Baseline: `839.4 ms +/- 15.9 ms`
- Candidate: `716.9 ms +/- 53.3 ms`
- Hyperfine summary: candidate `1.17x +/- 0.09` faster

Reversed GET/P16/C50/1M:

- Candidate: `708.7 ms +/- 51.9 ms`
- Baseline: `870.0 ms +/- 53.0 ms`
- Hyperfine summary: candidate `1.23x +/- 0.12` faster

Last-run JSON confirms throughput movement:

- Paired baseline: `1,305,177 ops/sec`
- Paired candidate: `1,453,859 ops/sec`
- Reversed candidate: `1,505,794 ops/sec`
- Reversed baseline: `1,313,440 ops/sec`

Score: Impact `3` x Confidence `3` / Effort `1` = `9.0`; keep gate `>=2.0` satisfied.

## Post-keep profile route

Candidate post-profile GET/P16/C50/3M:

- `1,387,761.995 ops/sec`
- p50 `537us`, p95 `814us`, p99 `1261us`
- `CommandHistogramTracker::record_with_kind` fell to `0.29%`; `record_canonical_with_kind` is no longer a top row.

Top shifted rows:

- kernel/vdso/`clock_gettime@@GLIBC_2.17`, including `Store::drop_if_expired` under the time path
- `Runtime::execute_plain_get_borrowed_into`
- `frankenredis::process_buffered_frames`
- `fr_protocol::parse_command_args_borrowed_into`
- `Runtime::plain_borrowed_default_key_read_allows`
- `fr_protocol::encode_bulk_string_slice`

Next route: attack a time/expiry sampling or borrowed GET execution-layout primitive with Redis-observable expiration/order proof. Do not repeat writer queue topology/ownership, tiny sync flushes, buffer-capacity reuse, direct parser shortcuts, or histogram-slot specialization.
