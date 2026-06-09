# frankenredis-ohsk5.6 Rejection Proof

## Target

- Bead: `frankenredis-ohsk5.6`
- Lever: switch `fr-server` event-loop `HashMap`/`HashSet` registries from std
  SipHash to `foldhash::quality`
- Profile evidence: pass95 SET P16/1M current-head profile showed
  `SipHasher::write` at `12.62%` self in `frankenredis`
- Parent commit for isolated comparison: `c85f25527`

## Isolation

- Baseline worktree: `/data/projects/.scratch/frankenredis-ohsk56-baseline`
- Candidate worktree: `/data/projects/.scratch/frankenredis-ohsk56-candidate`
- Candidate contained only the `fr-server`/`Cargo.lock` foldhash hunk.
- The unrelated shared-tree stream parity work was excluded from the comparison.

## Behavior Proof

- Golden comparator: `artifacts/optimization/frankenredis-6tsou.1/candidate/resp_golden_compare.py`
- Baseline transcript: `resp-golden-baseline.txt`
- Candidate transcript: `resp-golden-candidate.txt`
- Baseline SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Candidate SHA-256: `cd37cbcdc1c44b04bcd11c2644c6ed0233cbc87eca53c9edfab159e9d8a748f3`
- Equal: `true`

Isomorphism:

- Ordering preserved: per-client read, parse, deferred replay, and write-buffer
  append order are untouched; blocked-key FIFO remains `VecDeque`; timeout order
  remains `BinaryHeap`.
- Tie-breaking unchanged: no score/rank/tie comparator changed.
- Floating-point: N/A.
- RNG: user-visible RNG unchanged; hash seeds are not part of Redis-visible
  output.
- Expiry: expiry math and time reads unchanged.

Security note: `BlockedWakeIndex.by_key` hashes client-controlled Redis keys.
`foldhash` would reduce HashDoS resistance versus SipHash, so the lever requires
a clear benchmark win to justify keeping. It did not produce one.

## Benchmarks

SET P16/300k paired, 8 runs:

- Baseline: `0.5304333017s +/- 0.0144884337`
- Candidate: `0.5289788372s +/- 0.0189245460`
- Hyperfine summary: candidate `1.00x +/- 0.05`

SET P16/1M reversed, 5 runs:

- Candidate: `1.6703246832s +/- 0.0468934548`
- Baseline: `1.6731883738s +/- 0.1010037626`
- Hyperfine summary: candidate `1.00x +/- 0.07`

## Decision

Rejected under Score>=2.0.

- Impact: `0`
- Confidence: `4`
- Effort: `1`
- Score: `0`

Production source hunk removed. Evidence retained in this directory.

Next route: stop event-loop hasher swaps and re-profile for a different
primitive class, likely reply/output batching or keyspace layout/fingerprint
reduction only if the fresh profile supports it.
