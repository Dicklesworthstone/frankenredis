# FR-P2C-002 Contract Table

Packet: `FR-P2C-002`  
Subsystem: RESP parser contract  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-002/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: parse input condition.
- `preconditions`: parser state assumptions.
- `strict_contract`: legacy-compatible observable behavior.
- `hardened_contract`: bounded defensive behavior allowed without outward contract drift.
- `fail_closed_boundary`: mandatory hard-failure boundary.
- `unit_trace` / `e2e_trace`: required verification linkage.
- `reason_codes`: required diagnostics on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-002-C01` | Parse RESP2 scalar prefixes (`+`, `-`, `:`) | Input has full CRLF-terminated scalar frame | Decode type and payload exactly; consumed length matches frame bytes | Same; may emit additional deterministic diagnostics | Scalar prefix accepted without valid CRLF boundary | `FR-P2C-002-U001` | `FR-P2C-002-E001` | `protocol.scalar_decode_mismatch`, `protocol.scalar_missing_crlf` |
| `FR-P2C-002-C02` | Parse bulk string (`$`) with positive length | Length parse succeeds and payload bytes present | Produce exact payload bytes and advance cursor by `len + CRLF` | Same | Length underflow/overflow or short payload must reject | `FR-P2C-002-U002` | `FR-P2C-002-E002` | `protocol.bulk_decode_mismatch`, `protocol.bulk_truncated_payload` |
| `FR-P2C-002-C03` | Parse null bulk (`$-1`) and null array (`*-1`) | Prefix and length line parsed | Emit null frame semantics with correct cursor advancement | Same | Any non-canonical null encoding accepted as null | `FR-P2C-002-U010` | `FR-P2C-002-E010` | `protocol.null_semantics_drift` |
| `FR-P2C-002-C04` | Parse array (`*`) recursion | Nested frame bytes complete | Recursively decode elements in order and preserve nesting structure | Same | Partial recursive decode returned as success | `FR-P2C-002-U003` | `FR-P2C-002-E003` | `protocol.array_recursion_mismatch`, `protocol.array_partial_accept` |
| `FR-P2C-002-C05` | Invalid length encoding for bulk/multibulk | Prefix is `$` or `*` but length malformed | Reject deterministically as invalid length class | Same | Malformed length interpreted as value | `FR-P2C-002-U005` | `FR-P2C-002-E005` | `protocol.invalid_length_rejected` |
| `FR-P2C-002-C06` | Unknown type prefix byte | Prefix not in supported set | Reject deterministically as invalid prefix | Same | Unknown prefix parsed as valid frame | `FR-P2C-002-U006` | `FR-P2C-002-E006` | `protocol.invalid_prefix_rejected` |
| `FR-P2C-002-C07` | Input truncation / incomplete frames | Missing bytes for line, payload, or nested element | Return incomplete/error without fabricating partial frames | Same | Incomplete input accepted as complete frame | `FR-P2C-002-U004` | `FR-P2C-002-E004` | `protocol.incomplete_frame_detected` |
| `FR-P2C-002-C08` | RESP3 set/map/attribute/verbatim/big-number/bool/double/null-simple input | Strict RESP2 parser path | Strict mode must fail closed for unsupported RESP3 surface until implemented | Hardened mode may only add bounded diagnostics; still fail closed for unimplemented types | Unsupported RESP3 type parsed as supported RESP2 frame | `FR-P2C-002-U007` | `FR-P2C-002-E007` | `protocol.resp3_unimplemented_fail_closed` |
| `FR-P2C-002-C09` | Attribute wrapper plus nested reply | RESP3 attribute semantics exercised in parser-consumer path | Cursor progression must remain aligned across metadata pairs and wrapped reply | Same | Cursor drift after attribute parse | `FR-P2C-002-U008` | `FR-P2C-002-E008` | `protocol.attribute_cursor_drift` |
| `FR-P2C-002-C10` | Parser consumed length reporting | Any successful parse | `consumed` byte count must equal exact parsed frame span | Same | Success with incorrect consumed length | `FR-P2C-002-U009` | `FR-P2C-002-E009` | `protocol.consumed_length_mismatch` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-002-I01` | Prefix dispatch determinism | Required | Required |
| `FR-P2C-002-I02` | Length validation semantics | Required | Required |
| `FR-P2C-002-I03` | Incomplete-frame rejection | Required | Required |
| `FR-P2C-002-I04` | Consumed-length correctness | Required | Required |
| `FR-P2C-002-I05` | Unsupported RESP3 handling | Fail closed | Fail closed + bounded diagnostics only |
| `FR-P2C-002-I06` | Recursive array element ordering | Required | Required |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer reason-codes and deterministic forensic detail.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-002

All parser contract mismatches or reject paths must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-002`)
- `mode`
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

## Replay command templates

- Strict unit replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-protocol -- --nocapture fr_p2c_002_u001_scalar_decode_parity`
- Hardened unit/property replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_002_f_adversarial_runtime_parse_failures_emit_stable_reason_code`
- E2E replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_002_e2e_contract_smoke`

## Traceability checklist

- Every row `C01..C10` maps to at least one unit and one e2e ID.
- Every row has at least one deterministic `reason_code`.
- Every row defines strict/hardened behavior plus fail-closed boundary.

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

- `LEV-002-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

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
