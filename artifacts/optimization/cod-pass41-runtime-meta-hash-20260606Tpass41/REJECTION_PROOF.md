# Pass41 Rejection Proof: runtime client-tracking observed-key foldhash

Target bead: `frankenredis-ohsk5`

Profile target: post-pass40 SET P16 profile showed `std::hash::RandomState::hash_one::<&[u8]>` at 5.56%, `queue_client_tracking_invalidations` at 1.53%, and client-tracking observed-key `remove_entry` at 1.19%.

Lever tested: change only `ServerState::client_tracking_observed_keys` from the default `HashMap<Vec<u8>, HashSet<u64>>` SipHash state to `foldhash::quality::RandomState`. The main `Store.entries` keyspace dict was already foldhash and was not changed.

Baseline:

```text
SET P16/300k hyperfine: 0.88115280714s +/- 0.06626889410510739
```

Candidate:

```text
SET P16/300k hyperfine: 1.01559100538s +/- 0.04993041158916154
```

Decision:

```text
Rejected. Baseline was 1.15x faster than candidate, so Score < 2.0.
No production source or dependency change retained.
```

Behavior proof:

```text
Baseline/candidate client-tracking golden sha256:
c47481b2663830752cc8e2a746f7199951122de1e58041b97a9177fff6f72a28

Candidate vs Redis 7.2.4 client-tracking oracle:
PASS - fr CLIENT TRACKING matches redis 7.2.4 across 18 scenarios
a59be9a7d92caf4f8f5cc33aad9a5f0f9703365eee381f11da9a7c4c24065088
```

Isomorphism notes:

- Ordering: non-BCAST invalidations still iterate command keys in command-key order; BCAST batching still uses the existing `BTreeMap<u64, Vec<Vec<u8>>>` target order and per-command key iteration.
- Tie-breaking: no ranked, sorted, random, or floating-point behavior touched.
- RNG: no Redis-visible RNG state touched. Hash seeds affect only the internal observed-key table; no table iteration drives an observable reply.
- Side effects: observed-key insert/remove/clear/retain semantics are unchanged and were checked by the golden tracking matrix and Redis oracle probe.

Next primitive:

If continuing this bead, do not repeat runtime metadata-map hasher swaps. Attack the command-dispatch metadata packet primitive instead: compute one command classification/metadata record once in runtime and thread it through admission, stats, propagation, and `dispatch_argv`.
