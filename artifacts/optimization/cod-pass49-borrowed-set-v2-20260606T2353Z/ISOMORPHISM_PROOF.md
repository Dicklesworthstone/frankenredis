# Isomorphism Proof: Borrowed Exact SET Fast Path Candidate

This proof was captured while the candidate exact-SET fast path was applied.
After rebasing onto parent `5ce3f7231`, the candidate failed the performance
keep gate (`1.01x +/- 0.06` on SET P16/300k and `1.05x +/- 0.09` on SET
P16/1M), so the production source was restored to the parent implementation.

The candidate fast path applies only to strict multibulk `SET key value` frames
and only when all observable side channels that require the generic path are
inactive.

## Preserved Ordering

- One input frame still emits exactly one `+OK\r\n` reply.
- The frame is consumed only after the runtime write succeeds.
- Output-buffer limit handling runs immediately after appending the reply.
- Active expiry still runs before the write, matching the generic command path.
- Lazy expired-key propagation is drained after the write.

## Preserved Visibility

The fast path falls back to generic dispatch for:

- non-client execution sources
- non-DB0 sessions
- transactions, WATCH state, and dirty WATCH state
- paused clients
- CLIENT REPLY off/skip/suppression
- client no-touch mode
- client tracking and subscription mode
- maxmemory enforcement
- disk write denial and min-replica write policy
- keyspace notifications and pending notification state
- monitor clients/output
- blocked clients and ready keys
- pub/sub state
- replica role/state, AOF, replica stream application, and replica reconfigure
- cluster ASKING/non-read-write mode
- authentication-required or non-unrestricted ACL users

## Preserved Byte Outputs

Golden raw TCP transcript:

- input sha256:
  `e7a11a6135058dd81b9593b9002c5d93469ed8d1f26b1838fcb165749c5d0f04`
- baseline output sha256:
  `5c82044b4b0062c0db526300576dcf15087e4d8c64f07c6fc01965df18100508`
- candidate output sha256:
  `5c82044b4b0062c0db526300576dcf15087e4d8c64f07c6fc01965df18100508`
- baseline output bytes: 33
- candidate output bytes: 33
- `cmp -s` passed

## Preserved Tie-Breaking, Floating-Point, and RNG

`SET key value` does not define ordering tie-breaks, floating-point arithmetic,
or RNG side effects. The fast path only mutates the same key/value store entry
through `Store::set`, so those dimensions remain untouched.

## Preserved State Accounting

The focused runtime parity test compares the borrowed fast path against generic
dispatch for:

- stored value
- total commands processed
- total writes processed
- dirty counter
- last command name
- last argv length sum
- command histogram counters
- mixed-case slowlog argv preservation
