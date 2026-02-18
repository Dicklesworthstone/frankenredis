# FR-P2C-005 Contract Table

Packet: `FR-P2C-005`  
Subsystem: persistence format and replay  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic persistence/replay event.
- `preconditions`: required state before contract evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior that preserves API/ordering contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-005-C01` | Startup replay with BASE + INCR manifest chain | Manifest parse succeeded; files present | Replay order is exactly `BASE -> INCR[0..n]`; no file reordering or skipping | Same external order; additional forensic logging allowed | Any manifest-chain gap or out-of-order replay attempt | `FR-P2C-005-U001` | `FR-P2C-005-E001` | `persist.replay.manifest_order_mismatch`, `persist.replay.chain_gap_detected` |
| `FR-P2C-005-C02` | AOF replay file starts with RDB signature | Loader opened file; cursor at offset 0 | Consume RDB preamble fully, then resume AOF tail replay without cursor drift | Same; bounded diagnostics allowed | RDB/AOF boundary ambiguity or mixed cursor state | `FR-P2C-005-U002` | `FR-P2C-005-E002` | `persist.replay.rdb_preamble_tail_desync` |
| `FR-P2C-005-C03` | Command propagation path appends write command | Propagation target includes AOF; db context known | Replay-visible command sequence preserves `SELECT` boundaries and command order | Same; internal batching permitted only if outward order unchanged | Reordered or dropped replay commands | `FR-P2C-005-U003` | `FR-P2C-005-E003` | `persist.propagation.select_boundary_mismatch`, `persist.propagation.command_order_violation` |
| `FR-P2C-005-C04` | Replay parser reads RESP AOF command frames | Input line and bulk framing present | Parse `*argc` + `$len` frames exactly and execute sequentially via replay client context | Same; diagnostics may be richer | Frame accepted with malformed arity/length semantics | `FR-P2C-005-U013` | `FR-P2C-005-E013` | `persist.replay.frame_parse_invalid`, `persist.replay.frame_length_violation` |
| `FR-P2C-005-C05` | EOF reached while inside `MULTI` during replay | `valid_before_multi` checkpoint available | Rewind to checkpoint and discard incomplete transaction effects | Same | Partial transaction applied after incomplete `MULTI/EXEC` | `FR-P2C-005-U006` | `FR-P2C-005-E006` | `persist.replay.incomplete_multi_rollback` |
| `FR-P2C-005-C06` | Final INCR AOF tail is truncated/corrupt | Recovery policy flag enabled and tail within configured bound | Recover by truncating to last valid command offset only for final file | May apply bounded replay repair (`BoundedReplayRepair`) with explicit evidence record | Truncation/repair on non-final chain file | `FR-P2C-005-U004` | `FR-P2C-005-E004` | `persist.replay.tail_truncate_recover`, `persist.replay.repair_policy_applied` |
| `FR-P2C-005-C07` | Non-final AOF segment is truncated/corrupt | Manifest has additional segments after damaged one | Hard fail; no continuation into later files | Hard fail (same) | Any continuation after damaged non-final segment | `FR-P2C-005-U005` | `FR-P2C-005-E005` | `persist.replay.nonfinal_truncation_fatal` |
| `FR-P2C-005-C08` | Manifest parse/validation | Manifest line parsed into key-value pairs | Reject malformed format, duplicate base, and path-style filenames | Same; optional metadata diagnostics only | Path traversal filename or schema-ambiguous record accepted | `FR-P2C-005-U007` | `FR-P2C-005-E007` | `persist.manifest.parse_or_path_violation` |
| `FR-P2C-005-C09` | RDB load from disk or preamble path | Valid RDB header version and opcode stream | Handle required opcodes (`SELECTDB`, `RESIZEDB`, expiry, AUX) and ignore unknown AUX safely | Same; bounded metadata sanitization only when contract-preserving | Unknown mandatory opcode treated as benign data | `FR-P2C-005-U014` | `FR-P2C-005-E014` | `persist.rdb.opcode_contract_violation`, `persist.rdb.aux_handling_invalid` |
| `FR-P2C-005-C10` | RDB checksum validation | RDB version supports checksum and checksum check enabled | CRC mismatch is fatal; corrupted snapshot not activated | Same | Checksum mismatch accepted as success | `FR-P2C-005-U008` | `FR-P2C-005-E008` | `persist.rdb.checksum_or_format_invalid` |
| `FR-P2C-005-C11` | Replica full-sync transitions persistence state | Replica entering inbound RDB load path | Stop AOF before load, restart only after successful sync finalization | Same | AOF remains active during inbound full-load apply | `FR-P2C-005-U010` | `FR-P2C-005-E010` | `persist.replication.sync_aof_state_violation` |
| `FR-P2C-005-C12` | `WAITAOF` block/unblock check | AOF enabled for local ack; replica links active | Unblock only when local fsync + replica AOF ack thresholds satisfy request | Same | Unblock with unmet local or replica thresholds | `FR-P2C-005-U011` | `FR-P2C-005-E011` | `persist.waitaof_ack_semantics_mismatch` |
| `FR-P2C-005-C13` | AOF/RDB disk error surfaces during writable state | Persistence enabled and write path active | Deterministic write-deny behavior is applied (`MISCONF`-class failure) | Same | Write accepted while disk-error deny condition active | `FR-P2C-005-U009` | `FR-P2C-005-E009` | `persist.disk_error_write_denied` |
| `FR-P2C-005-C14` | Hardened-mode replay repair candidate outside allowlist | Mode = hardened; candidate deviation category unresolved | Strict-equivalent fail-closed unless deviation is explicitly allowlisted | Only allowlisted bounded repair paths may proceed; must emit policy evidence | Non-allowlisted deviation applied | `FR-P2C-005-U012` | `FR-P2C-005-E012` | `persist.hardened_repair_policy_violation`, `persist.hardened_nonallowlisted_rejected` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-005-I01` | Manifest replay ordering | Required | Required |
| `FR-P2C-005-I02` | RDB preamble + AOF tail cursor correctness | Required | Required |
| `FR-P2C-005-I03` | MULTI replay atomic rollback | Required | Required |
| `FR-P2C-005-I04` | Non-final segment corruption handling | Fail closed | Fail closed |
| `FR-P2C-005-I05` | Final-segment recovery scope | Config-bounded, legacy-compatible | Config-bounded + explicit evidence emission |
| `FR-P2C-005-I06` | RDB checksum enforcement | Required when enabled | Required when enabled |
| `FR-P2C-005-I07` | Sync-time AOF stop/restart ordering | Required | Required |
| `FR-P2C-005-I08` | `WAITAOF` ack semantics | Required | Required |
| `FR-P2C-005-I09` | Disk-error write denial | Required | Required |
| `FR-P2C-005-I10` | Non-allowlisted hardened deviations | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedReplayRepair`: deterministic tail repair within explicit configured bounds.
- `BoundedParserDiagnostics`: additional diagnostics with no outward contract drift.
- `MetadataSanitization`: bounded handling of malformed metadata that preserves replay contract.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-005 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-005`)
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

- Strict unit/property replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_differential_replay_fixture_passes`
- Hardened unit/property replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_005_f_adversarial_decode_reason_taxonomy_is_stable`
- E2E replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_005_e2e_contract_smoke`

## Implemented final evidence pack (bd-2wb.16.9)

- Packet final manifest + parity gate authored in `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/{fixture_manifest.json,parity_gate.yaml}`.
- Packet parity report finalized in `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/parity_report.json` with:
  - `readiness=READY_FOR_IMPL`
  - `missing_mandatory_fields=[]`
  - explicit unit/differential/e2e/optimization evidence IDs.
- Durability sidecar + decode-proof artifacts finalized in:
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/parity_report.raptorq.json`
  - `crates/fr-conformance/fixtures/phase2c/FR-P2C-005/parity_report.decode_proof.json`
  - reason code: `raptorq.decode_verified`
  - replay command: `./scripts/run_raptorq_artifact_gate.sh --run-id local-smoke`
- Packet schema/readiness replay:
  - `rch exec -- cargo run -p fr-conformance --bin phase2c_schema_gate -- crates/fr-conformance/fixtures/phase2c/FR-P2C-005`

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.

## Alien recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `fr-conformance.phase2c-gate.dir-scan-mask.v1` |
| `evidence_id` | `evidence.phase2c-gate.round_dir_scan_mask.v1` |
| Priority tier | `A` |
| EV score | `2.61` |
| Baseline comparator | `round_sort_prune + current HEAD pre-change` |
| Hotspot evidence | `statx calls 2911 -> 991`, syscall share `30.17% -> 10.76%`, output checksum unchanged |
| Graveyard mapping | `metadata-bound IO overhead -> layout-aware preindexing` |
| Adoption wedge | single-module behavior-isomorphic syscall reduction in Phase2C gate path |
| Budgeted-mode default | one optimization lever per round, then mandatory re-profile |

## Expected-loss decision model (optimization lever)

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

Calibration + fallback trigger:

- If `delta_percent <= 0` over the 20-run benchmark window, reject/revert the lever.
- If output checksum diverges, fail closed and reject promotion.
- Exhaustion behavior: stop after one lever and re-profile before further optimization.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-005-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

Required artifacts:

- Baseline/profile evidence: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/baseline_hyperfine.json`
- Hotspot syscall profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/baseline_strace.txt`
- Chosen lever note: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/optimization_report.md`
- Post-change re-profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/after_hyperfine.json`
- Post-change syscall profile: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/after_strace.txt`
- Behavior-isomorphism proof: `artifacts/optimization/phase2c-gate/round_dir_scan_mask/isomorphism_check.txt`

## Reproducibility/provenance pack references

- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/env.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/manifest.json`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/repro.lock`
- `artifacts/optimization/phase2c-gate/round_dir_scan_mask/LEGAL.md` (required when IP/provenance risk is plausible)
