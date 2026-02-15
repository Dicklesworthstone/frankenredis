# FR-P2C-001 Legacy Anchor Map

Packet: `FR-P2C-001`  
Subsystem: event loop core  
Target crate: `crates/fr-eventloop`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact captures Redis event-loop anchors with line-level references and
extracts behavior contracts needed for strict/hardened parity work in
FrankenRedis.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-001-A01` | `legacy_redis_code/redis/src/ae.h:21-35`, `legacy_redis_code/redis/src/ae.h:52-93` | Event-loop ABI: file/time event masks, `AE_BARRIER`, before/after sleep hooks, and `aeEventLoop` state fields are the contract surface. | Partial: simplified tick model exists; hook/mask semantics not implemented. |
| `FR-P2C-001-A02` | `legacy_redis_code/redis/src/ae.c:47-72` | Loop bootstrap must initialize monotonic clock, descriptor arrays, callbacks, and neutral masks (`AE_NONE`). | Partial in `crates/fr-eventloop/src/lib.rs:3-36` (budget/tick only). |
| `FR-P2C-001-A03` | `legacy_redis_code/redis/src/ae.c:145-179` | File-event registration is bounded by `setsize`, dynamically resizes event arrays, and updates `maxfd`. | Missing. |
| `FR-P2C-001-A04` | `legacy_redis_code/redis/src/ae.c:181-199` | File-event removal must clear `AE_BARRIER` with writable removal and recompute `maxfd` when needed. | Missing. |
| `FR-P2C-001-A05` | `legacy_redis_code/redis/src/ae.c:263-343` | Timer path is unsorted scan (`O(N)`), supports logical delete (`AE_DELETED_EVENT_ID`), refcount-safe callback recursion, and reschedule-or-delete result. | Missing. |
| `FR-P2C-001-A06` | `legacy_redis_code/redis/src/ae.c:360-468` | Core dispatch ordering contract: `beforesleep` -> poll -> `aftersleep` -> file events -> time events. `AE_BARRIER` can invert R/W callback order. | Missing in runtime integration. |
| `FR-P2C-001-A07` | `legacy_redis_code/redis/src/ae.c:492-510` | Main loop runs `AE_ALL_EVENTS | AE_CALL_BEFORE_SLEEP | AE_CALL_AFTER_SLEEP` each cycle and exposes callback setters. | Missing. |
| `FR-P2C-001-A08` | `legacy_redis_code/redis/src/server.c:3051-3070` | Server wires 1ms `serverCron`, then installs `beforeSleep` and `afterSleep` hooks before persistence loading. | Missing integration in `fr-runtime`. |
| `FR-P2C-001-A09` | `legacy_redis_code/redis/src/server.c:1857-2051` | `beforeSleep` is a critical pipeline: pending transport data, cluster hooks, blocked clients, fast expire, AOF flush, pending writes, IO-thread handoff, async free, backlog trim, eviction, `aeSetDontWait`. | Missing; only abstract tick budgeting exists. |
| `FR-P2C-001-A10` | `legacy_redis_code/redis/src/server.c:2053-2090` | `afterSleep` reacquires module GIL, marks loop start/cmd counters, toggles running flag, updates cached time. | Missing. |
| `FR-P2C-001-A11` | `legacy_redis_code/redis/src/server.c:2708-2718` | Accept-handler registration over listener fds is atomic with rollback on first failure. | Missing. |
| `FR-P2C-001-A12` | `legacy_redis_code/redis/src/networking.c:121-133` | Client creation on accepted transport sets read handler (`readQueryFromClient`) immediately and associates conn private data. | Missing in Rust runtime/eventloop integration. |
| `FR-P2C-001-A13` | `legacy_redis_code/redis/src/networking.c:1594-1667` | Accept path performs admission control (`maxclients`), creates client, then transport-level `connAccept` continuation. | Missing. |
| `FR-P2C-001-A14` | `legacy_redis_code/redis/src/networking.c:2773-2819` | Pending-write flush attempts synchronous writes before arming writable handlers, with IO-thread delegation rules. | Missing. |
| `FR-P2C-001-A15` | `legacy_redis_code/redis/src/networking.c:3504-3688` | Input path contract: lookahead parsing, pending command queue, main-thread-only execution, trim query buffer semantics for master/non-master. | Missing for full parity. |
| `FR-P2C-001-A16` | `legacy_redis_code/redis/src/networking.c:3690-3844` | Read path manages reusable query buffers, BIG_ARG optimization, client querybuf limits, fatal-read handling, and post-read processing. | Missing. |
| `FR-P2C-001-A17` | `legacy_redis_code/redis/src/networking.c:5395-5435` | `processEventsWhileBlocked` re-enters event loop in bounded non-blocking iterations and sets `ProcessingEventsWhileBlocked` guard semantics. | Missing. |
| `FR-P2C-001-A18` | `legacy_redis_code/redis/src/server.c:8025` | Server runtime entry point is `aeMain(server.el)` after initialization and listener bring-up. | Missing runtime/server binary path. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-001-B01` | Normal | Main loop iteration with pending sockets + timers | Execute before-sleep hooks, poll, after-sleep hooks, file callbacks, then timer callbacks. | `FR-P2C-001-U001` | `FR-P2C-001-E001` | `eventloop.dispatch.order_mismatch` |
| `FR-P2C-001-B02` | Edge | Writable event registered with `AE_BARRIER` | Writable callback must run before readable callback in same cycle. | `FR-P2C-001-U002` | `FR-P2C-001-E002` | `eventloop.ae_barrier_violation` |
| `FR-P2C-001-B03` | Edge | Pending TLS/IO/acks demand no sleep | Loop must set dont-wait behavior and immediately run next cycle. | `FR-P2C-001-U003` | `FR-P2C-001-E003` | `eventloop.dont_wait_not_set` |
| `FR-P2C-001-B04` | Edge | FD registration crosses current event array capacity | Event arrays grow safely and `maxfd` reflects new highest active fd. | `FR-P2C-001-U004` | `FR-P2C-001-E004` | `eventloop.fd_resize_failure` |
| `FR-P2C-001-B05` | Edge | `processEventsWhileBlocked` during loading/busy state | Only bounded subset work proceeds; loop exits early when no progress. | `FR-P2C-001-U005` | `FR-P2C-001-E005` | `eventloop.blocked_mode_progress_stall` |
| `FR-P2C-001-B06` | Adversarial | Connection count reaches `maxclients` | Accept path rejects with error payload and closes connection without partial initialization. | `FR-P2C-001-U006` | `FR-P2C-001-E006` | `eventloop.accept.maxclients_reached` |
| `FR-P2C-001-B07` | Adversarial | Query buffer exceeds configured limits | Client is closed asynchronously and disconnection stats increment. | `FR-P2C-001-U007` | `FR-P2C-001-E007` | `eventloop.read.querybuf_limit_exceeded` |
| `FR-P2C-001-B08` | Adversarial | Fatal transport read error/disconnect | Client read error path must trigger closure and skip command execution. | `FR-P2C-001-U008` | `FR-P2C-001-E008` | `eventloop.read.fatal_error_disconnect` |

## Test + log traceability contract (for downstream beads)

For each `FR-P2C-001-U*` and `FR-P2C-001-E*` case, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (must be `FR-P2C-001`)
- `mode` (`strict` or `hardened`)
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

Suggested suite IDs:

- Unit/property: `fr_eventloop_phase2c_packet_001`
- E2E/integration: `fr_runtime_phase2c_packet_001`

## Sequencing boundary notes

- This artifact intentionally focuses on extraction/behavior mapping only.
- Contract-table formalization and strict/hardened invariants are deferred to `bd-2wb.12.2`.
- Fixture schema packing/parity gate/risk note artifacts are deferred to `bd-2wb.12.6` and `bd-2wb.12.7`.

## Confidence notes

- Confidence is high for `ae.c`/`ae.h` anchors (direct line-level extraction).
- Confidence is medium-high for networking/server integration anchors due cross-file coupling (`server.c`, `networking.c`).
- No inference here changes legacy semantics; all behavioral statements are source-anchored.
