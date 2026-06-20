## Cod-a List LP-Byte Reuse Candidate

Date: 2026-06-20
Agent: CobaltCove
Issue: frankenredis-ohsk5

Candidate:
Thread the already-computed `list_lp_entry_bytes(elem)` result from
`ListValue::add_entry_bytes` into `ChunkedList::{push_back,push_front}` so the
large-list append path does not re-run the canonical integer/listpack sizing
probe for the same pushed element.

Decision:
Rejected. The source hunk is not retained. Same-window control beat the
candidate on the main target cell (`RPUSH`) and tied/slightly beat it on
`LPUSH`.

Profiling:
Kernel profiling was blocked by host policy:
`kernel.perf_event_paranoid = 4`. A direct `perf stat` check failed with
"Access to performance monitoring and observability operations is limited."
The repo profiling helper was not run because it deletes temp files during setup,
which violates this checkout's no-file-deletion rule.

Build:
Both release binaries were built with:
`CARGO_TARGET_DIR=/data/projects/.rch-targets/frankenredis-cod-a rch exec -- cargo build --release -p fr-server`.

Harness:
Vendored Redis 7.2.4 `redis-benchmark`, P16, c50, n150k, 9 interleaved trials.
Both servers reported `connected_slaves=0`.

| command | candidate fr/redis | control fr/redis | candidate/control | verdict |
|---|---:|---:|---:|---|
| lpush | 0.92x | 0.93x | 0.99x | neutral/reject |
| rpush | 0.82x | 0.87x | 0.94x | loss/reject |
| lpop | 1.16x | 1.15x | 1.01x | neutral guard |
| rpop | 1.15x | 1.25x | 0.92x | guard down |
| lrange_100 | 1.06x | 1.05x | 1.01x | neutral guard |
| sadd | 0.85x | 0.83x | 1.02x | neutral guard |
| zadd | 0.75x | 0.77x | 0.97x | guard down |
| set | 1.07x | 1.09x | 0.98x | neutral guard |
| get | 1.00x | 1.01x | 0.99x | neutral guard |
| incr | 1.03x | 1.03x | 1.00x | neutral guard |
| hset | 1.13x | 1.16x | 0.97x | guard down |
| mset | 1.19x | 1.18x | 1.01x | neutral guard |

Artifacts:
- `candidate.patch`: exact rejected source hunk
- `candidate_vs_redis_standard_p16_c50_n150k_trials9_list_family.txt`
- `control_vs_redis_standard_p16_c50_n150k_trials9_list_family.txt`
- `candidate_frankenredis.sha256`
- `control_frankenredis.sha256`

Remaining frontier:
Clean control still loses `RPUSH` (0.87x), `SADD` (0.83x), and `ZADD` (0.77x)
against Redis 7.2.4. The duplicate listpack-size accounting shortcut is not the
list-write lever; route deeper to end-chunk allocation/layout or command-batch
costs, and do not retry this standalone plumbing patch without a profile showing
the second `list_lp_entry_bytes` call dominates.
