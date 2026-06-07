# Pass 65 Hot-Key Read Certificate Rejection

## Target

- Bead: `frankenredis-l67mp`
- Baseline source: `50db9c4ff` / `c233eb801` have the same GET hot path as
  the pass64 GET profile; intervening commits are evidence and bead-ledger
  changes only for this path.
- Profile-backed hotspot: GET P16/1M showed `__memmove_avx_unaligned_erms`
  at 9.56%, `Value::string_owned` at 8.83%, `process_buffered_frames` at
  1.90%, `parse_command_args_borrowed_into` at 1.62%, and foldhash/hash lookup
  cost still visible after direct-GET and output-buffer cursor variants failed
  the keep gate.

## Candidate Primitive

Alien-graveyard mapping: hot-key caches, pointer-swizzling style stable handles,
and epoch/fingerprint validation can turn repeated key lookups into a cheap
certificate when the command is a pure read of cached bytes.

The `fr-bench` GET workload has enough key locality to make this worth checking:

- 1,000,000 requests, 16 clients, 10,000-key LCG keyspace.
- Ideal fully-associative hit rate after warmup: `0.990000`.
- Direct-mapped 16,384-slot certificate table: `0.990000`.
- 8,192 sets x 2 ways: `0.990000`.

The full simulation output is in
`key_locality_simulation.txt`.

## Isomorphism Check

A one-lever request-path hot-key certificate does not preserve current Redis
observable semantics.

`Store::get` is not just a byte read:

- `Store::record_keyspace_lookup` first performs lazy expiry through
  `drop_if_expired`, then updates keyspace hits/misses.
- LFU mode consumes `Store::next_rand()` before the entry access.
- The entry is reached with `entries.get_mut(key)`, then string bytes are
  materialized.
- LFU metadata may be probabilistically updated through `Entry::bump_lfu_freq`.
- LRU/idle metadata is updated through `Entry::touch`.

The affected source contract is visible in `crates/fr-store/src/lib.rs`:

- `record_keyspace_lookup`: lines 3682-3692.
- `Entry::touch` / `Entry::bump_lfu_freq`: lines 1947-1998.
- `Store::get`: lines 4056-4077.
- `OBJECT IDLETIME`, `DEBUG OBJECT` LRU clock, and `OBJECT FREQ` expose that
  same metadata: lines 5567-5648.

Therefore a cache hit that returns only cached bytes would preserve reply bytes
but change at least one observable side effect under valid command sequences:
keyspace stats, lazy-expiry propagation, LRU idle time, LFU frequency, or RNG
sequence under LFU policy. A cache hit that preserves those effects must still
mutate the exact entry or an exact sidecar access record. In the current store,
that means performing the same `entries.get_mut(key)` lookup the certificate is
supposed to avoid.

## Golden Output

No production source patch was produced. The correct output-preserving action is
to keep source unchanged, so the pass64 golden RESP SHA remains the last
applicable byte-level proof for the GET path:

- output-cursor proof: `2612d02989f4a06e17bf0f2f06c69dfe9bc475051f1674481e85347a3f44e688`
- inline-bulk proof: `77b1fb0a092c82d445d128c3571e82e83717ce2b6e5f152b5944594179160c56`

## Decision

Reject before source edit.

- Impact if redesigned around stable entry ids: 3
- Confidence for a one-lever request-path certificate: 0.3
- Effort for a correct side-effect-preserving cache: 3
- Score: `0.3 = 3 x 0.3 / 3`

Next route: do not retry direct GET encode, inline-bulk, output-cursor, or
request-path hot-key certificate variants. The deeper primitive is a stable
entry table plus sidecar access metadata with generation/expiry validation, so
hot reads can update LRU/LFU/keyspace/RNG-visible state without rehashing the
key bytes on every command.
