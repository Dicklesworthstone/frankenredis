# FR-P2C-001 Risk Note

Packet: `FR-P2C-001`  
Subsystem: event loop core  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/contract_table.md`

## Compatibility envelope

- `strict` mode: must preserve Redis-observable ordering, replies, disconnections,
  and side effects for event-loop-sensitive behavior.
- `hardened` mode: may add bounded defenses (`BoundedParserDiagnostics`,
  `ResourceClamp`) but must not alter outward API contract.
- Unknown or non-allowlisted compatibility/security behavior must be
  `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-001-T01` | Resource exhaustion | Connection storm to exceed `maxclients` | `C05`, `I03` | Deterministic reject and close | Same outward behavior; bounded diagnostics permitted | `FR-P2C-001-U006` | `FR-P2C-001-E006` | `eventloop.accept.maxclients_reached` | Critical |
| `FR-P2C-001-T02` | Parser abuse | Oversized/incremental query-buffer growth | `C06`, `I04` | Disconnect over limit; no command execution continuation | Same behavior with bounded parser telemetry | `FR-P2C-001-U007` | `FR-P2C-001-E007` | `eventloop.read.querybuf_limit_exceeded` | Critical |
| `FR-P2C-001-T03` | Ordering manipulation | Barrier semantics drift (`AE_BARRIER` ignored) | `C02`, `I02` | Writable/readable callback order must match legacy | Same | `FR-P2C-001-U002` | `FR-P2C-001-E002` | `eventloop.ae_barrier_violation` | Critical |
| `FR-P2C-001-T04` | Starvation / busy-loop abuse | Incorrect dont-wait gating under pending data | `C03`, `I01` | Poll/sleep behavior follows legacy conditions | May clamp to deterministic no-wait only when allowlisted | `FR-P2C-001-U003` | `FR-P2C-001-E003` | `eventloop.dont_wait_not_set`, `eventloop.timeout_invalid` | High |
| `FR-P2C-001-T05` | Re-entry abuse | `processEventsWhileBlocked` executes unbounded/full pipeline | `C08`, `I05` | Bounded iterations + reduced work scope | Same | `FR-P2C-001-U005` | `FR-P2C-001-E005` | `eventloop.blocked_mode_progress_stall`, `eventloop.blocked_mode_scope_violation` | High |
| `FR-P2C-001-T06` | State corruption | FD/event-array resize inconsistency | `C04`, `I06` | Out-of-range fd hard-error; no partial side effects | Same, optional resource-clamp diagnostics | `FR-P2C-001-U004` | `FR-P2C-001-E004` | `eventloop.fd_resize_failure`, `eventloop.fd_out_of_range` | High |
| `FR-P2C-001-T07` | Delivery integrity drift | Pending writes silently dropped/reordered | `C07`, `I07` | No silent drop; deterministic write scheduling | Same | `FR-P2C-001-U009` | `FR-P2C-001-E009` | `eventloop.write.flush_order_violation`, `eventloop.write.pending_reply_lost` | High |
| `FR-P2C-001-T08` | Initialization downgrade | Missing before/after sleep hooks or cron timer | `C09`, `C10`, `I08` | Loop not considered valid until required hooks/timer exist | Same | `FR-P2C-001-U010`, `FR-P2C-001-U011` | `FR-P2C-001-E010`, `FR-P2C-001-E011` | `eventloop.hook_install_missing`, `eventloop.server_cron_timer_missing`, `eventloop.main_loop_entry_missing` | Critical |

## Fail-closed rules

1. Any non-allowlisted deviation from `C01..C10` is a hard failure.
2. Unknown state transition in event-loop phase machine is rejected (`fail_closed`).
3. Out-of-range descriptor registration is rejected with no partial mutation.
4. If required initialization hooks are absent, runtime start is blocked.
5. If read path enters fatal error state, command execution path is not resumed.

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-001`
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Implemented packet-001 final parity + durability artifacts (bd-2wb.12.9)

- Final packet manifest + gate policy published:
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/fixture_manifest.json`
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/parity_gate.yaml`
  - hardened policy keeps `non_allowlisted_action=fail_closed`.
- Final packet parity report published:
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/parity_report.json`
  - readiness set to `READY_FOR_IMPL` with no missing mandatory fields.
- RaptorQ proof chain published:
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/parity_report.raptorq.json`
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/parity_report.decode_proof.json`
  - deterministic decode reason code: `raptorq.decode_verified`
  - replay command: `./scripts/run_raptorq_artifact_gate.sh --run-id local-smoke`
- Replay validation command:
  - `rch exec -- cargo run -p fr-conformance --bin phase2c_schema_gate -- crates/fr-conformance/fixtures/phase2c/FR-P2C-001`

## Expected-loss decision model

### States

- `S0`: contract-preserving operation
- `S1`: bounded defensive handling required
- `S2`: unsafe/unknown compatibility drift detected

### Actions

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense
- `A2`: fail closed and abort operation

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 6 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 7 | 1 |

Decision policy:

- choose action minimizing expected loss under posterior state estimate.
- when posterior confidence for `S2` exceeds threshold `0.35`, enforce `A2` (`fail_closed`).
- when confidence for `S1` exceeds `0.40` and deviation category is allowlisted, choose `A1`.

## Calibration and fallback trigger

- Calibration metric: per-threat false-negative rate on adversarial suite must be `< 1%`.
- Fallback trigger: if any critical threat row (`T01`, `T02`, `T03`, `T08`) exhibits unresolved drift in strict mode, packet promotion is blocked and runtime path remains fail-closed.

## Replay commands

- Strict unit threat suite: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u006_accept_path_rejects_over_maxclients`
- Hardened unit threat suite: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u005_runtime_blocked_mode_is_bounded`
- E2E threat suite: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_001_e2e_contract_smoke`

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-001-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

Required artifacts:

- Baseline/profile evidence: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/baseline_hyperfine.json`
- Hotspot syscall profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/baseline_strace.txt`
- Chosen lever note: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/optimization_report.md`
- Recommendation contract card: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/alien_recommendation_card.md`
- Post-change re-profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/after_hyperfine.json`
- Post-change syscall profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/after_strace.txt`
- Behavior-isomorphism proof: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/isomorphism_check.txt`

## Reproducibility/provenance pack references

- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/env.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/manifest.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/repro.lock`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/LEGAL.md` (required when IP/provenance risk is plausible)

## Residual risks

- Full fidelity still depends on downstream implementation (`bd-2wb.12.4+`) and differential fixtures (`bd-2wb.12.6+`).
- IO-thread behavior parity is high risk until integration tests with thread handoff are landed.
