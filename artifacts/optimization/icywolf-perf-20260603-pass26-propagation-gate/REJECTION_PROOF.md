# frankenredis-g4nfc: disabled-propagation classification gate

## Profile-backed target

Post-`9e4572a9c` SET pipeline=16 profiling showed the remaining hot cluster:

- `sendto`: 31,250 calls, 68.40% traced time
- `recvfrom`: 31,300 calls, 28.10% traced time
- `epoll_wait`: 668 calls, 1.84% traced time

GDB samples on a longer run still showed a secondary runtime path:

`capture_aof_record -> command_advances_replication_offset -> fr_command::is_write_command -> classify_command`

on the no-AOF/no-replica SET workload.

## Candidate lever

Move the existing disabled-propagation gate in `ServerState::capture_aof_record`
before command classification. For a master with no replica ever connected and
AOF disabled, the previous path returned without side effects after
classification; the candidate returned before classification. Active AOF and
replication paths kept the existing classification and record order.

## Benchmark

Harness: `fr-bench` against `frankenredis`, 500,000 SET requests, 50 clients,
pipeline 16, keyspace 10,000, datasize 3. Baseline and candidate were built via
`rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.

First run:

- Baseline hyperfine: `1.717 s +/- 0.143 s`
- Candidate hyperfine: `1.640 s +/- 0.039 s`

The baseline variance was too high, so a paired hyperfine kept both servers
running on separate ports and alternated both benchmark commands:

- Paired baseline: `1.632 s +/- 0.060 s`
- Paired candidate: `1.689 s +/- 0.045 s`
- Result: baseline ran `1.03x +/- 0.05x` faster than candidate

## Verdict

Rejected. The candidate failed the keep gate and no source change is retained.

Isomorphism: final tree has no source change, so ordering, tie-breaking,
replication offsets, AOF capture ordering, floating-point behavior, and RNG
behavior remain unchanged. Golden RESP proof was not run because the candidate
failed the performance gate before retention.

Next attack: the profile points back to the socket/response path, not command
classification. The next target should be a deeper response batching or
submission-queue primitive for the `sendto`/`recvfrom` cluster.
