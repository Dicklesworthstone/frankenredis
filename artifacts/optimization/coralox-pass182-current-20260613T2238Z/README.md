# frankenredis-vh4k9 pass182 rejected lever

Bead: `frankenredis-vh4k9`
Agent: `CoralOx`
Date: 2026-06-13

## Target

Fresh current-main dashboard selected list writes as the top measured residual:

- RPUSH P16/C50/n400k best-of-3: Redis `1,038,961 req/s`, FrankenRedis `900,900 req/s`, `redis/fr 1.15x`.
- LPUSH P16/C50/n400k best-of-3: Redis `1,017,811 req/s`, FrankenRedis `925,925 req/s`, `redis/fr 1.10x`.

`perf_event_paranoid=4` blocked kernel perf capture. FR RPUSH strace showed `sendto` 31,251 calls, `recvfrom` 31,302 calls, and `epoll_wait` 11,690 calls. Redis had comparable write-call counts, so this pass did not pursue syscall batching.

## Candidate Lever

Added borrowed `ListValue::push_back_slice` / `push_front_slice` helpers and routed `Store::rpush/lpush` through them so packed-list writes copy directly from the command argument slice instead of allocating a temporary `Vec<u8>` first.

Mapped primitive: data-plane allocation/copy reduction from the graveyard zero-copy/arena/slab family.

## Isomorphism Proof

- Ordering preserved: yes. The same packed/deque append and prepend methods decide list element order.
- Tie-breaking unchanged: yes. Lists have no tie-break ordering beyond command order, which is unchanged.
- Error precedence unchanged: yes. Type check and expiry/drop paths were outside the helper replacement.
- CLIENT REPLY state unchanged: yes. Golden includes SKIP, OFF, ON, and later replies.
- Floating point: N/A.
- RNG: unchanged; the LFU random sample path is outside the helper replacement.
- Golden output: baseline and candidate raw RESP outputs matched byte-for-byte.

Golden SHA256:

```text
f283a2d967d530865d70f93327347303d6646093a359b95cbb27c833c2fcf168  list_push_golden_input.resp
c5b5bb0500ee4ef508b5261dce714d3e91a2d7969a9ba535de1adba44199ab47  list_push_golden_baseline.raw
c5b5bb0500ee4ef508b5261dce714d3e91a2d7969a9ba535de1adba44199ab47  list_push_golden_candidate.raw
```

## Validation

- `cargo fmt --check -p fr-store`
- `rch exec -- cargo check -p fr-store --all-targets`
- `rch exec -- cargo test -p fr-store list_value_borrowed_push_matches_owned_push -- --nocapture`
- `rch exec -- cargo clippy -p fr-store --all-targets -- -D warnings`
- `rch exec -- cargo build --release -p fr-server`

## Benchmark Decision

Pre-change standalone RPUSH P16/C50/n1M baseline:

```text
1.552 s +/- 0.019 s
```

Final paired RPUSH P16/C50/n1M:

```text
baseline:  1.062 s +/- 0.049 s
candidate: 1.093 s +/- 0.037 s
summary:   baseline 1.03x +/- 0.06 faster than candidate
```

Score `< 2.0`; rejected. Source hunk and candidate-only test were removed.

## Next Route

Re-profile current main. If list writes remain the residual, attack a different primitive: command packet/list batch construction, packed-list append layout, or server/runtime request framing. Do not repeat borrowed `ListValue` slice wrappers without a new profile signature.
