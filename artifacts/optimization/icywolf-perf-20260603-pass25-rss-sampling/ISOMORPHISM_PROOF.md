# frankenredis-zjhte: RSS procfs sampling overhead

## Profile-backed target

Post-`b14855627` SET p16 profiling showed `epoll_ctl` reduced to 154 calls.
The next feasible non-send/recv profile cluster was periodic procfs sampling:

- `read`: 6,553 calls, 5.75% traced time
- `openat`: 823 calls, 1.58% traced time
- `statx`: 818 calls, 0.58% traced time

The source path is `fr-store::read_rss_bytes()`, called from periodic
`record_ops_sec_sample()`. It used `std::fs::read_to_string("/proc/self/status")`
and then scanned for the `VmRSS:` line.

## One lever

Replace `read_to_string` with a safe fixed-buffer read:

- opens the same `/proc/self/status` path
- reads into an 8192-byte stack buffer
- decodes the bytes as UTF-8
- scans for the same `VmRSS:` line
- preserves the existing `None` fallback when open/read/decode/parse fails

No command dispatch, key ordering, networking, replication, persistence, or
mutation logic changes.

## Benchmark

Harness: `fr-bench` against `frankenredis`, 500,000 SET requests, 50 clients,
pipeline 16, keyspace 10,000, datasize 3. Baseline and candidate were built via
`rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.

- Baseline hyperfine: `1.592 s +/- 0.031 s`
- Candidate hyperfine: `1.552 s +/- 0.023 s`
- Speedup: `1.026x`

Syscall mechanism check on 500,000 SET p16:

- `read`: 6,553 calls -> 858 calls
- `statx`: 818 calls -> not present in the candidate top summary
- `openat`: 823 calls -> 854 calls

Score: `Impact 2.0 * Confidence 0.9 / Effort 0.8 = 2.25`; keep gate passes.

## Validation

- `rch exec -- rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`: pass
- `rch exec -- cargo test -p fr-store periodic_sampling_updates_rss_and_peak_memory_stats -- --nocapture`: pass
- `rch exec -- cargo check -p fr-store --all-targets`: pass
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`: pass in the isolated candidate worktree; the shared tree run is blocked by an unrelated in-flight `fr-persist` LZF edit
- `ubs crates/fr-store/src/lib.rs`: existing broad inventory, exit 1; internal fmt/clippy/check/test-build gates clean and no finding is tied to the RSS hunk

## Behavior proof

RSS values are host/process dynamic, so byte-identical `INFO memory` output is
not a stable golden target. The source-level isomorphism is that both versions
read the same procfs file and parse the same `VmRSS:` field into the same
kilobyte-to-byte conversion; failure still returns `None` to the same caller
fallback.

Golden raw RESP trace sent one pipelined byte stream:

1. `SET icyzjhte:k1 v1`
2. `GET icyzjhte:k1`
3. `INCR icyzjhte:n1`
4. `MGET icyzjhte:k1 icyzjhte:n1`
5. `PING`
6. `QUIT`

Baseline and candidate reply bytes are identical:

`3b95e455b5c2fc4f6ba1633bb0c94601d9ae74d66ceaf57642a68ff5067b15b7`

Ordering/tie-breaking are unchanged because the change only affects how a
periodic memory statistic is sampled. Floating-point and RNG behavior are not
involved.

## Artifacts

- `baseline-set-p16-hyperfine.txt`
- `candidate-set-p16-hyperfine.txt`
- `baseline-strace-set-p16.txt`
- `candidate-strace-set-p16.txt`
- `golden-baseline.resp`
- `golden-candidate.resp`
- `golden-resp.sha256`
