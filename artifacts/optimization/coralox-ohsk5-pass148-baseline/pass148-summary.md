# Pass 148: reject borrowed packed-map overwrite

- Started: 2026-06-12T21:54:00-04:00.
- Parent bead: `frankenredis-ohsk5`.
- Child bead: `frankenredis-aulax`.
- Baseline target: current HSET residual from `scripts/perf_gap_dashboard.sh -n 200000 -P16 -c50 --reps 2`.

## Baseline

```text
cmd          redis         fr   redis/fr
hset        738007     526315      1.40x  FR-SLOWER
```

Profile constraints on this host:

- `perf_event_paranoid=4`, so `perf record` produced no samples.
- `ptrace_scope` blocked attach-mode gdb.
- child `strace -c` was too invasive for reliable throughput evidence.

## Candidate

Changed the packed `HashFieldMap::insert_borrowed` path to locate a borrowed
field once, overwrite in place without allocating an owned field on updates, and
append borrowed fields directly when the value can remain packed.

## Behavior proof

- Focused property test passed while candidate was applied:
  `rch exec -- cargo test -p fr-store --lib hash_field_map_borrowed_insert_equivalent_to_indexmap -- --nocapture`.
- `rch exec -- cargo check -p fr-store --lib` passed.
- `rch exec -- cargo clippy -p fr-store --lib -- -D warnings` passed.
- Raw RESP transcript matched byte-for-byte across baseline/candidate:
  `ebdd2e13dee8907214ea0fa16db4757cafe3fdebc6ef25ea31cd31f1a64db9d1`.
- Transcript commands:
  `PING,HSET,HSET,HSET,HGETALL,HGET,HLEN,HDEL,HSET,HGETALL,QUIT`.

Ordering/tie-breaking/floating-point/RNG proof: the candidate touched only hash
field storage mutation inside `fr-store`; it did not change command dispatch,
reply ordering, key ordering, score comparison, random sampling, hash seeding,
floating-point arithmetic, persistence, propagation, or replication paths.

## Benchmarks

Broad HSET, `-r 100000`, P16, c50, n800k:

- Forward: baseline `1.391 s +/- 0.154`, candidate `1.223 s +/- 0.030`,
  candidate `1.14x +/- 0.13` faster.
- Reversed: candidate `1.251 s +/- 0.061`, baseline `1.252 s +/- 0.084`,
  candidate `1.00x +/- 0.08` faster.

Overwrite-focused HSET, `-r 1`, P16, c50, n3M:

- Forward: baseline `3.287 s +/- 0.046`, candidate `3.273 s +/- 0.072`,
  candidate `1.00x +/- 0.03` faster.
- Reversed: candidate `3.273 s +/- 0.074`, baseline `3.649 s +/- 0.559`,
  candidate `1.11x +/- 0.17` faster with baseline outlier warning.

## Decision

Reject under Score>=2.0. Robust Impact is `0.0`; Confidence is `4.0` that the
lever does not move the target under paired/reversed evidence; Effort is `1.0`;
Score = `0.0`.

The candidate source hunk was reverted before commit. Evidence-only artifacts
are retained for route history.

## Next Route

Do not repeat packed-map overwrite micro-levers. Pass149 should attack a larger
safe-Rust HSET primitive: zero-copy command-packet ownership or parser/runtime
arena reuse that removes per-request allocation/probe overhead as a class.
