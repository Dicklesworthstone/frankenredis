# frankenredis-6m959 - Rejected full-drain clear branch

## Target

- Hotspot: `fr-server::ClientConnection::try_flush`.
- Candidate lever: use `write_buf.clear()` when a nonblocking flush fully drains the buffer, and keep `write_buf.drain(..total_written)` only for partial writes.
- Result: rejected. The golden output stayed byte-identical, but the target benchmark regressed.

## Benchmark

Command shape:

```text
fr-bench --host 127.0.0.1 --workload set --requests 50000 --clients 50 --pipeline 16 --datasize 3 --keyspace 10000
```

Hyperfine:

- warmup: 2
- runs: 5
- prepare: `FLUSHALL` before each run
- server mode: `strict`

Results:

| Build | Hyperfine mean | fr-bench ops/sec | p50 us | p95 us | p99 us |
| --- | ---: | ---: | ---: | ---: | ---: |
| before | 406.8 ms +/- 9.3 ms | 126,074.12 | 6,091 | 7,807 | 9,263 |
| after | 419.5 ms +/- 23.3 ms | 119,887.94 | 6,107 | 8,327 | 9,183 |

Verdict:

- wall time regressed by 3.1%
- throughput regressed by 4.9%
- p95 regressed by 6.7%
- code change discarded

## Isomorphism Proof

Deterministic RESP transcript:

```text
FLUSHALL
PING
SET clear:golden bar
GET clear:golden
```

SHA256:

```text
request: efbd095092ef9ed587900a01eeded2dc52b5ffacc67f70ba80ea2543fdbbd818
before output: 7bb6b41852c5de6128967ecb3e278117fb2baf1947a54c5a6d935155c53c321c
after output: 7bb6b41852c5de6128967ecb3e278117fb2baf1947a54c5a6d935155c53c321c
```

Behavior was unchanged, but performance did not improve. No source change was kept.
