# p8wd1 pass222 - grouped stream index nodes

## Target

`frankenredis-p8wd1`: stream RAM residual after arena-backed stream storage and
SAMEFIELDS. The profile-backed residual was per-entry index/node overhead versus
Redis's rax-of-listpack stream nodes. The one lever in this pass groups stream
entry index records into 100-entry stream nodes while keeping the arena payload,
field dictionary, and public `PackedStreamLog` API unchanged.

Alien-graveyard primitive: succinct/cache-local packed data structure replacing
pointer-heavy tree nodes with grouped contiguous stream-node records.

## Baseline

Built locally because ts1/rch was offline:

```text
7eaf24d12d016332f8a0bb64427a3c9ac0a31e8ec78c423127145c54588d8a30  /data/tmp/frankenredis-coralox-pass222-rebased-baseline-target/release-perf/frankenredis
```

Command:

```text
hyperfine --warmup 1 --runs 5 --export-json artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/baseline-stream-rss-hyperfine.json 'python3 artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/stream_load_once.py --server-bin /data/tmp/frankenredis-coralox-pass222-rebased-baseline-target/release-perf/frankenredis --port 31231 --streams 100 --entries-per-stream 1000 --pipeline 128 --json-out artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/baseline-stream-rss-last.json --transcript-out artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/baseline-stream-golden.resp'
```

Result:

```text
mean 3.028s +/- 0.071s
median 3.053s
RSS delta 10448 KiB
106.98752 bytes/entry
load_seconds 2.089818020001985
golden transcript sha256 34149eaaf5911f71631a87db32ce20223fe7760c33040ac2e1c1f792b0f6b141
```

## Candidate

```text
50db89592aedb662364f1bcd66d780ccdbacac7c6ff4ceb79c1e44aa1c7c405d  /data/tmp/frankenredis-coralox-pass222-rebased-candidate-target/release-perf/frankenredis
```

Command:

```text
hyperfine --warmup 1 --runs 5 --export-json artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/candidate-stream-rss-hyperfine.json 'python3 artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/stream_load_once.py --server-bin /data/tmp/frankenredis-coralox-pass222-rebased-candidate-target/release-perf/frankenredis --port 31232 --streams 100 --entries-per-stream 1000 --pipeline 128 --json-out artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/candidate-stream-rss-last.json --transcript-out artifacts/optimization/coralox-pass222-p8wd1-stream-node-grouping/candidate-stream-golden.resp'
```

Result:

```text
mean 2.979s +/- 0.087s
median 3.013s
RSS delta 7712 KiB
78.97088 bytes/entry
load_seconds 2.1001661350019276
golden transcript sha256 34149eaaf5911f71631a87db32ce20223fe7760c33040ac2e1c1f792b0f6b141
```

## Delta

```text
RSS delta: 10448 KiB -> 7712 KiB
RSS reduction: 2736 KiB, 26.19%
bytes/entry: 106.98752 -> 78.97088
bytes/entry reduction: 28.01664
hyperfine median: 3.053s -> 3.013s (flat/slightly faster within noise)
load_seconds in final run: 2.089818020001985 -> 2.1001661350019276
```

Score:

```text
Impact 4 * Confidence 4 / Effort 3 = 5.33
```

## Isomorphism

- Ordering preserved: yes. Each stream node stores sorted `(ms, seq)` ids, nodes
  are keyed by their first id, and `iter`, `keys`, `range`, `first_id`, and
  `last_id` expose the same ascending id order as the former per-entry BTreeMap.
- Tie-breaking unchanged: yes. Stream id ordering is still the tuple order over
  `(ms, seq)`. No command-level tie-breaker changed.
- Floating-point: N/A. Stream ids, field names, and values are integer/byte data.
- RNG seeds: unchanged/N/A. The stream index path does not call RNG.
- Golden outputs: baseline and candidate RESP transcript files are byte-identical
  at sha256 `34149eaaf5911f71631a87db32ce20223fe7760c33040ac2e1c1f792b0f6b141`.
  The transcript includes `XLEN`, `XRANGE`, `XREVRANGE`, `XINFO STREAM`, `DUMP`,
  `DEBUG DIGEST-VALUE`, and `DEBUG DIGEST`.

## Validation

Local, crate-scoped gates:

```text
cargo fmt -p fr-store -- --check
env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass222-rebased-check-target cargo check -j 1 -p fr-store --all-targets
env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass222-rebased-check-target cargo clippy -j 1 -p fr-store --all-targets -- -D warnings
env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass222-rebased-check-target cargo test -j 1 -p fr-store stream -- --nocapture
env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass222-rebased-check-target cargo test -j 1 -p fr-runtime dump_restore_stream -- --nocapture
python3 scripts/stream_cg_reload_gate.py --bin /data/tmp/frankenredis-coralox-pass222-rebased-candidate-target/release-perf/frankenredis --redis-bin legacy_redis_code/redis/src/redis-server
env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass222-rebased-candidate-target cargo build -j 1 --profile release-perf -p fr-server
```

All passed.
