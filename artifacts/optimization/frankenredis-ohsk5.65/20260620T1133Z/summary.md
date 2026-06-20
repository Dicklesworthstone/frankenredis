# frankenredis-ohsk5.65 front-biased list chunk

Decision: keep.

Source lever: `ListChunk::Owned` can store active front chunks in reversed
physical order. Repeated `LPUSH` appends to the physical tail instead of shifting
the whole chunk with `Vec::insert(0, ...)`. Logical order is preserved by
translated `get`, forward/reverse iterators, quicklist DUMP export, and
normalizing arbitrary mutation paths.

Measured performance:

| artifact | gate | result |
|---|---|---|
| `control_vs_redis_list_writes.txt` | current-control vs Redis 7.2.4 | LPUSH 0.72x, RPUSH 0.81x, SADD 0.84x, ZADD 0.78x |
| `candidate_vs_redis_list_writes.txt` | candidate vs Redis 7.2.4 | LPUSH 0.85x, RPUSH 0.89x, SADD 0.86x, ZADD 0.74x |
| `candidate_vs_control_list_writes.txt` | candidate vs current-control | LPUSH 1.104x, RPUSH 1.013x, SADD 1.027x, ZADD 1.030x |
| `candidate_vs_control_lpush_confirm.txt` | focused LPUSH confirmation | LPUSH 1.170x |

Correctness and gates:

| gate | result |
|---|---|
| `rustfmt --edition 2024 --check crates/fr-store/src/packed_set.rs` | pass |
| `rch exec -- cargo check -p fr-store --all-targets` | pass |
| `rch exec -- cargo test -p fr-store list -- --nocapture` | pass |
| `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings` | pass |
| `rch exec -- cargo test -p fr-conformance -- --nocapture` | pass |
| `list_differ_seed65065.txt` | pass, 500 live random list operations |
| `list_quicklist_dump_differ.txt` | pass, multi-node quicklist DUMP/RDB-save byte-exact |

Binary hashes:

```text
f87a7115fd0496112fd261eabd6b06864867543611cc450bc2f13e1e3a00fd71  control-frankenredis
e72478495c275aa35c45332a5c9f1d3bab5d055694eea71212e8bc68e8d7540e  candidate-frankenredis
```
