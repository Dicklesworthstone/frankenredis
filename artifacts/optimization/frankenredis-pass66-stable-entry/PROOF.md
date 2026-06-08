# Pass 66 Single-Probe GET Rejection

## Target

- Bead: `frankenredis-ejzjv`
- Baseline source: `4a175fa6a`
- Clean scratch worktree:
  `/data/projects/.scratch/frankenredis-pass66-stable-entry-20260607194730`
- Build path:
  `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass66-target cargo build --profile release-perf -p fr-server -p fr-bench`

The shared repo had live peer `fr-server` edits, so all source profiling and
candidate work happened in the clean detached worktree.

## Baseline And Profile

Baseline GET P16/300k, 7-run hyperfine:

- mean `399.8 ms +/- 11.0 ms`
- last-run throughput `786405.93 ops/sec`
- last-run p99 `555 us`

User-space GET P16/1M profile:

- throughput `803121.03 ops/sec`
- p99 `611 us`
- `__memcmp_avx2_movbe` `16.12%`
- vDSO/`clock_gettime` path `15.36%`
- `Store::drop_if_expired` `13.20%`
- `Runtime::refresh_store_runtime_info_context` `4.86%`
- `Runtime::execute_plain_get_borrowed` `2.46%`
- `process_buffered_frames` `2.30%`

Profile report: `baseline-get-p16-1m-user-perf-report.txt`.

## Lever Tested

Candidate patch: `candidate-single-probe-get.patch`.

For `Store::get` only, when the database has no volatile keys and LFU tracking
is disabled, the candidate used one `entries.get_mut(key)` probe and recorded
hit/miss directly. Any TTL-bearing DB or LFU maxmemory policy fell back to the
existing `record_keyspace_lookup` / `drop_if_expired` path.

This is broader than the earlier rejected no-TTL branch inside
`drop_if_expired`: it removes the separate lazy-expiry probe for the default
no-expiry GET path rather than adding a branch inside that helper.

## Behavior Proof

Raw TCP RESP transcript covered:

- default no-TTL string hit
- default no-TTL miss
- wrong-type GET
- `OBJECT IDLETIME` after GET touch
- TTL-bearing key fallback
- existing key read while a volatile key is present
- LFU policy fallback
- `OBJECT FREQ` after LFU GET

Baseline and candidate emitted 158 identical bytes.

Golden SHA-256:
`eb1b713a84af987f324af7ea7e2dc818684b3cbe7664511029be7657c00b2111`

Ordering, tie-breaking, and floating-point behavior are not touched. RNG
behavior is preserved because the fast path is disabled whenever LFU tracking is
enabled; missing GETs therefore do not consume an LFU random sample.

## Validation

- `cargo fmt -p fr-store --check` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass66-candidate-check cargo check -p fr-store --all-targets` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass66-candidate-clippy cargo clippy -p fr-store --all-targets -- -D warnings` passed.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-pass66-candidate-target cargo build --profile release-perf -p fr-server -p fr-bench` passed.

## Benchmarks

GET P16/300k paired, 7 runs:

- baseline `471.8 ms +/- 26.7 ms`
- candidate `461.4 ms +/- 16.8 ms`
- candidate `1.02x +/- 0.07` faster, inside noise.

GET P16/1M reversed, 5 runs:

- candidate `1.321 s +/- 0.022 s`
- baseline `1.317 s +/- 0.023 s`
- baseline `1.00x +/- 0.02` faster, tied.

## Decision

Reject under Score>=2.0.

- Impact: 0
- Confidence: 1
- Effort: 2
- Score: `0 = 0 x 1 / 2`

No production source hunk was kept.

Next route: stop no-expiry/drop-if-expired micro variants. The fresh profile
now points at two deeper classes:

- timekeeping / active-expire scheduling: vDSO + `clock_gettime` path is
  `15.36%`, mostly under active expire and borrowed GET command timing.
- actual stable-entry data layout: `__memcmp_avx2_movbe` remains `16.12%`, but
  it needs a real entry-id table or command-batch key-handle model, not a
  branch around the current hash table.
