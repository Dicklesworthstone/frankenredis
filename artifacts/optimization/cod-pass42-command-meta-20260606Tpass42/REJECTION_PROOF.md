# Pass42 Rejection Proof: command metadata packet

Target bead: `frankenredis-ohsk5`

Profile target: post-pass40 SET P16 profile showed `Runtime::execute_dispatch` at 5.80%, `dispatch_with_client_context`/`dispatch_argv` at 2.32%/1.16%, `canonical_command_fullname` at 1.14%, and repeated command metadata derivation after command-specific micro-levers failed.

Lever tested: add a `fr_command::CommandMetadata` packet, compute it once in runtime, reuse it for processed read/write counters, arity checks, disk/min-replica write gates, maxmemory write checks, post-dispatch read tracking, and pass it into a metadata-aware `dispatch_argv_with_metadata` so dispatch skips its own command classification.

Baseline:

```text
SET P16/300k hyperfine: 0.8133836211200001s +/- 0.0702167983272912
```

Candidate:

```text
SET P16/300k hyperfine: 1.03543921186s +/- 0.1337266108848078
```

Decision:

```text
Rejected. Baseline was 1.27x faster than candidate, so Score < 2.0.
No production source hunk retained.
```

Behavior proof:

```text
Baseline/candidate raw dispatch golden transcript: 458 bytes each
sha256: 1da726a1acca3a2d234a275cb4469552ddf34ed331a85fefea105930abecb02a
cmp_exit: 0
```

Golden coverage:

- Known command dispatch: `PING`, `SET`, `GET`, `HSET`, `HGET`.
- HGET arity: bad-arity `HGET h`.
- Unknown command: `NOPE a b`.
- DB routing: `SELECT 1`, `SELECT 0`, and cross-DB `GET`.
- Transaction queueing: `MULTI`, queued `SET`/`GET`, `EXEC`.
- ACL allow/deny: `ACL SETUSER`, `AUTH`, allowed `GET`, denied `SET`.
- Pub/sub context: `SUBSCRIBE`, subscribed-mode `PING`, `UNSUBSCRIBE`.

Isomorphism notes:

- Ordering: command execution, transaction queue order, selected-db command order, ACL gate order, and pub/sub reply order are unchanged.
- Tie-breaking: no ranked or sorted data path changed.
- Floating point: no floating-point path changed.
- RNG: no RNG state or random command path changed.
- Error text: unknown-command and arity error bytes are included in the golden transcript.

Next primitive:

Do not keep iterating command metadata packets. The candidate moved work around but lost in the benchmark. The next pass should attack a larger parser/dispatch fusion or output batching primitive that removes allocation/copy work as a class, not another metadata-cache micro-line.
