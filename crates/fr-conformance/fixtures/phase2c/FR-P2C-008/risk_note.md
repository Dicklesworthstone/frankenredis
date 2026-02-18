# FR-P2C-008 Risk Note

Packet: `FR-P2C-008`  
Subsystem: expiration + eviction  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable expiration and eviction replies,
  side effects, ordering, and maxmemory behavior for scoped packet surfaces.
- `hardened` mode: permit only bounded controls
  (`BoundedParserDiagnostics`, `MetadataSanitization`, `ResourceClamp`) with no
  outward API/ordering drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-008-T01` | Expiration liveness drift | Active expire cycles under-expire or over-expire keys | `C01`, `C02`, `C03`, `I01`, `I02` | Deterministic bounded expiry cycles and deletion semantics | Same | `FR-P2C-008-U002` | `FR-P2C-008-E002` | `expire.active_cycle_contract_violation`, `expire.fast_slow_budget_violation`, `expire.subexpiry_cycle_contract_violation` | Critical |
| `FR-P2C-008-T02` | Option-policy bypass | Incompatible `NX/XX/GT/LT` combinations accepted or misinterpreted | `C04`, `I03` | Option matrix rejects invalid combinations and enforces predicates | Same | `FR-P2C-008-U004` | `FR-P2C-008-E004` | `expire.option_parse_contract_violation` | High |
| `FR-P2C-008-T03` | Immediate-delete rewrite drift | Past-time `EXPIRE*` path fails to rewrite/propagate deterministic delete | `C05`, `C15`, `I03`, `I11` | Immediate delete path emits deterministic DEL/UNLINK propagation | Same | `FR-P2C-008-U005` | `FR-P2C-008-E005` | `expire.immediate_delete_rewrite_violation`, `expireevict.propagation_order_violation` | Critical |
| `FR-P2C-008-T04` | TTL observability mismatch | TTL-family and PERSIST replies diverge from contract values | `C06`, `C07`, `I04` | Preserve `-2`/`-1`/relative/absolute semantics and PERSIST side effects | Same | `FR-P2C-008-U006` | `FR-P2C-008-E006` | `expire.ttl_observable_contract_violation`, `expire.persist_contract_violation` | High |
| `FR-P2C-008-T05` | Expiry gate bypass | `expireIfNeeded` allows forbidden access/deletion in replica/import/pause contexts | `C08`, `C09`, `I05`, `I06` | Enforce mode/flag gate matrix deterministically | Same | `FR-P2C-008-U009` | `FR-P2C-008-E009` | `expire.metadata_api_contract_violation`, `expire.lookup_guard_contract_violation` | Critical |
| `FR-P2C-008-T06` | Maxmemory accounting abuse | Memory pressure undercount/overcount causes incorrect eviction decisions | `C10`, `I07` | Accurate to-free/level accounting with not-counted memory handling | Same | `FR-P2C-008-U010` | `FR-P2C-008-E010` | `evict.maxmemory_state_contract_violation` | Critical |
| `FR-P2C-008-T07` | Eviction policy poisoning | Candidate selection deviates from LRU/LFU/TTL/random policy envelope | `C11`, `I08` | Deterministic policy-specific candidate behavior | Same | `FR-P2C-008-U011` | `FR-P2C-008-E011` | `evict.policy_candidate_selection_violation` | High |
| `FR-P2C-008-T08` | Eviction loop starvation | Eviction loop runs unbounded or exits with wrong state under pressure | `C12`, `I09` | Bounded loop with deterministic `OK/RUNNING/FAIL` semantics | Same | `FR-P2C-008-U012` | `FR-P2C-008-E012` | `evict.eviction_loop_contract_violation` | Critical |
| `FR-P2C-008-T09` | Safety gate bypass | Eviction proceeds during unsafe contexts (ASM importing, paused evict, replica-ignore) | `C13`, `I10` | Unsafe-state suppression prevents eviction side effects | Same | `FR-P2C-008-U013` | `FR-P2C-008-E013` | `evict.safety_gate_contract_violation` | High |
| `FR-P2C-008-T10` | Hook/propgation ordering drift | Runtime hook ordering or implicit deletion propagation diverges | `C14`, `C15`, `I11` | Deterministic hook ordering and deletion propagation order preserved | Same | `FR-P2C-008-U014` | `FR-P2C-008-E014` | `expireevict.runtime_hook_order_violation`, `expireevict.propagation_order_violation` | High |
| `FR-P2C-008-T11` | Policy downgrade abuse | Hardened mode applies non-allowlisted expire/evict behavior | `C16`, `I12` | N/A (strict fail-closed baseline) | Reject non-allowlisted deviation | `FR-P2C-008-U013` | `FR-P2C-008-E013` | `expireevict.hardened_nonallowlisted_rejected`, `expireevict.hardened_policy_violation` | Critical |

## Fail-closed rules

1. Expiry-gate ambiguity must not execute deletion/access paths without allowed policy (`C09`).
2. Over-maxmemory handling must not silently continue with unsafe accounting drift (`C10`, `C12`).
3. Eviction must not run in forbidden safety states (`C13`).
4. Implicit deletion propagation ordering drift is fail-closed (`C15`).
5. Non-allowlisted hardened deviations are rejected (`C16`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-008`
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Implemented packet-008 unit/property evidence (bd-2wb.19.5)

- `fr_p2c_008_u005_nonpositive_expire_deletes_immediately_and_logs`  
  Focus: immediate-delete rewrite contract for non-positive expire values (`T03`/`C05`)  
  Replay: `FR_MODE=strict FR_SEED=803 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u005_nonpositive_expire_deletes_immediately_and_logs`
- `fr_p2c_008_u006_ttl_pttl_persist_contract_and_logs`  
  Focus: TTL/PTTL/PERSIST observability contract and deterministic reply semantics (`T04`/`C06`/`C07`)  
  Replay: `FR_MODE=strict FR_SEED=812 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u006_ttl_pttl_persist_contract_and_logs`
- `fr_p2c_008_u009_property_expired_keys_are_invisible_across_access_paths`  
  Focus: lazy-expire visibility/property reduction across read-path orderings (`T05`/`C09`)  
  Replay: `FR_MODE=strict FR_SEED=1050 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u009_property_expired_keys_are_invisible_across_access_paths`

## Implemented packet-008 differential/metamorphic/adversarial evidence (bd-2wb.19.6)

- `fr_p2c_008_f_differential_fixture_passes`  
  Focus: deterministic packet journey fixture parity for expiration/eviction command envelope (`T01`/`T03`/`T04`/`T05`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_differential_fixture_passes`
- `fr_p2c_008_f_differential_expire_evict_surface_mode_split_is_stable`  
  Focus: strict vs hardened output equivalence on packet-008 scoped expire surface (`T03`/`T04`/`T05`/`T11`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_differential_expire_evict_surface_mode_split_is_stable`
- `fr_p2c_008_f_metamorphic_expire_and_pexpire_equivalence_holds`  
  Focus: `EXPIRE` and `PEXPIRE` path convergence for TTL-family observability (`T04`/`C06`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_metamorphic_expire_and_pexpire_equivalence_holds`
- `fr_p2c_008_f_adversarial_expire_reason_codes_are_stable`  
  Focus: stable reason-code taxonomy for parse/arity drift and hardened non-allowlisted rejection (`T02`/`T11`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_adversarial_expire_reason_codes_are_stable`
- Live oracle command fixture (remote redis parity target):  
  `rch exec -- cargo run -p fr-conformance --bin live_oracle_diff -- command fr_p2c_008_expire_evict_journey.json 127.0.0.1 6379`

## Implemented packet-008 e2e probe (bd-2wb.19.7)

- `fr_p2c_008_e2e_contract_smoke`  
  Focus: deterministic expiration/eviction smoke journey spanning immediate delete, TTL observability, and lazy-expire cleanup (`T03`/`T04`/`T05`)  
  Fixture: `crates/fr-conformance/fixtures/fr_p2c_008_expire_evict_journey.json`  
  Replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_008_e2e_contract_smoke`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-008-CLAIM-03` |
| `evidence_id` | `FR-P2C-008-EVID-RISK-001` |
| Hotspot evidence | `T01`, `T06`, `T08` (active-expire liveness, maxmemory accounting, bounded eviction loop) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-PERF-08`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis expire/evict threat surface (`expire.c` + `evict.c` + `db.c` + `server.c`) |
| EV score | `3.1` |
| Priority tier | `S` |
| Adoption wedge | Enforce expire gate and maxmemory accounting guardrails before policy-level optimization |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | Budget exhaustion forces strict-equivalent fail-closed and emits `expireevict.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED` |

## Expected-loss decision model

### States

- `S0`: contract-preserving expire/evict operation
- `S1`: recoverable bounded condition (allowlisted)
- `S2`: unsafe expiration/eviction divergence condition

### Actions

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and abort transition

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: bounded parse/metadata anomalies without deletion-order drift.
- `P(S2|e)`: expiry-gate mismatch, unsafe maxmemory accounting, or eviction-loop divergence.

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial expire/evict suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C05`, `C09`, `C12`, `C16`) blocks packet promotion.
- Budget exhaustion policy: hardened exhaustion across two consecutive windows reverts packet to strict fail-closed mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-008-03`: deterministic maxmemory-pressure decision cache over `(memory_level_bucket, policy_epoch, unsafe_state_mask)` with strict invalidation on accounting/flag transitions.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-008/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-008/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-008/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-008/isomorphism_report.md`

## Replay commands

- Unit threat suite: `rch exec -- cargo test -p fr-expire -- --nocapture FR_P2C_008`
- Store threat suite: `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`
- E2E threat suite: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008`
- Hardened replay: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-008/env.json`
- `artifacts/phase2c/FR-P2C-008/manifest.json`
- `artifacts/phase2c/FR-P2C-008/repro.lock`
- `artifacts/phase2c/FR-P2C-008/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Current Rust maxmemory eviction engine is absent; threat controls remain contractual until implementation beads land.
- Expire/evict propagation ordering is highly sensitive to event-loop and replication interactions; dedicated ordering tests are mandatory.
- Subexpiry and policy-sampling behavior may require additional deterministic fixtures once hash-field expiry scope is finalized.
