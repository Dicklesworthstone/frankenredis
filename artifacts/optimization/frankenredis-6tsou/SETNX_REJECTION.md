# frankenredis-6tsou SETNX Borrowed Fast-Path Rejection

Timestamp: 2026-06-08T03:52:17Z

## Target

- Bead: `frankenredis-6tsou`
- Workload family: plain RESP write commands after the APPEND borrowed fast-path keep
- Candidate lever: add a dedicated borrowed fast path for `SETNX`
- Gate: keep only if same-harness before/after evidence supports Score >= 2.0

## Profile-Backed Context

Pass 1 command-family baseline, P16 / 300k requests:

| Mode | Mean |
| --- | ---: |
| `setnx-miss` | 2.308s +/- 0.035 |
| `getset-hit` | 2.425s +/- 0.059 |
| `append` control | 2.524s +/- 0.092 |
| `setnx-hit` | 2.597s +/- 0.333 |
| `psetex` | 2.647s +/- 0.259 |
| `setex` | 2.823s +/- 0.418 |
| `getdel-hit` | 4.578s +/- 0.153 |

The focused `SETNX` profile did not show the candidate fast path as a dominant
hotspot. `Store::setnx_borrowed` accounted for only 0.62% self time in the
captured candidate profile, while harness/Python, kernel polling, runtime info
refresh, and lookup/compare overhead dominated.

## Same-Harness Benchmarks

`SETNX` hit workload:

| Binary | Mean |
| --- | ---: |
| Baseline `HEAD` | 2.043s +/- 0.034 |
| Candidate | 2.031s +/- 0.032 |

Result: 1.006x faster, within noise.

`SETNX` miss workload:

| Binary | Mean |
| --- | ---: |
| Baseline `HEAD` | 2.101s +/- 0.031 |
| Candidate | 2.097s +/- 0.042 |

Result: 1.002x faster, within noise.

## Decision

Reject. Impact is too small for the added command-specific surface.

Score: Impact 1 x Confidence 2 / Effort 2 = 1.0, below the required 2.0 gate.

## Isomorphism Note

The candidate was removed from production source, so there is no behavior
change to prove or retain. The implementation-specific tests were also removed
because the fast-path API no longer exists. The generic `SETNX` path remains the
sole production path.

Ordering/tie-breaking/floating-point/RNG: unchanged, because production code is
back at the generic path and this command has no floating-point or RNG surface.

## Next Primitive

Proceed to `GETSET` profiling before any implementation. `GETSET` remains a
profile-backed Pass 2 candidate from the command-family baseline and has a
larger observable payload-return surface than `SETNX`; if it still underperforms
the Score gate, pivot to the deeper alien-graveyard families: zero-copy RESP
framing, output batching, arena/slab response reuse, and inline small frames.
