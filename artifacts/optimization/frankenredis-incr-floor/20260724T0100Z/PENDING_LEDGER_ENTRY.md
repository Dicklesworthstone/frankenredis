PARKED for docs/NEGATIVE_EVIDENCE.md (CreamPeak holds the reservation as of 2026-07-24).
Fold this block in at the top (after the intro paragraph, before the newest entry) once the hold lapses.

## 2026-07-24: SHIPPED — dispatch-floor front gate for INCR key; 1.78x fewer instructions on P16 (frankenredis-incr-floor)

INCR's `process_buffered_frames` was its TOP frame at 8.30% self (P16 profile on HEAD) — the largest
unfloored ubiquitous WRITE. 4-byte classifier arm, arity 2 ONLY: INCRBY (3-arg) and all other forms fall
through unchanged. FastReply via `execute_plain_incr_borrowed(key, ts)`, which enforces the same write
gate / AOF / replication / keyspace-notify path as the generic route and returns None (byte-exact
fallback) on any type/overflow/gate failure. Parser+executor already shipped; only the dispatch-floor
routing is new. This is the write-command analogue of the shipped SETBIT/SADD floors.

MEASURED (commandstats-normalized instructions:u; hash-bracketed ctl 6dc4c8b4 / cand 3f2218d6, peer WIP
fr-runtime/lib.rs stable across both builds; -P16, perf -- sleep 6, INCR calls-delta, interleaved 6 reps):
  INCR       ctl 3508.46 -> cand 1966.80 instr/op = 0.5606x (1.784x fewer, 43.9%); null/ctl 1.0022,
             ctl cv 2.19% / cand cv 1.43%
  GET guard  ratio cand/ctl median 0.983 (neutral; GET untouched)

BYTE-IDENTICAL vs redis 7.2.4 (12 cases): fresh->1/2/3, non-int-string(err), ->i64max, overflow(err),
leading-space(err), float-string(err), wrong-arity(err), INCRBY-fallback(105), hash-wrongtype(WRONGTYPE),
empty-string-val(err) = 12/12 MATCH. RESP3 = MATCH. Keyspace notifications under floored path (KEA:
__keyspace@0__:nk incrby / __keyevent@0__:nk) byte-identical to redis — write gate intact. Gates: full
fr-server test binary compiles+links clean; floor unit test PASS incl INCR assertion. fr-conformance NOT
re-run (rch under sustained critical worker pressure, 14+ no-slot retries) — change confined to fr-server
socket dispatch, does not touch the executor/runtime conformance exercises; the live 12-case + RESP3 +
keyspace-notif differential is the direct gate for a dispatch-floor lever.
Artifacts: artifacts/optimization/frankenredis-incr-floor/20260724T0100Z/.
