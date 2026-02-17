# FR-P2C-002 Rust Implementation Plan

Packet: `FR-P2C-002`  
Scope: RESP parser parity backbone  
Inputs:

- `legacy_anchor_map.md`
- `contract_table.md`
- `risk_note.md`

## 1) Implementation objective

Implement Redis-compatible parser behavior for FR-P2C-002 with:

- deterministic parse/consume semantics,
- explicit strict/hardened policy boundaries,
- fail-closed behavior for unsupported/unknown protocol paths,
- traceable unit/e2e evidence for each contract/threat row.

## 2) Module boundary skeleton

### `crates/fr-protocol` (core parser crate)

Proposed decomposition:

1. `frame.rs`
   - canonical frame enum(s), including staged RESP3 extensions
2. `decode.rs`
   - prefix dispatch and recursive parse state
3. `decode_resp2.rs`
   - RESP2 scalar/bulk/array logic
4. `decode_resp3.rs`
   - staged RESP3 support (initially fail-closed gates for unsupported families)
5. `cursor.rs`
   - byte cursor/consumed accounting and bounds helpers
6. `errors.rs`
   - parse error taxonomy mapped to contract reason codes
7. `encode.rs`
   - canonical wire encoding

### `crates/fr-runtime` (integration seam)

1. parser error to response mapping
2. strict/hardened policy usage for parser decisions
3. structured evidence emission for parser rejects

### `crates/fr-config` (policy seam)

1. allowlisted parser deviation categories:
   - `BoundedParserDiagnostics`
2. reject path for non-allowlisted parser drift

### `crates/fr-conformance` (verification seam)

1. packet-level parser fixtures for normal/edge/adversarial cases
2. contract assertions (`C01..C10`)
3. threat assertions (`T01..T08`)

## 3) Data model invariants

1. Prefix dispatch invariant (`I01`): deterministic type mapping.
2. Length semantics invariant (`I02`): invalid lengths rejected deterministically.
3. Incomplete-frame invariant (`I03`): no synthetic success on truncated bytes.
4. Consumed-byte invariant (`I04`): parsed span exactly tracked.
5. Unsupported-RESP3 invariant (`I05`): fail-closed until explicitly implemented.
6. Recursive ordering invariant (`I06`): nested element order preserved.

## 4) Error taxonomy (packet-specific)

1. `RespParseError::InvalidPrefix`
2. `RespParseError::InvalidInteger`
3. `RespParseError::InvalidBulkLength`
4. `RespParseError::InvalidMultibulkLength`
5. `RespParseError::Incomplete`
6. `RespParseError::InvalidUtf8`
7. `RespParseError::UnsupportedResp3Type` (planned extension for explicit fail-closed rows)

Each error must map to row-level `reason_code` requirements from `contract_table.md`.

## 5) Staged implementation sequence

1. **Stage D1**: lock RESP2 parser invariants (`C01..C07`, `C10`) with exhaustive unit cases.
2. **Stage D2**: introduce explicit unsupported-RESP3 fail-closed handling (`C08`).
3. **Stage D3**: harden consumed-length and cursor drift assertions (`C09`, `C10`).
4. **Stage D4**: runtime integration for parser diagnostics and policy gates.
5. **Stage D5**: conformance packet fixture wiring and adversarial suite.
6. **Stage D6**: staged RESP3 implementation (when unblocked) with invariant-preserving tests.

## 6) Unit/property test matrix

| Test ID | Contract rows | Threat IDs | Type | Expected result |
|---|---|---|---|---|
| `FR-P2C-002-U001` | `C01` | - | unit | scalar decode parity |
| `FR-P2C-002-U002` | `C02` | - | unit | bulk decode parity |
| `FR-P2C-002-U003` | `C04` | - | unit | nested array recursion parity |
| `FR-P2C-002-U004` | `C07` | `T01` | adversarial unit | truncated frame rejection |
| `FR-P2C-002-U005` | `C05` | `T02` | adversarial unit | malformed length rejection |
| `FR-P2C-002-U006` | `C06` | `T03` | adversarial unit | invalid prefix rejection |
| `FR-P2C-002-U007` | `C08` | `T03` | adversarial unit | unsupported RESP3 fail-closed |
| `FR-P2C-002-U008` | `C09` | `T04` | unit | attribute/cursor alignment checks |
| `FR-P2C-002-U009` | `C10` | `T04` | unit | consumed-length exactness |
| `FR-P2C-002-U010` | `C03` | `T05` | unit | null semantics parity |
| `FR-P2C-002-U011` | `C01` | `T06` | adversarial unit | invalid UTF-8 behavior consistency |
| `FR-P2C-002-U012` | `C04`,`C07` | `T07` | adversarial unit | depth/size clamp behavior |
| `FR-P2C-002-U013` | `I05` | `T08` | policy unit | hardened drift non-allowlist rejection |

## 7) E2E scenario matrix

| Scenario ID | Contract rows | Threat IDs | Expected result |
|---|---|---|---|
| `FR-P2C-002-E001` | `C01` | - | scalar parser parity through runtime |
| `FR-P2C-002-E002` | `C02` | - | bulk parser parity through runtime |
| `FR-P2C-002-E003` | `C04` | - | nested arrays through runtime |
| `FR-P2C-002-E004` | `C07` | `T01` | truncated input reject behavior |
| `FR-P2C-002-E005` | `C05` | `T02` | malformed length reject behavior |
| `FR-P2C-002-E006` | `C06` | `T03` | unknown prefix reject behavior |
| `FR-P2C-002-E007` | `C08` | `T03` | unsupported RESP3 fail-closed behavior |
| `FR-P2C-002-E008` | `C09` | `T04` | attribute cursor/no-desync behavior |
| `FR-P2C-002-E009` | `C10` | `T04` | consumed length parity checks |
| `FR-P2C-002-E010` | `C03` | `T05` | null behavior parity |
| `FR-P2C-002-E011` | `C01` | `T06` | invalid UTF-8 handling via runtime |
| `FR-P2C-002-E012` | `C04`,`C07` | `T07` | deep nesting/size robustness |
| `FR-P2C-002-E013` | `I05` | `T08` | hardened policy drift gate |

## 8) Structured logging boundary interface

Parser boundaries (`dispatch`, `length-parse`, `cursor-advance`, `runtime-map`) must emit replay-complete logs with:

- `ts_utc`, `suite_id`, `test_or_scenario_id`, `packet_id`
- `mode`, `seed`, `input_digest`, `output_digest`
- `duration_ms`, `outcome`, `reason_code`
- `replay_cmd`, `artifact_refs`

## 9) Execution commands (local/CI)

Use remote offload for CPU-intensive validation:

```bash
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-protocol -- --nocapture fr_p2c_002_u001_scalar_decode_parity
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_002_f_adversarial_runtime_parse_failures_emit_stable_reason_code
FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_002_e2e_contract_smoke
rch exec -- cargo fmt --check
```

## 10) Sequencing boundary notes

- This bead defines architecture and execution sequencing only.
- Behavior-changing parser implementation proceeds in `bd-2wb.13.5+`.
- Unsupported RESP3 handling remains explicitly fail-closed until dedicated implementation beads land.

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

- `LEV-002-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

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
rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_002_f_differential_protocol_fixture_passes
```
