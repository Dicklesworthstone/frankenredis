# FR-P2C-008 Rust Implementation Plan

Packet: `FR-P2C-008`  
Scope: expiration + eviction parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible expiration and eviction behavior for FR-P2C-008 with:

- deterministic `EXPIRE*`/`TTL*`/`PERSIST` command semantics,
- explicit strict/hardened compatibility boundaries,
- fail-closed handling for expiration/eviction safety-gate ambiguity,
- traceable unit/e2e evidence for every `C01..C16` and `T01..T11` row.

## 2) Baseline architecture (as-is evidence)

Current implementation baseline (must be preserved unless a contract row requires extension):

1. `crates/fr-store/src/lib.rs`
   - Owns in-memory key/value + expiry metadata.
   - Current expiry/TTL primitives: `Store::expire_seconds`, `Store::pttl`, `Store::persist`, `Store::drop_if_expired`.
   - Current delete primitives: `Store::del`, `Store::flushdb`.
2. `crates/fr-command/src/lib.rs`
   - Routes RESP commands in `dispatch_argv`.
   - Expiration surface currently delegates through `expire`, `pttl`, `ttl`, `persist`, `del`, `flushdb` handlers.
3. `crates/fr-expire/src/lib.rs`
   - Contains `evaluate_expiry` helper and unit tests only; not yet integrated into runtime/store flow.
4. `crates/fr-runtime/src/lib.rs`
   - Hosts protocol preflight gate (`preflight_gate`) and `EvidenceLedger`.
   - Uses `RuntimePolicy` allowlist decisions, but does not yet enforce maxmemory/eviction gates.
5. `crates/fr-eventloop/src/lib.rs`
   - Contains generic tick budgeting (`run_tick`) with no expire/evict semantics.
6. `crates/fr-config/src/lib.rs`
   - Provides strict/hardened mode policy and allowlist decisions (`RuntimePolicy`, `is_deviation_allowed`).
7. `crates/fr-conformance`
   - Provides fixture harness and packet evidence surface; FR-P2C-008 contract/risk artifacts already exist.

Current gap summary: lazy per-key expiry exists, but active-expire cycles, maxmemory accounting, eviction policy loops, and explicit eviction safety gates are not implemented in Rust yet.

## 3) Module boundary skeleton (target)

### `crates/fr-store` (primary expiration/eviction state seam)

1. `expire_metadata` boundary (initially extracted from `lib.rs`)
   - key TTL metadata mutation/query invariants (`C08`, `C09`)
2. `active_expire_scheduler` boundary
   - slow/fast cycle scheduling + fairness budget enforcement (`C01`, `C02`, `C03`)
3. `maxmemory_accounting` boundary
   - memory-pressure accounting and `to_free` calculations (`C10`)
4. `eviction_policy` boundary
   - candidate sampling/ranking for LRU/LFU/volatile-ttl/random semantics (`C11`)
5. `eviction_loop` boundary
   - bounded `performEvictions` state machine and outcomes (`C12`, `C15`)

### `crates/fr-command` (command-semantics seam)

1. `expire_command_surface`
   - parse/validate `NX/XX/GT/LT`, timestamp normalization, immediate-delete rewrite (`C04`, `C05`)
2. `ttl_query_surface`
   - `TTL/PTTL/EXPIRETIME/PEXPIRETIME` reply semantics (`C06`)
3. `persist_surface`
   - TTL removal semantics and side effects (`C07`)

### `crates/fr-runtime` (policy + orchestration seam)

1. `expire_evict_preflight_gate`
   - strict/hardened policy gating for pressure and unsafe states (`C13`, `C16`)
2. `expire_evict_hook_order`
   - command-path + periodic hook order orchestration (`C14`, `C15`)
3. `expire_evict_evidence_bridge`
   - deterministic `reason_code` and replay metadata emission for packet checks

### `crates/fr-expire` (pure decision seam)

1. evolve `evaluate_expiry` into reusable decision kernel:
   - shared timestamp and expiration decision rules consumed by store/command/runtime paths (`C05`, `C09`)

### `crates/fr-eventloop` (scheduler seam)

1. `expire_evict_tick_hooks`
   - periodic active-expire and deferred eviction continuation scheduling (`C02`, `C12`, `C14`)

### `crates/fr-config` (policy/config seam)

1. packet-scoped expiration/eviction knobs:
   - maxmemory parameters, policy mode, and hardened allowlist compatibility wiring (`C10`, `C11`, `C16`)

### `crates/fr-conformance` (verification seam)

1. fixture extensions for active-expire cycles, pressure-driven eviction, and unsafe-state suppression
2. row-level assertions for `C01..C16` + adversarial `T01..T11`
3. strict/hardened drift classification and replay artifacts

## 4) Data model invariants

1. Active-expire delete determinism invariant (`I01`) preserves deterministic delete side effects.
2. Fast/slow scheduler fairness invariant (`I02`) preserves time-budget cadence and DB rotation.
3. Expire option/timestamp invariant (`I03`) preserves `EXPIRE*` parsing, normalization, and immediate-delete semantics.
4. TTL/PERSIST observability invariant (`I04`) preserves reply values and side effects.
5. Expire metadata integrity invariant (`I05`) preserves key/TTL linkage consistency.
6. Expiry gate matrix invariant (`I06`) preserves mode/flag checks before delete/access transitions.
7. Maxmemory accounting invariant (`I07`) preserves pressure classification correctness.
8. Eviction candidate policy invariant (`I08`) preserves policy-specific selection bounds.
9. Eviction loop boundedness invariant (`I09`) preserves outcome-state correctness.
10. Eviction safety-state invariant (`I10`) suppresses side effects in forbidden contexts.
11. Hook ordering/propagation invariant (`I11`) preserves deletion propagation and scheduling order.
12. Hardened allowlist invariant (`I12`) rejects non-allowlisted behavior changes.

## 5) Error taxonomy (packet-specific)

1. `ExpireEvictError::ActiveCycleContractViolation`
2. `ExpireEvictError::CycleBudgetViolation`
3. `ExpireEvictError::SubexpiryCycleViolation`
4. `ExpireEvictError::ExpireOptionParseViolation`
5. `ExpireEvictError::ExpireCommandSemanticsViolation`
6. `ExpireEvictError::TtlObservableContractViolation`
7. `ExpireEvictError::PersistContractViolation`
8. `ExpireEvictError::ExpireMetadataApiViolation`
9. `ExpireEvictError::ExpireLookupGuardViolation`
10. `ExpireEvictError::MaxmemoryStateViolation`
11. `ExpireEvictError::EvictionPolicySelectionViolation`
12. `ExpireEvictError::EvictionLoopContractViolation`
13. `ExpireEvictError::EvictionSafetyGateViolation`
14. `ExpireEvictError::RuntimeHookOrderViolation`
15. `ExpireEvictError::PropagationOrderViolation`
16. `ExpireEvictError::HardenedDeviationRejected`

Each error maps to deterministic `expire.*`, `evict.*`, or `expireevict.*` reason codes from FR-P2C-008 contracts.

## 6) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: lock command-level `EXPIRE*` parsing + timestamp/overflow normalization (`C04`, `C05`).
2. **Stage D2**: harden TTL/PERSIST observability and metadata mutation checks (`C06`, `C07`, `C08`).
3. **Stage D3**: centralize expiry decision kernel usage (`fr-expire` + store integration) and gate matrix checks (`C09`).
4. **Stage D4**: add active-expire scheduler + fast/slow budget execution (`C01`, `C02`, `C03`).
5. **Stage D5**: implement maxmemory accounting baseline and pressure classification (`C10`).
6. **Stage D6**: implement policy-driven candidate selection (`C11`).
7. **Stage D7**: implement bounded eviction loop + implicit deletion propagation contracts (`C12`, `C15`).
8. **Stage D8**: wire explicit unsafe-state suppression gates (`C13`).
9. **Stage D9**: enforce runtime hook ordering across command path + periodic hooks (`C14`).
10. **Stage D10**: enforce hardened allowlist rejection and full strict/hardened adversarial sweep (`C16`, `T11`).

## 7) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-008-U001` | `C01` | `T01` | unit | active-expire delete side effects are deterministic |
| `FR-P2C-008-U002` | `C02` | `T01` | adversarial unit | fast/slow scheduler fairness and budget invariants hold |
| `FR-P2C-008-U003` | `C03` | `T01` | adversarial unit | subexpiry sweep remains bounded and scoped |
| `FR-P2C-008-U004` | `C04` | `T02` | unit | invalid option combinations rejected; predicates enforced |
| `FR-P2C-008-U005` | `C05` | `T03` | adversarial unit | immediate-delete rewrite and timestamp semantics are deterministic |
| `FR-P2C-008-U006` | `C06` | `T04` | unit | TTL-family observable replies match contract |
| `FR-P2C-008-U007` | `C07` | `T04` | unit | `PERSIST` semantics and side effects match contract |
| `FR-P2C-008-U008` | `C08` | `T05` | unit | metadata API linkage invariants hold |
| `FR-P2C-008-U009` | `C09` | `T05` | adversarial unit | lookup/access gate matrix prevents forbidden paths |
| `FR-P2C-008-U010` | `C10` | `T06` | unit | maxmemory pressure state computed correctly |
| `FR-P2C-008-U011` | `C11` | `T07` | adversarial unit | policy candidate selection/tie bounds are deterministic |
| `FR-P2C-008-U012` | `C12`, `C15` | `T08`, `T10` | adversarial unit | eviction loop outcome + propagation order remain bounded/deterministic |
| `FR-P2C-008-U013` | `C13`, `C16` | `T09`, `T11` | policy/adversarial unit | unsafe states suppress eviction; non-allowlisted hardened deviations rejected |
| `FR-P2C-008-U014` | `C14` | `T10` | integration unit | runtime hook ordering is deterministic |

## 8) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-008-E001` | `C01` | `T01` | active-expire deletion behavior and stats parity |
| `FR-P2C-008-E002` | `C02` | `T01` | fast/slow cycle cadence under mixed-key workloads |
| `FR-P2C-008-E003` | `C03` | `T01` | subexpiry sweep boundedness under field-heavy keys |
| `FR-P2C-008-E004` | `C04` | `T02` | `EXPIRE*` option matrix compatibility |
| `FR-P2C-008-E005` | `C05` | `T03` | immediate-delete rewrite and propagation behavior |
| `FR-P2C-008-E006` | `C06` | `T04` | TTL-family reply contract parity |
| `FR-P2C-008-E007` | `C07` | `T04` | `PERSIST` side effects under mixed persistence states |
| `FR-P2C-008-E008` | `C08` | `T05` | metadata consistency after mutation churn |
| `FR-P2C-008-E009` | `C09` | `T05` | gate matrix behavior in replica/import/pause contexts |
| `FR-P2C-008-E010` | `C10` | `T06` | pressure classification stability under memory churn |
| `FR-P2C-008-E011` | `C11` | `T07` | policy sampling/ranking behavior bounds |
| `FR-P2C-008-E012` | `C12`, `C15` | `T08`, `T10` | bounded eviction outcomes + propagation ordering |
| `FR-P2C-008-E013` | `C13`, `C16` | `T09`, `T11` | unsafe-state suppression + hardened rejection contract |
| `FR-P2C-008-E014` | `C14` | `T10` | command-path and periodic-hook ordering parity |

## 9) Structured logging boundary interface

All expiration/eviction boundaries (store scheduler/accounting/policy/loop,
command handlers, runtime gates, eventloop hooks) must emit:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 10) Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-008-CLAIM-04` |
| `evidence_id` | `FR-P2C-008-EVID-PLAN-001` |
| Hotspot evidence | `D4`, `D7`, `D9` (active-expire scheduler, bounded eviction loop, runtime hook ordering) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-PERF-08`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis expiration+eviction path (`expire.c` + `evict.c` + runtime hooks) |
| EV score | `3.2` |
| Priority tier | `S` |
| Adoption wedge | Land expiry command semantics and scheduler first, then maxmemory/eviction loop, then hardened policy enforcement |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` (allowlist only) |
| Deterministic exhaustion behavior | Hardened budget exhaustion => strict-equivalent fail-closed with `expireevict.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED` |

## 11) Expected-loss decision model

States:

- `S0`: implementation preserves expiration/eviction contracts
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe expiration/eviction divergence

Actions:

- `A0`: continue normal implementation path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail closed and block stage promotion

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)` from bounded parse/metadata irregularities without ordering drift.
- `P(S2|e)` from expiry-gate mismatch, pressure-accounting drift, or eviction-loop outcome violations.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive windows with critical-row drift (`C05`, `C09`, `C12`, `C16`) or calibration breach.
- Fallback behavior: disable hardened deviations and force strict fail-closed packet mode.

## 12) One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-008-04`: deterministic pressure-decision cache keyed by `(policy_epoch, pressure_bucket, unsafe_state_mask, ttl_bucket)` with strict invalidation on metadata/policy transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-008/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-008/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-008/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-008/isomorphism_report.md`

## 13) Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-008/env.json`
- `artifacts/phase2c/FR-P2C-008/manifest.json`
- `artifacts/phase2c/FR-P2C-008/repro.lock`
- `artifacts/phase2c/FR-P2C-008/LEGAL.md` (required if IP/provenance risk is plausible)

## 14) Verification command set (local + CI replay)

- `rch exec -- cargo test -p fr-expire -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-runtime -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_STRICT`
- `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED`
- `rch exec -- cargo clippy --workspace --all-targets -- -D warnings`
