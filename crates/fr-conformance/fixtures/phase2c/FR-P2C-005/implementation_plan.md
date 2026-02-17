# FR-P2C-005 Rust Implementation Plan

Packet: `FR-P2C-005`  
Scope: persistence format and replay parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible persistence/replay behavior for FR-P2C-005 with:

- deterministic AOF/RDB replay ordering,
- explicit strict/hardened compatibility boundaries,
- fail-closed behavior for tamper/unknown drift paths,
- traceable unit/e2e evidence for each `C01..C14` and `T01..T10` row.

## 2) Module boundary skeleton

### `crates/fr-persist` (core persistence crate)

Proposed decomposition:

1. `manifest.rs`
   - manifest parse/validate/serialize (`BASE`/`INCR`/`HISTORY` invariants)
2. `aof_feed.rs`
   - canonical command serialization and DB-boundary `SELECT` emission
3. `aof_replay.rs`
   - replay parser/executor pipeline (including RDB preamble handoff)
4. `aof_repair.rs`
   - bounded tail-truncation repair policy gates
5. `rdb_codec.rs`
   - RDB header/opcode/checksum read-write support
6. `rewrite.rs`
   - atomic rewrite/rotate orchestration semantics
7. `errors.rs`
   - packet-specific persistence/replay error taxonomy

### `crates/fr-runtime` (integration seam)

1. startup persistence selection policy (AOF-first when enabled)
2. replay application ordering and state digest checkpoints
3. policy-driven threat/evidence emission for persistence anomalies

### `crates/fr-config` (policy seam)

1. hardened allowlist enforcement for packet-relevant categories:
   - `BoundedReplayRepair`
   - `BoundedParserDiagnostics`
   - `MetadataSanitization`
2. explicit reject path for non-allowlisted deviations

### `crates/fr-repl` / runtime-repl seam

1. full-sync transition hooks for stop-AOF/load-RDB/restart-AOF ordering
2. `WAITAOF`-style ack semantics surface and offset visibility

### `crates/fr-conformance` (verification seam)

1. packet fixture wiring for FR-P2C-005
2. contract-row assertions (`C01..C14`)
3. threat-row adversarial assertions (`T01..T10`)

## 3) Data model invariants

1. Manifest chain order invariant (`I01`) is immutable.
2. RDB-preamble + AOF-tail cursor invariant (`I02`) must never drift.
3. Replay transaction boundary invariant (`I03`) forbids partial `MULTI` apply.
4. Non-final corruption invariant (`I04`) is always fail-closed.
5. Final-tail repair invariant (`I05`) is strictly bounded and policy-gated.
6. Checksum invariant (`I06`) requires reject on mismatch when enabled.
7. Sync transition invariant (`I07`) requires stop/load/restart sequencing.
8. Ack invariant (`I08`) requires local+replica thresholds for unblocking.
9. Disk-error gate invariant (`I09`) forbids writes during active persistence fault.
10. Hardened allowlist invariant (`I10`) rejects non-allowlisted deviation.

## 4) Error taxonomy (packet-specific)

1. `PersistError::ManifestParseViolation`
2. `PersistError::ManifestPathViolation`
3. `PersistError::ReplayFrameInvalid`
4. `PersistError::ReplayOrderViolation`
5. `PersistError::ReplayIncompleteMulti`
6. `PersistError::ReplayNonFinalCorruption`
7. `PersistError::RdbOpcodeViolation`
8. `PersistError::RdbChecksumMismatch`
9. `PersistError::DiskErrorWriteDenied`
10. `PersistError::HardenedDeviationRejected`

Each error maps directly to contract/risk `reason_code` values.

## 5) Staged implementation sequence (risk-minimizing)

1. **Stage D1**: manifest parser + validation + chain-order checks (`C01`, `C08`)
2. **Stage D2**: replay parser skeleton with strict frame validation (`C04`)
3. **Stage D3**: RDB preamble handoff + cursor accounting (`C02`, `I02`)
4. **Stage D4**: transaction checkpoint rollback for incomplete `MULTI` (`C05`)
5. **Stage D5**: bounded final-tail repair and non-final hard-fail paths (`C06`, `C07`, `C14`)
6. **Stage D6**: RDB opcode/checksum load/save contract (`C09`, `C10`)
7. **Stage D7**: propagation ordering + disk-error gate integration (`C03`, `C13`)
8. **Stage D8**: replication sync/AOF transition + `WAITAOF` semantics (`C11`, `C12`)
9. **Stage D9**: conformance fixtures + adversarial packet suite (`T01..T10`)

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-005-U001` | `C01` | `T02` | unit | manifest chain ordering preserved |
| `FR-P2C-005-U002` | `C02` | - | unit | RDB preamble to AOF tail handoff parity |
| `FR-P2C-005-U003` | `C03` | `T10` | unit | propagation order + DB boundary parity |
| `FR-P2C-005-U004` | `C06` | - | adversarial unit | bounded final-tail recovery behavior |
| `FR-P2C-005-U005` | `C07` | `T03` | adversarial unit | non-final corruption fails closed |
| `FR-P2C-005-U006` | `C05` | `T04` | adversarial unit | incomplete `MULTI` rollback |
| `FR-P2C-005-U007` | `C08` | `T01` | adversarial unit | manifest validation fail-closed |
| `FR-P2C-005-U008` | `C10` | `T05` | adversarial unit | checksum mismatch rejection |
| `FR-P2C-005-U009` | `C13` | `T08` | unit | disk-error write deny semantics |
| `FR-P2C-005-U010` | `C11` | `T06` | integration unit | sync-time AOF transition ordering |
| `FR-P2C-005-U011` | `C12` | `T07` | integration unit | `WAITAOF` local+replica ack behavior |
| `FR-P2C-005-U012` | `C14` | `T09` | policy unit | non-allowlisted hardened reject |
| `FR-P2C-005-U013` | `C04` | - | unit | replay frame parse strictness |
| `FR-P2C-005-U014` | `C09` | `T05` | unit | RDB opcode/AUX handling contract |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-005-E001` | `C01` | `T02` | chain replay parity across BASE+INCR |
| `FR-P2C-005-E002` | `C02` | - | preamble+tail replay continuity |
| `FR-P2C-005-E003` | `C03` | `T10` | propagation/replay order parity |
| `FR-P2C-005-E004` | `C06` | - | final-tail bounded recovery behavior |
| `FR-P2C-005-E005` | `C07` | `T03` | non-final corruption abort behavior |
| `FR-P2C-005-E006` | `C05` | `T04` | incomplete transaction rollback |
| `FR-P2C-005-E007` | `C08` | `T01` | manifest tamper fail-closed |
| `FR-P2C-005-E008` | `C10` | `T05` | checksum tamper rejection |
| `FR-P2C-005-E009` | `C13` | `T08` | disk error write-deny parity |
| `FR-P2C-005-E010` | `C11` | `T06` | full-sync transition ordering |
| `FR-P2C-005-E011` | `C12` | `T07` | `WAITAOF` unblock semantics |
| `FR-P2C-005-E012` | `C14` | `T09` | hardened non-allowlist rejection |
| `FR-P2C-005-E013` | `C04` | - | replay frame strict parse behavior |
| `FR-P2C-005-E014` | `C09` | `T05` | RDB opcode/AUX contract behavior |

## 8) Structured logging boundary interface

Persistence boundaries (`manifest`, `replay_parser`, `replay_apply`, `rdb_codec`,
`repair_policy`, `sync_transition`) must emit replay-complete logs with:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Execution commands (local/CI)

Use remote offload for CPU-intensive validation:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_differential_replay_fixture_passes
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_adversarial_decode_reason_taxonomy_is_stable
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_005_e2e_contract_smoke
rch exec -- cargo fmt --check
```

## 10) Sequencing boundary notes

- This bead defines architecture and execution sequencing only.
- Behavior-changing persistence implementation proceeds in `bd-2wb.16.5+`.
- Any deferred semantics remain explicitly tied to follow-up packet beads.

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

- `LEV-005-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

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
rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_differential_replay_fixture_passes
```
