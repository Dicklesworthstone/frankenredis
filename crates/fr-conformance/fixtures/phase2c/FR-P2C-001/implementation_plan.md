# FR-P2C-001 Rust Implementation Plan

Packet: `FR-P2C-001`  
Scope: event loop core parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible event-loop behavior in Rust with:

- deterministic phase ordering,
- explicit strict/hardened compatibility boundaries,
- fail-closed behavior for undefined/non-allowlisted states,
- traceable unit/e2e evidence for each contract row.

## 2) Module boundary skeleton

### `crates/fr-eventloop` (core)

Proposed modules:

1. `event_loop.rs`
   - `EventLoopState`
   - phase machine (`BeforeSleep`, `Poll`, `AfterSleep`, `FileDispatch`, `TimeDispatch`)
2. `file_events.rs`
   - fd registration/deletion, mask semantics, `AE_BARRIER` ordering equivalent
3. `time_events.rs`
   - timer registry, logical-delete, reschedule semantics
4. `poll_backend.rs`
   - trait-based backend abstraction (`epoll`/`kqueue`/`select` compatibility surface)
5. `hooks.rs`
   - before/after sleep hook registration and execution contracts
6. `errors.rs`
   - event-loop error taxonomy (bounds, invalid state, backend failure)
7. `metrics.rs`
   - deterministic counters and optional instrumentation (no behavior changes)

### `crates/fr-runtime` (integration seam)

1. loop construction and lifecycle ownership
2. wiring for:
   - pre-sleep pipeline tasks
   - post-sleep tasks
   - no-sleep gating
3. strict/hardened policy gating via `fr-config::RuntimePolicy`

### `crates/fr-config` (policy seam)

1. allowlisted hardened deviation gates for event-loop packet:
   - `BoundedParserDiagnostics`
   - `ResourceClamp`
2. explicit reject path for non-allowlisted deviations

### `crates/fr-conformance` (verification seam)

1. packet fixture wiring for FR-P2C-001
2. contract-row assertions (`C01..C10`)
3. adversarial threat assertions (`T01..T08`)

## 3) Data model invariants

1. Callback phase order invariant (`I01`) is immutable under strict/hardened.
2. Barrier ordering invariant (`I02`) must hold for dual read/write readiness.
3. Registration invariant (`I06`): no partial mutation on out-of-range fd.
4. Bounded blocked-mode invariant (`I05`): limited iterations; reduced work scope.
5. Delivery invariant (`I07`): no silent pending-write loss.

## 4) Error taxonomy (packet-specific)

1. `EventLoopError::FdOutOfRange`
2. `EventLoopError::StateTransitionInvalid`
3. `EventLoopError::BackendPollFailed`
4. `EventLoopError::HookMissing`
5. `EventLoopError::PendingWriteInvariantViolation`
6. `EventLoopError::BlockedModeScopeViolation`

Each error maps to one or more `reason_code` values from `contract_table.md` / `risk_note.md`.

## 5) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: phase machine skeleton + hook registration (`C01`, `C09`, `C10`)
2. **Stage D2**: file-event registry + bounds checks + barrier semantics (`C02`, `C04`)
3. **Stage D3**: timer registry and time-dispatch ordering (`C01`, timer portion)
4. **Stage D4**: no-sleep gating and poll timeout logic (`C03`)
5. **Stage D5**: pending-write scheduling semantics (`C07`)
6. **Stage D6**: blocked-mode bounded path (`C08`)
7. **Stage D7**: runtime integration in `fr-runtime` and policy wiring
8. **Stage D8**: conformance packet fixture + adversarial threat suite

## 6) Unit/property test matrix

| Test ID | Contract rows | Type | Expected result |
|---|---|---|---|
| `FR-P2C-001-U001` | `C01` | unit | phase order preserved |
| `FR-P2C-001-U002` | `C02` | unit | barrier ordering preserved |
| `FR-P2C-001-U003` | `C03` | unit | no-sleep/timeout semantics preserved |
| `FR-P2C-001-U004` | `C04` | unit | fd resize and bounds behavior preserved |
| `FR-P2C-001-U005` | `C08` | unit | blocked-mode bounded scope preserved |
| `FR-P2C-001-U006` | `C05` | adversarial unit | maxclients reject path preserved |
| `FR-P2C-001-U007` | `C06` | adversarial unit | query-buffer over-limit disconnect preserved |
| `FR-P2C-001-U008` | `C06` | adversarial unit | fatal read path terminates processing |
| `FR-P2C-001-U009` | `C07` | unit | no pending-write loss/reorder |
| `FR-P2C-001-U010` | `C09` | unit | hook/timer wiring enforced |
| `FR-P2C-001-U011` | `C10` | unit | runtime loop entry contract enforced |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-001-E001` | `C01` | - | end-to-end phase order parity |
| `FR-P2C-001-E002` | `C02` | `T03` | barrier-compatible flush/reply ordering |
| `FR-P2C-001-E003` | `C03` | `T04` | no-sleep path under pending conditions |
| `FR-P2C-001-E004` | `C04` | `T06` | registration/resizing robustness |
| `FR-P2C-001-E005` | `C08` | `T05` | blocked-mode bounded behavior |
| `FR-P2C-001-E006` | `C05` | `T01` | connection-cap rejection contract |
| `FR-P2C-001-E007` | `C06` | `T02` | query-buffer abuse handling |
| `FR-P2C-001-E009` | `C07` | `T07` | pending-write delivery integrity |
| `FR-P2C-001-E010` | `C09` | `T08` | init hook/timer presence |
| `FR-P2C-001-E011` | `C10` | `T08` | main loop entry validation |

## 8) Structured logging boundary interface

Every boundary (`hooks`, `poll`, `file_events`, `time_events`, `runtime_integration`) must emit replay-complete logs with:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Execution commands (local/CI)

Use remote offload for CPU-intensive checks:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u001_phase_order_is_deterministic
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u005_runtime_blocked_mode_is_bounded
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_001_e2e_contract_smoke
rch exec -- cargo fmt --check
```

## 10) Sequencing boundary notes

- This bead defines architecture/plan only (no behavior-changing Rust implementation yet).
- Implementation execution continues in `bd-2wb.12.5` and later packet-001 dependents.
- Any behavior deferment must remain explicitly linked to packet-001 follow-up beads.

## 11) Expected-loss decision model and fallback policy (optimization lever)

States:

- `S0`: metadata syscall pressure dominates latency
- `S1`: JSON parse dominates latency
- `S2`: mixed pressure

Actions:

- `A0`: keep repeated per-file probes
- `A1`: single directory scan + file-presence bitmask

Loss matrix (lower is better):

| State \ Action | `A0` | `A1` |
|---|---:|---:|
| `S0` | 8 | 2 |
| `S1` | 3 | 3 |
| `S2` | 5 | 3 |

Posterior/evidence terms:

- Baseline `statx` hotspot dominance from strace profile.
- Post-change `statx` reduction while preserving output checksum.

Calibration + fallback:

- Calibration metric target: non-negative delta over 20-run hyperfine window.
- Fallback trigger: `delta_percent <= 0` or output checksum divergence.
- Fallback behavior: reject optimization promotion and keep fail-closed baseline.

## 12) One-lever extreme-optimization loop artifacts

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

## 13) Reproducibility/provenance pack references

- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/env.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/manifest.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/repro.lock`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/LEGAL.md` (required if IP/provenance risk is plausible)

## 14) Benchmark replay commands (`rch`-offloaded)

```bash
rch exec -- cargo build -p fr-conformance --bin phase2c_schema_gate
rch exec -- bash artifacts/optimization/phase2c-gate/run_gate_bench.sh
rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_001_f_differential_fixture_passes
```
