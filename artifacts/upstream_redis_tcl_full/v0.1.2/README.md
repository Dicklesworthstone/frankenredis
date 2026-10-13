# Upstream Redis 7.2.4 Tcl receipts for v0.1.2

Runs of the unmodified Redis 7.2.4 test suite (commit d2c8a4b9) against FrankenRedis in
external-server mode, recorded for the v0.1.2 release. `provenance.json` lists the binaries
(debug builds), their hashes, the host and the exact invocations.

| Receipt | What | Result |
|---|---|---|
| `full-report.json`, `full-runtest.log` | complete default suite, no tags | 13 of 89 units, 623 assertions pass, 0 fail; stops on a harness exception in `unit/type/set` (a slow test needs the server pid) |
| `noslow-tags-slow-report.json`, `noslow-tags-slow-runtest.log` | `--tags -slow`, as upstream's own external-server CI runs | 21 of 89 units, 1,271 pass, 16 fail; stops in `unit/multi` (READONLY: the server is still a replica after "MULTI / EXEC with REPLICAOF") |
| `unit-by-unit-vs-v0.1.1.log` | six units run alone, v0.1.1 and the candidate | no assertion that passes on v0.1.1 fails on the candidate |
| `fidelity-report.json`, `fidelity.log` | curated lane (`scripts/upstream_redis_tcl_fidelity.py`) | 72 of 72 selected assertions pass |

The complete suite is not green; see issue #3. The v0.1.1 receipts are in the parent directory.
