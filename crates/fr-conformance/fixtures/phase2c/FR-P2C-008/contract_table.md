# FR-P2C-008 Contract Table

Packet: `FR-P2C-008`  
Subsystem: expiration + eviction  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic expiration/eviction event.
- `preconditions`: required state before contract evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior preserving API/ordering contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-008-C01` | Active expiration delete event | Key logically expired and selected for deletion | Expired key deletion propagates deterministic side effects/stats/notifications. | Same; diagnostics may be richer. | Expired key retained without allowed gate justification. | `FR-P2C-008-U001` | `FR-P2C-008-E001` | `expire.active_cycle_contract_violation` |
| `FR-P2C-008-C02` | Fast/slow active-expire cycle scheduling | Expire action enabled; cycle type selected | Slow/fast cycle cadence, fairness, and time budgets follow legacy adaptive policy. | Same with bounded diagnostics. | Cycle overruns budget or starves DB sweep determinism. | `FR-P2C-008-U002` | `FR-P2C-008-E002` | `expire.fast_slow_budget_violation` |
| `FR-P2C-008-C03` | Subexpiry (hash-field expiry) sweep | Subexpires structures populated | Field-level expiry sweep preserves bounded expiration and bucket rotation invariants. | Same. | Subexpiry sweep mutates fields outside scope or skips required expiry path. | `FR-P2C-008-U003` | `FR-P2C-008-E003` | `expire.subexpiry_cycle_contract_violation` |
| `FR-P2C-008-C04` | `EXPIRE*` option parsing | Command has option tail | `NX/XX/GT/LT` compatibility and predicate checks match legacy contract. | Same. | Incompatible option combinations accepted. | `FR-P2C-008-U004` | `FR-P2C-008-E004` | `expire.option_parse_contract_violation` |
| `FR-P2C-008-C05` | Generic expire command evaluation | Key exists and expiry timestamp parsed | Relative/absolute timestamp handling, overflow checks, immediate delete path, and rewrite normalization are deterministic. | Same. | Timestamp overflow/past-time behavior accepted with wrong side effects. | `FR-P2C-008-U005` | `FR-P2C-008-E005` | `expire.command_semantics_violation`, `expire.immediate_delete_rewrite_violation` |
| `FR-P2C-008-C06` | `TTL`/`PTTL`/`EXPIRETIME`/`PEXPIRETIME` query | Key lookup result known | Reply semantics preserve `-2` missing, `-1` persistent, relative/absolute conversion rules. | Same. | TTL-family reply drift from observable contract. | `FR-P2C-008-U006` | `FR-P2C-008-E006` | `expire.ttl_observable_contract_violation` |
| `FR-P2C-008-C07` | `PERSIST` command | Key exists with/without TTL | TTL removal and side effects follow deterministic contract. | Same. | `PERSIST` mutates key state inconsistently. | `FR-P2C-008-U007` | `FR-P2C-008-E007` | `expire.persist_contract_violation` |
| `FR-P2C-008-C08` | Expire metadata API mutation | Key metadata entry exists | `setExpire`/`removeExpire`/`getExpire` preserve metadata integrity and dict-link invariants. | Same. | Expire metadata out-of-sync with key state. | `FR-P2C-008-U008` | `FR-P2C-008-E008` | `expire.metadata_api_contract_violation` |
| `FR-P2C-008-C09` | Expiry gate evaluation on access/write path | Key may be logically expired; mode flags available | `expireIfNeeded` honors replica/import/pause/force-delete/access flags deterministically. | Same. | Gate allows forbidden access/deletion path. | `FR-P2C-008-U009` | `FR-P2C-008-E009` | `expire.lookup_guard_contract_violation` |
| `FR-P2C-008-C10` | Maxmemory state check | Maxmemory configured or checked | Memory accounting excludes not-counted overhead and computes accurate to-free/level values. | Same. | Memory pressure misclassified due accounting drift. | `FR-P2C-008-U010` | `FR-P2C-008-E010` | `evict.maxmemory_state_contract_violation` |
| `FR-P2C-008-C11` | Eviction candidate selection | Policy in `{LRU,LFU,volatile-ttl,random}` | Candidate sampling/ranking follows policy-specific contract with deterministic tie behavior bounds. | Same. | Candidate policy mismatch changes observable eviction behavior envelope. | `FR-P2C-008-U011` | `FR-P2C-008-E011` | `evict.policy_candidate_selection_violation` |
| `FR-P2C-008-C12` | `performEvictions` under pressure | Over-maxmemory and eviction allowed | Eviction loop runs with bounded time slices, async continuation semantics, and deterministic success/fail outcomes. | Same with bounded diagnostics. | Eviction loop exceeds safety budget or returns invalid outcome state. | `FR-P2C-008-U012` | `FR-P2C-008-E012` | `evict.eviction_loop_contract_violation` |
| `FR-P2C-008-C13` | Eviction safety gate evaluation | Loading/yielding/replica-ignore/ASM-import/pause states may apply | Unsafe contexts suppress eviction deterministically without side effects. | Same. | Eviction proceeds in forbidden safety state. | `FR-P2C-008-U013` | `FR-P2C-008-E013` | `evict.safety_gate_contract_violation` |
| `FR-P2C-008-C14` | Runtime hook integration | Server cron/beforeSleep/command path active | Slow+fast expire and command-path eviction hooks execute in deterministic order and gating. | Same. | Hook ordering/gating drift alters observable expire/evict behavior. | `FR-P2C-008-U014` | `FR-P2C-008-E014` | `expireevict.runtime_hook_order_violation` |
| `FR-P2C-008-C15` | Implicit deletion propagation | Expire/evict deletion triggered outside command call path | DEL/UNLINK propagation ordering remains deterministic for replica/AOF correctness. | Same. | Implicit deletion propagation order diverges. | `FR-P2C-008-U012` | `FR-P2C-008-E012` | `expireevict.propagation_order_violation` |
| `FR-P2C-008-C16` | Hardened non-allowlisted deviation candidate | Mode=`hardened` and deviation unresolved | Strict-equivalent fail-closed baseline unless deviation category is explicitly allowlisted. | Only allowlisted bounded defenses may proceed with policy evidence. | Non-allowlisted deviation changes outward expiration/eviction semantics. | `FR-P2C-008-U013` | `FR-P2C-008-E013` | `expireevict.hardened_nonallowlisted_rejected`, `expireevict.hardened_policy_violation` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-008-I01` | Active expiration delete determinism | Required | Required |
| `FR-P2C-008-I02` | Fast/slow cycle budget + fairness | Required | Required |
| `FR-P2C-008-I03` | `EXPIRE*` option and timestamp semantics | Required | Required |
| `FR-P2C-008-I04` | TTL/PERSIST observable reply semantics | Required | Required |
| `FR-P2C-008-I05` | Expire metadata API consistency | Required | Required |
| `FR-P2C-008-I06` | `expireIfNeeded` policy gate matrix | Required | Required |
| `FR-P2C-008-I07` | Maxmemory accounting correctness | Required | Required |
| `FR-P2C-008-I08` | Eviction policy candidate selection correctness | Required | Required |
| `FR-P2C-008-I09` | Eviction loop boundedness/outcome semantics | Required | Required |
| `FR-P2C-008-I10` | Eviction safety-state suppression semantics | Required | Required |
| `FR-P2C-008-I11` | Runtime hook ordering and propagation consistency | Required | Required |
| `FR-P2C-008-I12` | Non-allowlisted hardened deviation handling | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer diagnostics for expire/evict parse/state checks without behavior drift.
- `MetadataSanitization`: bounded metadata sanitation only when strict-equivalent.
- `ResourceClamp`: bounded resource control that does not alter visible command/reply/deletion ordering contract.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-008 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

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

## Replay command templates

- Unit/property: `rch exec -- cargo test -p fr-expire -- --nocapture FR_P2C_008`
- Store-level integration: `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`
- Command-layer integration: `rch exec -- cargo test -p fr-command -- --nocapture FR_P2C_008`
- Integration/E2E: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008`
- Strict-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_STRICT`
- Hardened-mode sweep: `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED`

## Implemented unit/property evidence (bd-2wb.19.5)

- `fr_p2c_008_u005_nonpositive_expire_deletes_immediately_and_logs` (`C05`)  
  Replay: `FR_MODE=strict FR_SEED=803 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u005_nonpositive_expire_deletes_immediately_and_logs`
- `fr_p2c_008_u006_ttl_pttl_persist_contract_and_logs` (`C06`/`C07`)  
  Replay: `FR_MODE=strict FR_SEED=812 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u006_ttl_pttl_persist_contract_and_logs`
- `fr_p2c_008_u009_property_expired_keys_are_invisible_across_access_paths` (`C09` + lazy-expire observability property)  
  Replay: `FR_MODE=strict FR_SEED=1050 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_u009_property_expired_keys_are_invisible_across_access_paths`

## Implemented differential/metamorphic/adversarial evidence (bd-2wb.19.6)

- `fr_p2c_008_f_differential_fixture_passes` (`fr_p2c_008_expire_evict_journey.json`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_differential_fixture_passes`
- `fr_p2c_008_f_differential_expire_evict_surface_mode_split_is_stable` (`C05`/`C06`/`C07`/`C09`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_differential_expire_evict_surface_mode_split_is_stable`
- `fr_p2c_008_f_metamorphic_expire_and_pexpire_equivalence_holds` (`C03`/`C06`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_metamorphic_expire_and_pexpire_equivalence_holds`
- `fr_p2c_008_f_adversarial_expire_reason_codes_are_stable` (`C05`/`C06`/`C07`/`C16`)  
  Replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_008_f_adversarial_expire_reason_codes_are_stable`
- Live oracle differential entrypoint:  
  `rch exec -- cargo run -p fr-conformance --bin live_oracle_diff -- command fr_p2c_008_expire_evict_journey.json 127.0.0.1 6379`

## Implemented e2e evidence (bd-2wb.19.7)

- `fr_p2c_008_e2e_contract_smoke` (`E001` smoke path for `C05`/`C06`/`C09` contract surface)  
  Fixture: `crates/fr-conformance/fixtures/fr_p2c_008_expire_evict_journey.json`  
  Replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_008_e2e_contract_smoke`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-008-CLAIM-02` |
| `evidence_id` | `FR-P2C-008-EVID-CONTRACT-001` |
| Hotspot evidence | `C02`, `C12`, `C14` (expire scheduler, eviction loop, runtime hook ordering) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-PERF-08`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis expire/evict state machine |
| EV score | `3.0` |
| Priority tier | `S` |
| Adoption wedge | Land active-expire + EXPIRE* semantics first, then maxmemory loop and safety gates |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | On budget exhaustion force strict-equivalent fail-closed with `expireevict.hardened_budget_exhausted_failclosed` |
| Replay commands | `rch exec -- cargo test -p fr-store -- --nocapture FR_P2C_008`; `rch exec -- cargo test -p fr-conformance -- --nocapture FR_P2C_008_HARDENED` |

## Expected-loss decision model

States:

- `S0`: contract-preserving expire/evict behavior
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe expiration/eviction divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail closed and block transition

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: bounded metadata/parse anomalies without deletion-order drift.
- `P(S2|e)`: expiry gate mismatches, eviction policy drift, or unsafe maxmemory handling.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive calibration breaches or critical row drift (`C05`, `C09`, `C12`, `C16`).
- Fallback behavior: disable hardened deviations and enforce strict fail-closed packet mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-008-02`: deterministic expiry+eviction decision memo keyed by `(key_digest, now_bucket, policy_epoch, pressure_bucket)` with strict invalidation on TTL/policy mutations.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-008/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-008/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-008/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-008/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-008/env.json`
- `artifacts/phase2c/FR-P2C-008/manifest.json`
- `artifacts/phase2c/FR-P2C-008/repro.lock`
- `artifacts/phase2c/FR-P2C-008/LEGAL.md` (required if IP/provenance risk is found)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.
- Expiration/eviction user-visible outcomes are explicit for replies, deletions, and memory-pressure behavior.
