# Pass 214 Summary - Current-Head Large-SET Residual Closeout

## Decision

Evidence-only closeout for `frankenredis-4krcd`. No production source change was
attempted because the fresh current-head baseline did not confirm a scoreable
large-SET recv/syscall gap.

## Current Head

```text
897cb010173dbc71dde4c64a94096403386d73a5
```

RCH build:

```text
rch exec -- env CARGO_TARGET_DIR=/data/projects/.scratch/frankenredis-coralox-pass214b-target cargo build --profile release-perf -p fr-server -p fr-bench
```

Binary SHA256:

```text
frankenredis dc402b78f712258b540c2d7996216b21e1d4135e4b127343624737cb8df12fc4
fr-bench     a3b6a98e2596edd36c307c8cb3a7c1010156d8e6bd2470c8fb85c3b1520d66a1
redis-server e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7
redis-bench  8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b
```

Environment:

```text
loadavg=16.66 12.69 10.90
perf_event_paranoid=4
```

Kernel `perf` remains unavailable locally, so this pass used rch-built timing
and command-level residual sweeps as routing evidence.

## Fresh Baseline

Default P16/C50/n300k median-ratio dashboard:

```text
set=1.07x get=1.08x incr=1.08x lpush=1.03x rpush=1.02x
sadd=0.98x hset=1.05x zadd=1.03x spop=1.06x mset=1.33x lrange_100=1.14x
```

All tested commands were median `>=0.9x` vs vendored Redis 7.2.4.

Large-value gate:

```text
SET 262144B  fr=14875 op/s  redis=16198 op/s  ratio=0.92x
SET 1048576B fr=4197 op/s   redis=4331 op/s   ratio=0.97x
GET 1048576B fr=3037 op/s   redis=1501 op/s   ratio=2.02x
```

All SET/GET sizes were `>=0.9x`; no row clears a Score>=2.0 implementation
target.

## Isomorphism

No source code changed in this pass.

- Ordering preserved: yes; no execution, parser, store, persistence, or output
  code changed.
- Tie-breaking unchanged: yes; no data-structure or comparator code changed.
- Floating-point: N/A.
- RNG seeds: unchanged; no command semantics changed.
- Golden outputs: N/A for production source because this is an evidence-only
  closeout.

## Alien Route

Canonical graveyard routing still points to deeper primitives only after a real
profile row exists:

- Tail decomposition: separate queueing, service, network/io, retries,
  synchronization, and allocator terms.
- I/O syscall overhead: evaluate `io_uring`/registered buffers only with an
  epoll-vs-uring profile and fallback policy.
- Allocation stalls: region/slab command storage is eligible only if a fresh
  userspace profile names parser/argv allocation or buffer ownership transfer.

Recommendation contract for the next valid child:

```text
Change: perf-capable userspace/syscall profile for the next sub-0.9x row.
Hotspot evidence: none currently; pass214 invalidates large-SET as a source target.
Mapped graveyard sections: §0.1, §0.15, §15.8, tail-latency decision tree.
EV score: implementation EV not scoreable until a real hotspot appears.
Priority tier: A for profiling, no production lever yet.
Fallback: keep epoll/current read path unless profile + benchmark proves a win.
Isomorphism proof plan: raw RESP golden transcript for the exact command row.
Rollback: no source change in profiling pass; later levers must be one commit.
Baseline comparator: vendored Redis 7.2.4 and current rch-built frankenredis.
```

## Artifact Hashes

```text
0e7b2a4c44656230ec38e2aa7462008a4afb1589f9e9b0dc61822888afb5e4b6  bench-vs-redis.txt
deff46bf7dee3e836d3185afe2ebd40d298379dd8bebe6355e21b7d2c75eee58  env.txt
01a5decfcf6b72d395301fc1579f3ea69d7945261f30f8d3a6224b078c1d0817  large-value-gate.txt
30eec8a42f128766c45e360b7bf6b71c247072b1ab7e0cc07118ca0382e0a427a  frankenredis.log
e5df358035337ef5255e65151a20c6c3a923cb481e59fd155b820f05a348c615  redis-server.log
```

## Next Route

Close `frankenredis-4krcd` as invalidated/no-source. Re-check the ready queue.
Do not retry large-SET buffer reuse, reusable read slabs, or static chunk tuning
without a future below-gate large-value row plus syscall/userspace attribution.
