# Per-core reactors over a partitioned keyspace

2026-07-31. Host `thinkstation1`, AMD Ryzen Threadripper PRO 5975WX, 32 physical
cores / 64 threads, kernel 6.17.0-35-generic. Rival is the vendored live
`redis-server` 7.2.4 at `legacy_redis_code/redis/src/redis-server`, ELF
`e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`, configured
with its own documented scaling knob (`--io-threads 8 --io-threads-do-reads
yes`, persistence off).

## The contract that had to go

`--experimental-sharded-set-get-workers N` gave each reactor thread sole
ownership of one keyspace partition. A connection was routed to a reactor by
hashing its FIRST command's key, and any later key that hashed elsewhere was
answered `-CROSSSHARD` and the connection closed.

That was measured directly before any change, eight `SET key:1..key:8` down one
socket:

```
+OK
-CROSSSHARD this connection is pinned to a different keyspace partition; ...
```

One real command served, then an error and a dead socket. `redis-benchmark`,
`redis-cli` and every non-cluster driver scatter keys across a single socket, so
the mode could not be pointed at the workload it existed to win — and because
redis-benchmark counts an error reply as a completed request, pointing it there
anyway produces a large and entirely fake throughput number.

## What changed

**Separate WHO EXECUTES from WHAT IS PROTECTED.** Reactors stay one per core and
still own their sockets end to end — the reactor that reads a command also
parses, executes, encodes and writes it, so no command crosses a thread
boundary — but the keyspace became `Arc<Vec<Mutex<Runtime>>>` with `P = 4N`
partitions, and a reactor reaches any key by taking that key's partition lock.
No queue, no envelope, no completion channel, no cross-core wakeup: the whole
handoff apparatus is gone and the only shared state on the hot path is one mutex
acquisition per command.

**Round-robin at accept, not routing by first key.** Once partitions are separate
from reactors, key routing buys nothing and costs everything: in a hot-key
workload every client's first key is IDENTICAL, so every connection lands on the
SAME reactor. Round-robin also lets the acceptor stop reading and parsing a
command before it can place a socket, which removes the `pending` map, its poll
registrations and the first-command parse from the one thread every connection
must pass through.

**Bounded spin with doubling backoff before parking**
(`FR_PARTITION_LOCK_SPINS`, default 48). A partition is held only for the store
hit — tens to a few hundred nanoseconds — while parking on a contended mutex
costs a futex round trip in the microseconds.

Verified byte-exact: 64 scattered keys × all seven served families
(SET/GET/INCR/LPUSH/LPOP/HSET/HGET) pipelined down ONE connection are identical
to live 7.2.4, pinned by
`shared_nothing_connection_serves_scattered_keys_like_legacy_redis`.

## Throughput vs live Redis

`scripts/mixed_workload_connection_sweep.sh`, whole-job wall time over
`-t set,get,incr,lpush,lpop,hset`, n=300,000 per family, P=1, r=100,000, 5
rounds, arm order alternating every round, server and client on **disjoint
16-physical-core cpusets** (server `0-15,32-47`, client `16-31,48-63`), 16
reactors / 64 partitions, bootstrap 95% median CI, second FrankenRedis at
identical config as the A/A null arm. fr RUNNING-IMAGE
`afdc203c19785091582b474969feab5606f1618138f92f219ff53ed4b060bf41`.

| conns | fr ops/s | redis ops/s | **fr/redis** | 95% CI | A/A null | verdict |
|---|---|---|---|---|---|---|
| 8 | 157,827 | 73,675 | **2.1422x** | [2.0639, 2.2698] | 1.0168 | WIN |
| 32 | 347,690 | 133,813 | **2.5983x** | [2.4511, 2.7216] | 0.9992 | WIN |
| 64 | 362,135 | 248,643 | **1.4564x** | [1.2399, 1.5399] | 1.0002 | WIN |
| 128 | 362,090 | 234,900 | **1.5415x** | [1.4773, 1.6738] | 1.0003 | WIN |

At c=1 the same harness lands at 1.2582x [1.0481, 1.4611] against a 1.0631 null,
i.e. **null** — which is the correct shape. There is no parallelism to exploit at
one connection, and a large win there would mean the job was measuring something
other than concurrency.

## Connection placement, isolated (the hot-key fixture)

Placement was landed as key affinity in b9f2decc7 and then measured against
round-robin. Both arms are the SAME binary lineage differing only in how an
accepted socket is placed on a reactor: same partitioned keyspace, same spin
lock, same 16 reactors, same cpuset, one at a time, **arm order alternating per
round**. Fixture `-t lpush,lpop,hset -n 300000 -c 64 -P 1` — redis-benchmark
points all three families at ONE key, the worst case a partitioned design can be
handed. Harness `scripts/hotkey_routing_ab.sh`.

| round | keyed rps | reactors >5% | round-robin rps | reactors >5% |
|---|---|---|---|---|
| 1 | 85,649 | 1/16 | 333,233 | 16/16 |
| 2 | 83,763 | 1/16 | 299,700 | 16/16 |
| 3 | 81,853 | 1/16 | 400,000 | 16/16 |

Medians against live redis-server at 239,808 on the same fixture:

| placement | rps | vs live Redis |
|---|---|---|
| by first key | 83,763 | **0.3493x** |
| round-robin | 333,233 | **1.3896x** |

**3.978x from one placement decision.** Key placement makes the entire
thread-per-core design LOSE to single-threaded redis-server by nearly 3x, because
every client's first command in a hot-key workload carries the same key and so
every connection lands on the same reactor. The `reactors` column is the
mechanism and it is a count, not a stopwatch: one core pegged at ~95% with
fifteen idle, versus sixteen at ~400% aggregate. The partition lock is not
implicated — a futex census on this fixture reports 0.0000 waits per operation,
because only one thread is ever running.

Ask of any connection-placement scheme: *what happens when every client's first
command is identical?*

## Two ways to misread this harness, both hit during this run

**1. The client cpuset can be the thing you are measuring.** A first pass used a
24-physical-core server cpuset against an 8-physical-core client cpuset. fr
plateaued at ~187,000 ops/s at BOTH c=32 and c=64 while the client sat at
663–676% of its 800% ceiling — just under the harness's own 85% CLIENT-BOUND
guard, so nothing was flagged. Rebalancing to 16/16 roughly DOUBLED absolute
throughput (362,135 at c=64) and turned c=128 from `null` into a clean WIN. A
flat ops/s that does not move with concurrency is the signature; check the client
cpuset before believing a server ceiling.

**2. Per-family csv numbers quantize at high rates.** With n=100,000 and both
engines above ~200k ops/s, SET/GET/INCR/LPUSH/LPOP all reported
199,601–200,000 for BOTH arms — redis-benchmark's 250ms tick, not the servers.
Those per-family ratios collapse to ~0.99x and mean nothing. The whole-job wall
time is the quotable figure; raise `-n` before reading any per-family split.

## Scope and limits

* Claims cover the command families the per-core reactor path serves: single-key
  SET, GET, INCR, LPUSH, LPOP, HSET, HGET, plus local PING and QUIT. Cross-key
  and aggregate commands are still refused by this mode.
* The mode remains incompatible with hardened mode, `--config`, `--aof`,
  `--rdb`, `--replicaof` and `--enable-debug-command`.
* Taken at loadavg 19–34 from other tenants compiling on the same box. The
  paired, order-alternated arms and the A/A null (0.9992–1.0168) are what carry
  the verdicts; re-run on a quiet host before quoting as a headline.
* fr used ~700% of a 1600% server ceiling at c=64–128 and the client ~705–750%,
  so neither side was saturated — there is headroom left above these numbers and
  the next ceiling has not yet been named.

## Harnesses

* `scripts/mixed_workload_connection_sweep.sh` — mixed families swept over
  c=1/8/32/64/128 against live Redis, A/A null arm, alternating arm order,
  CLIENT-BOUND guard, per-family split.
* `scripts/hot_key_partition_lock_discriminator.sh` — separates fixture
  (scattered vs hot-key) from lock policy (spin vs park) from rival.
* `scripts/partition_lock_futex_census.sh` — counts futex entries and user
  instructions per operation instead of timing them.
