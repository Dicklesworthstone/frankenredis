# FR-P2C-008 Legacy Anchor Map

Packet: `FR-P2C-008`  
Subsystem: expiration + eviction  
Target crates: `crates/fr-expire`, `crates/fr-store`, `crates/fr-command`, `crates/fr-runtime`, `crates/fr-conformance`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored expiration/eviction contracts from legacy
Redis and maps them to current FrankenRedis coverage, including active/lazy
expiration behavior, `EXPIRE*` command semantics, maxmemory eviction policy
execution, and deterministic failure reason codes for downstream validation.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-008-A01` | `legacy_redis_code/redis/src/expire.c:40-58` | `activeExpireCycleTryExpire` deletes logically expired keys and propagates deterministic delete side effects/stat updates. | Missing dedicated active-expire deletion/propagation executor. |
| `FR-P2C-008-A02` | `legacy_redis_code/redis/src/expire.c:287-470` | `activeExpireCycle` runs adaptive slow/fast expiry loops with effort tuning, DB iteration fairness, and strict time budgets. | Missing active-expire scheduler and budgeted sweeps. |
| `FR-P2C-008-A03` | `legacy_redis_code/redis/src/expire.c:157-281` | Subexpiry (hash-field expiry) cycle performs bounded field-level expiration and rotation across subexpires buckets. | Missing hash-field expiry active cycle. |
| `FR-P2C-008-A04` | `legacy_redis_code/redis/src/expire.c:548-645` | Writable-replica expire tracking (`expireSlaveKeys`, `rememberSlaveKeyWithExpire`) prevents unsafe local expiry drift and manages key tracking state. | Missing writable-replica expire tracker. |
| `FR-P2C-008-A05` | `legacy_redis_code/redis/src/expire.c:652-714` | `checkAlreadyExpired` and extended option parser enforce `NX/XX/GT/LT` compatibility rules and past-time behavior gates. | Missing full option matrix + past-expiry command gate. |
| `FR-P2C-008-A06` | `legacy_redis_code/redis/src/expire.c:726-843` | `expireGenericCommand` handles relative/absolute expiry, overflow checks, option predicates, immediate delete rewrite, and `PEXPIREAT` propagation normalization. | Missing full Redis `EXPIRE*` semantic matrix and rewrite semantics. |
| `FR-P2C-008-A07` | `legacy_redis_code/redis/src/expire.c:865-905` | `TTL`/`PTTL`/`EXPIRETIME`/`PEXPIRETIME` return semantics preserve `-2` missing, `-1` persistent, and absolute-vs-relative output contracts. | Partially covered (`TTL`/`PTTL` only). |
| `FR-P2C-008-A08` | `legacy_redis_code/redis/src/expire.c:910-924` | `PERSIST` removes TTL metadata and emits deterministic side effects/notifications. | Partially covered (basic TTL clear only). |
| `FR-P2C-008-A09` | `legacy_redis_code/redis/src/db.c:2600-2683` | Core expires API (`removeExpire`, `setExpire`, `getExpire`) maintains per-key expiry metadata with dict/link-level invariants. | Partial TTL metadata support; missing full DB expire API parity. |
| `FR-P2C-008-A10` | `legacy_redis_code/redis/src/db.c:2683-2778` | Expired/evicted deletion propagation centralizes DEL/UNLINK emission ordering and stats/latency updates. | Missing centralized expire+evict propagation contract. |
| `FR-P2C-008-A11` | `legacy_redis_code/redis/src/db.c:2850-2912` | `expireIfNeeded` enforces access/deletion gates for replica mode, cluster import/trim, pause state, and forced-delete options. | Missing `expireIfNeeded` policy gate matrix. |
| `FR-P2C-008-A12` | `legacy_redis_code/redis/src/evict.c:55-84` | LRU clock and idle-time estimation define deterministic aging basis for eviction scoring. | Missing LRU/LFU scoring subsystem. |
| `FR-P2C-008-A13` | `legacy_redis_code/redis/src/evict.c:112-228` | Eviction pool sampling/population performs candidate ranking for LRU/LFU/volatile-TTL policies across DB/slot sampling. | Missing eviction candidate pool/ranking engine. |
| `FR-P2C-008-A14` | `legacy_redis_code/redis/src/evict.c:265-311` | LFU time/counter decay and increment logic maintain frequency-based eviction scoring consistency. | Missing LFU decay/increment semantics. |
| `FR-P2C-008-A15` | `legacy_redis_code/redis/src/evict.c:384-434` | Maxmemory state computation excludes non-counted memory and determines required memory-to-free safely. | Missing maxmemory accounting model. |
| `FR-P2C-008-A16` | `legacy_redis_code/redis/src/evict.c:468-505` | Eviction safety gate blocks eviction under unsafe contexts (loading, yielding command, replica ignore, ASM import, paused evict action) and sets time limits by tenacity. | Missing eviction safety gate and tenacity budget logic. |
| `FR-P2C-008-A17` | `legacy_redis_code/redis/src/evict.c:532-760` | `performEvictions` executes policy-specific eviction loops with bounded time slices, lazyfree reconciliation, and async continuation proc. | Missing maxmemory eviction execution loop. |
| `FR-P2C-008-A18` | `legacy_redis_code/redis/src/evict.c:87-91`, `legacy_redis_code/redis/src/evict.c:468-485` | Cluster migration/import constraints explicitly suppress unsafe eviction sampling/deletion paths during slot migration operations. | Missing cluster-aware eviction suppression hooks. |
| `FR-P2C-008-A19` | `legacy_redis_code/redis/src/server.c:1247-1258` | `databasesCron` triggers slow active-expire cycles on masters and replica-specific expire handling. | Missing runtime cron-driven active-expire orchestration. |
| `FR-P2C-008-A20` | `legacy_redis_code/redis/src/server.c:1910-1916` | `beforeSleep` triggers fast active-expire cycle with strict low-latency budget behavior. | Missing before-sleep fast-expire integration. |
| `FR-P2C-008-A21` | `legacy_redis_code/redis/src/server.c:2967-2967` | Startup path initializes eviction pool state (`evictionPoolAlloc`) prior to command traffic. | Missing maxmemory eviction bootstrap init. |
| `FR-P2C-008-A22` | `legacy_redis_code/redis/src/server.c:4478-4490` | Command path invokes `performEvictions` pre-execution and handles OOM/eviction side effects deterministically. | Missing command-path maxmemory enforcement gate. |
| `FR-P2C-008-A23` | `legacy_redis_code/redis/src/db.c:2769-2794` | `propagateDeletion` guarantees ordering-safe replica/AOF propagation for implicit expire/evict deletions. | Missing centralized deletion propagation ordering contract. |
| `FR-P2C-008-A24` | `crates/fr-expire/src/lib.rs:3-34` | Rust `fr-expire` currently provides only a minimal `evaluate_expiry` helper (`remaining_ms` + `should_evict`). | Present as minimal expiry predicate only. |
| `FR-P2C-008-A25` | `crates/fr-store/src/lib.rs:95-130`, `crates/fr-store/src/lib.rs:290-297`, `crates/fr-command/src/lib.rs:71-107`, `crates/fr-command/src/lib.rs:252-381`, `crates/fr-runtime/src/lib.rs:98-132` | Rust baseline supports basic per-key TTL checks and `EXPIRE`/`PTTL`/`TTL`, but has no active-expire cycle, maxmemory eviction engine, policy scoring, or propagation parity paths. | Present as partial TTL semantics; eviction absent. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-008-B01` | Normal | Active expire cycle encounters expired key | Expired key is deleted and propagated with deterministic stats/notifications. | `FR-P2C-008-U001` | `FR-P2C-008-E001` | `expire.active_cycle_contract_violation` |
| `FR-P2C-008-B02` | Normal | Slow/fast expire cycles run under effort/time limits | Cycle cadence and time-budget behavior remain deterministic and bounded. | `FR-P2C-008-U002` | `FR-P2C-008-E002` | `expire.fast_slow_budget_violation` |
| `FR-P2C-008-B03` | Normal | `EXPIRE*` command with `NX/XX/GT/LT` options | Option compatibility and predicate semantics match legacy matrix. | `FR-P2C-008-U003` | `FR-P2C-008-E003` | `expire.option_parse_contract_violation` |
| `FR-P2C-008-B04` | Normal | `EXPIRE*` resolves to past timestamp | Immediate delete path rewrites/propagates explicit `DEL/UNLINK` deterministically. | `FR-P2C-008-U004` | `FR-P2C-008-E004` | `expire.immediate_delete_rewrite_violation` |
| `FR-P2C-008-B05` | Normal | `TTL/PTTL/EXPIRETIME/PEXPIRETIME` query | Return values (`-2`, `-1`, relative/absolute) remain contract-compatible. | `FR-P2C-008-U005` | `FR-P2C-008-E005` | `expire.ttl_observable_contract_violation` |
| `FR-P2C-008-B06` | Normal | `PERSIST` on volatile key | TTL metadata is removed with deterministic side effects. | `FR-P2C-008-U006` | `FR-P2C-008-E006` | `expire.persist_contract_violation` |
| `FR-P2C-008-B07` | Edge | Lookup on logically expired key under mode/flag constraints | `expireIfNeeded` gate semantics honor replica/import/pause/force-delete envelopes. | `FR-P2C-008-U007` | `FR-P2C-008-E007` | `expire.lookup_guard_contract_violation` |
| `FR-P2C-008-B08` | Normal | Maxmemory state evaluated under memory pressure | Memory accounting excludes not-counted overhead and computes to-free targets correctly. | `FR-P2C-008-U008` | `FR-P2C-008-E008` | `evict.maxmemory_state_contract_violation` |
| `FR-P2C-008-B09` | Normal | Eviction candidate selection under policy | LRU/LFU/TTL/random policy candidate ranking/sampling is deterministic within policy envelope. | `FR-P2C-008-U009` | `FR-P2C-008-E009` | `evict.policy_candidate_selection_violation` |
| `FR-P2C-008-B10` | Adversarial | Over-maxmemory command execution path | `performEvictions` runs bounded loops and transitions to async continuation/fail states deterministically. | `FR-P2C-008-U010` | `FR-P2C-008-E010` | `evict.eviction_loop_contract_violation` |
| `FR-P2C-008-B11` | Adversarial | Unsafe context for eviction (replica ignore, ASM import, paused evict) | Eviction is skipped deterministically without unsafe side effects. | `FR-P2C-008-U011` | `FR-P2C-008-E011` | `evict.safety_gate_contract_violation` |
| `FR-P2C-008-B12` | Adversarial | Hardened mode receives non-allowlisted expire/evict deviation | Non-allowlisted behavior is rejected and packet stays strict-equivalent fail-closed. | `FR-P2C-008-U012` | `FR-P2C-008-E012` | `expireevict.hardened_nonallowlisted_rejected` |

## High-risk traceability and structured-log contract

For all `FR-P2C-008-U*` and `FR-P2C-008-E*` rows, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-008`)
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

- Unit/property: `fr_expire_evict_phase2c_packet_008`
- E2E/integration: `fr_runtime_phase2c_packet_008`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-008-CLAIM-01` |
| `evidence_id` | `FR-P2C-008-EVID-LEGACY-EXP-EVICT-001` |
| Hotspot evidence | `A02`, `A17`, `A22` (active-expire scheduler, eviction loop, command-path maxmemory gate) |
| Mapped graveyard section IDs | `AG-DET-04` (deterministic state reduction), `AG-PERF-08` (budgeted latency control), `AG-SEC-11` (fail-closed policy) |
| Baseline comparator | Legacy Redis expire/evict path (`expire.c`, `evict.c`, `db.c`, `server.c`) |
| EV score | `2.9` |
| Priority tier | `S` |
| Adoption wedge | Implement active-expire + EXPIRE* semantics first, then maxmemory eviction engine and safety gates |
| Budgeted mode defaults | Strict: `FailClosed`; Hardened: bounded defenses only on allowlist |
| Deterministic exhaustion behavior | On hardened budget exhaustion, force strict-equivalent fail-closed and emit `expireevict.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008` |

## Expected-loss decision model

States:

- `S0`: contract-preserving expire/evict behavior
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe expiration/eviction divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and reject/abort transition

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` rises with bounded parser/metadata anomalies without deletion-order drift.
- `P(S2|e)` rises with TTL/expire gate mismatches, eviction policy drift, or unsafe over-maxmemory handling.

Calibration and fallback policy:

- Calibration metric: false-negative rate on adversarial expire/evict suite `< 1%`.
- Fallback trigger: calibration breach in two consecutive windows or `P(S2|e) >= 0.30`.
- Trigger behavior: disable hardened deviations for packet scope and enforce strict fail-closed.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever (downstream execution target):

- `LEV-008-01`: deterministic TTL+eviction decision cache keyed by `(key_digest, now_bucket, policy_epoch, memory_pressure_bucket)` with strict invalidation on expiry mutation/policy change.

Required loop artifacts and paths:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-008/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-008/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-008/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-008/isomorphism_report.md`

Replay commands (strict/hardened):

- `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED`

## Reproducibility and provenance pack references

Required pack (to be produced and linked by downstream packet beads):

- `artifacts/phase2c/FR-P2C-008/env.json`
- `artifacts/phase2c/FR-P2C-008/manifest.json`
- `artifacts/phase2c/FR-P2C-008/repro.lock`
- `artifacts/phase2c/FR-P2C-008/LEGAL.md` (mandatory if IP/provenance risk is detected)

## Confidence notes

- High confidence for active-expire and maxmemory-eviction anchor extraction from `expire.c`, `evict.c`, `db.c`, and `server.c`.
- Medium confidence for hash-field expiry and ASM-related suppression scope pending packet-level narrowing.
- High confidence that current Rust coverage is partial for TTL semantics and missing for maxmemory eviction engine.
