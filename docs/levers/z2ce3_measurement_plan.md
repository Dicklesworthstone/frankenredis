# z2ce3 — the measurement to run the moment the freeze lifts

Written under the freeze so no decision is made with a warm result in hand. Every choice below is a
consequence of something that went wrong today; the reasons are stated so they can be argued with
before the run rather than after.

## Bench and arms

**HARNESS: `scripts/shape_instr_per_op.py`. NOT `scripts/balanced_square_ab.py`.**

Not a preference. Today `balanced_square_ab.py` ran its cross-process A/A at loadavg 51 and returned
`get_control` — whose true value is 1.0000 by construction — at **1.0233 with nulls spanning
0.9886/1.0696**, 0 of 6 rows admissible. `shape_instr_per_op.py` on the same ELF, same host, same
hour, held a **0.54 pct spread across three runs**. Instruction counts are load-immune; throughput is
not, and they do not degrade together. Banked as the INADMISSIBLE row.

**ARMS: one source tree, selected by a cargo feature — `perf-ab-unborrowed-keys`.**

The fast arm is HEAD. The slow arm restores the unconditional copy, gated by that feature, exactly as
`perf-ab-rdb-hash-owned` restores the pre-lever decode. Two binaries from two trees is not an A/B; it
is a comparison of two builds that also differ by the lever, and the ledger already records
cross-worker codegen drift of 8-18.5 pct that nobody has controlled for.

**THE SLOW ARM MUST BE ASSERTED WORSE.** The census must fail on the slow arm at `>= 2.0`
allocations/op before the fast arm's `< 1.0` is believed. Three times today a measurement measured
nothing because the thing under test was present in both arms — an A/A null where both arms were the
same ELF, a `sinter_9` shape whose intersection was 3 members, and `sinter_borrow_scan_reports_ab`
where both arms sort. **An A/B can only measure what differs between its arms.**

## Shapes, in the order they answer questions

| shape | why | expected |
|---|---|---|
| `del_1_missing` | the measured worst ratio, 0.7685x, 12.2 pct dispatch — the DEL/UNLINK half | primary |
| `touch_missing` | closure-only route, store already takes borrows — the cheap half | primary |
| `exists_missing` | closure-only, does all its work on borrows before copying | primary |
| `get_control` | control: touches none of this, must not move | must be flat |

`touch_missing` and `exists_missing` are already harness shapes, which is why they go first: no
harness work, and they isolate the closure-only fix from the store-signature fix.

## Schedule

1. **Allocation census first**, both arms, before any profiling. It is deterministic, load-immune,
   and answers "did the lever fire" with an integer. If the slow arm does not read `>= 2.0`, STOP —
   the instrument is blind and no ratio from it means anything.
2. **Then callgrind** via `shape_instr_per_op.py`, three runs per shape, both arms.
3. **Then `get_control`**, same session. If it moves, the session is void.

## A/A null

**For the census:** the null is the slow arm. Both arms run the same test binary against the same
keyspace; only the feature differs. A slow arm reading `< 2.0` means the census cannot see the
allocations at all.

**For the callgrind ratio:** `get_control` is the A/A. It shares dispatch and reply encoding with the
shapes under test and touches none of the changed code, so it must reproduce across runs. Today it
held 0.4181x / 0.4254x — a 1.7 pct spread — which is the bound to hold the run to.

**Monotonicity is a per-run gate, not a formality:** `Ir(2N) > Ir(N)` on BOTH arms of every run. The
SORT_RO rows were retracted because two of four runs violated it and the surviving two clustered
tightly enough to look trustworthy. **A tight spread across adjacent runs is not evidence a
measurement is sound.**

## How the result gets read, decided in advance

The saving is **two allocations per op** (DEL) and **one Vec plus one per key** (the five). With
mimalloc that may be a few percent, not a multiple.

**A 3 pct result is not a failure and must not be filed REJECT.** `del_1_missing`'s narrow 0.7685x
margin was attributed to DEL-on-missing being nearly pure fixed overhead, with redis retiring only
~2650 instr/op. Two allocations are exactly the fixed overhead that attribution predicts should
matter there. So:

- **saving materially above the noise** — the attribution is confirmed and the other six routes follow
- **saving near 3 pct** — the remaining fixed overhead is somewhere else, which is worth as much as a
  win because it tells the next person where not to look

**And the noise floor cuts the other way from the usual assumption.** On `del_1_missing` the redis arm
swung **2.78 pct** run-to-run while fr's reproduced to **0.01 pct**. A 3 pct saving is within a hair
of incumbent-side noise on that shape, so one pair will not call it. **The census count is the
trustworthy half of this measurement; the callgrind ratio is the noisy half.**

## Provenance, non-negotiable

Build, copy the binary to a private path, sha THE COPY. `target/release/frankenredis` is a shared
rendezvous — an 87 pct INCRBYFLOAT discrepancy was traced to measuring a binary of unverified
provenance out of it. Record the ELF sha, the tree, and whether peers' uncommitted WIP is compiled in.

## Landing order

The census lands in the SAME COMMIT as the code it pins. **Cargo compiles every `.rs` under
`tests/` regardless of gitignore**, so an uncompiled census committed ahead of the code breaks every
pane on the first build anyone runs — worse than an uncompiled `src/` file, which only breaks panes
that touch it. That is why the code in this directory is `.rs.txt` and not `.rs`.
