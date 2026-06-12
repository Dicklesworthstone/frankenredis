# Pass 149: reject HSET expiry-lookup reuse

- Started: 2026-06-12T22:31:56-04:00.
- Parent bead: `frankenredis-ohsk5`.
- Child bead: `frankenredis-2vxpv`.
- Baseline target: HSET remained the current worst measured residual in the
  pass149 one-command dashboard.

## Baseline

```text
cmd          redis         fr   redis/fr
hset        746268     602409      1.24x  FR-SLOWER
```

Alien-graveyard primitive used: §5.10 region/probe-elimination guidance and
the escape tree path "GC/allocation stalls -> Region alloc / slab". The specific
one-lever candidate reused the existence lookup already performed by
`drop_if_expired` so `hset_borrowed` could skip a redundant top-level
`entries.contains_key(key)` probe on live keys.

## Behavior proof

- `rch exec -- cargo check -p fr-store --lib` passed.
- `rch exec -- cargo clippy -p fr-store --lib -- -D warnings` passed on worker
  `vmi1149989`; worker `hz1` lacked the nightly clippy component.
- Raw TCP RESP transcript matched byte-for-byte across baseline/candidate:
  `48f320000103c17289157ebb7a72461e1d990da40420ab21419c113802b7c5bc`.
- Transcript covered normal HSET, existing-field HSET, wrongtype HSET,
  expired-key replacement, LFU policy mode, and `OBJECT FREQ`.

Ordering/tie-breaking/floating-point/RNG proof: the candidate reused an existing
boolean from `drop_if_expired` and did not change command order, reply order,
hash field ordering, floating-point operations, random sampling conditions,
hash seeds, persistence, propagation, or replication paths. LFU-visible output
matched in the golden transcript.

## Benchmarks

Broad HSET, `-r 100000`, P16, c50, n800k:

- Forward: baseline `1.211 s +/- 0.022`, candidate `1.186 s +/- 0.030`,
  candidate `1.02x +/- 0.03` faster.
- Reversed: candidate `1.198 s +/- 0.061`, baseline `1.212 s +/- 0.042`,
  candidate `1.01x +/- 0.06` faster.

Overwrite-focused HSET, `-r 1`, P16, c50, n3M:

- Forward: baseline `3.277 s +/- 0.066`, candidate `3.202 s +/- 0.067`,
  candidate `1.02x +/- 0.03` faster.
- Reversed: candidate `3.326 s +/- 0.099`, baseline `3.373 s +/- 0.116`,
  candidate `1.01x +/- 0.05` faster.

## Decision

Reject under Score>=2.0. The measured gain is real-looking but too small for
the project gate: Impact `0.2` x Confidence `2.0` / Effort `1.0` = `0.4`.

The candidate source hunk was reverted before commit. Evidence-only artifacts
are retained for route history.

## Next Route

Stop top-level keyspace-probe micro-levers. Pass150 should attack a larger
safe-Rust parser/runtime ownership primitive: avoid per-HSET value `Vec`
materialization where possible by moving command packet ownership or a request
arena through the server/runtime boundary.
