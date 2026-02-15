# FR-P2C-005 Risk Note

Packet: `FR-P2C-005`  
Subsystem: persistence format and replay  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable persistence/replay ordering, replies,
  and side effects across AOF/RDB load and replay boundaries.
- `hardened` mode: allow only bounded defensive controls
  (`BoundedReplayRepair`, `BoundedParserDiagnostics`, `MetadataSanitization`)
  without outward API/ordering drift.
- Unknown or non-allowlisted behavior is `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-005-T01` | Persistence tampering | Manifest poisoning (duplicate base, malformed row, path traversal filename) | `C01`, `C08`, `I01` | Reject manifest and abort replay | Same; diagnostics may be richer | `FR-P2C-005-U007` | `FR-P2C-005-E007` | `persist.manifest.parse_or_path_violation` | Critical |
| `FR-P2C-005-T02` | Replay-order attack | Segment omission/reordering in BASE+INCR chain | `C01`, `C03`, `I01` | Deterministic chain-order enforcement; no continuation on gap | Same | `FR-P2C-005-U001` | `FR-P2C-005-E001` | `persist.replay.manifest_order_mismatch`, `persist.replay.chain_gap_detected` | Critical |
| `FR-P2C-005-T03` | Corrupt-tail abuse | Truncation/corruption in non-final segment to induce partial replay | `C06`, `C07`, `I04` | Hard fail for non-final segment corruption | Hard fail (same) | `FR-P2C-005-U005` | `FR-P2C-005-E005` | `persist.replay.nonfinal_truncation_fatal` | Critical |
| `FR-P2C-005-T04` | Transaction-boundary attack | EOF mid `MULTI/EXEC` to force partial transaction application | `C05`, `I03` | Rewind to `valid_before_multi`; no partial apply | Same | `FR-P2C-005-U006` | `FR-P2C-005-E006` | `persist.replay.incomplete_multi_rollback` | Critical |
| `FR-P2C-005-T05` | Snapshot tampering | RDB checksum mismatch, malformed opcode stream, unsafe AUX interpretation | `C09`, `C10`, `I06` | Reject corrupted snapshot; do not activate | Same | `FR-P2C-005-U008`, `FR-P2C-005-U014` | `FR-P2C-005-E008`, `FR-P2C-005-E014` | `persist.rdb.checksum_or_format_invalid`, `persist.rdb.opcode_contract_violation` | Critical |
| `FR-P2C-005-T06` | Replication-order attack | Full-sync transition leaves AOF active during inbound RDB load | `C11`, `I07` | Stop AOF before load and restart only after success | Same | `FR-P2C-005-U010` | `FR-P2C-005-E010` | `persist.replication.sync_aof_state_violation` | High |
| `FR-P2C-005-T07` | Durability-ack spoof/drift | `WAITAOF` unblocks before local fsync/replica AOF offsets satisfy threshold | `C12`, `I08` | Must remain blocked until constraints are met | Same | `FR-P2C-005-U011` | `FR-P2C-005-E011` | `persist.waitaof_ack_semantics_mismatch` | High |
| `FR-P2C-005-T08` | Disk-failure bypass | AOF/RDB disk error state does not gate writes | `C13`, `I09` | Deterministic write deny (`MISCONF` class) | Same | `FR-P2C-005-U009` | `FR-P2C-005-E009` | `persist.disk_error_write_denied` | Critical |
| `FR-P2C-005-T09` | Policy downgrade abuse | Hardened mode applies non-allowlisted replay repair | `C14`, `I10` | N/A (strict fail-closed baseline) | Reject non-allowlisted deviation | `FR-P2C-005-U012` | `FR-P2C-005-E012` | `persist.hardened_repair_policy_violation`, `persist.hardened_nonallowlisted_rejected` | Critical |
| `FR-P2C-005-T10` | Propagation integrity drift | Replay-visible command sequence diverges from propagated order | `C03`, `I01` | Preserve command/`SELECT` order exactly | Same | `FR-P2C-005-U003` | `FR-P2C-005-E003` | `persist.propagation.command_order_violation`, `persist.propagation.select_boundary_mismatch` | High |

## Fail-closed rules

1. Manifest parse/validation ambiguity is fatal (`C08`).
2. Non-final AOF segment corruption is always fatal (`C07`).
3. RDB checksum mismatch is fatal when validation is enabled (`C10`).
4. Any non-allowlisted hardened deviation is rejected (`C14`).
5. Disk-error write gating cannot be bypassed once active (`C13`).

## Audit-log requirements

All threat detections/rejections/recoveries must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-005`
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Expected-loss decision model

### States

- `S0`: contract-preserving persistence/replay operation
- `S1`: recoverable bounded condition (allowlisted repair/diagnostics)
- `S2`: unsafe tamper/drift condition

### Actions

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense with evidence emission
- `A2`: fail closed and abort apply/replay

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial persistence suite `< 1%`.
- Fallback trigger: unresolved strict-mode drift on critical rows (`C01`, `C07`, `C10`, `C13`, `C14`) blocks packet promotion.

## Replay commands

- Unit threat suite: `cargo test -p fr-persist -- --nocapture FR_P2C_005`
- E2E threat suite: `cargo test -p fr-conformance -- --nocapture FR_P2C_005`

## Residual risks

- Rust persistence subsystem is still skeletal; threat coverage is contractual until implementation beads land.
- Manifest lifecycle + rewrite orchestration has high race-risk until end-to-end tests cover rotate/replay/failure permutations.
