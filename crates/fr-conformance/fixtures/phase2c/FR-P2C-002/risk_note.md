# FR-P2C-002 Risk Note

Packet: `FR-P2C-002`  
Subsystem: RESP parser contract  
Related artifacts:

- `crates/fr-conformance/fixtures/phase2c/FR-P2C-002/legacy_anchor_map.md`
- `crates/fr-conformance/fixtures/phase2c/FR-P2C-002/contract_table.md`

## Compatibility envelope

- `strict` mode: preserve Redis-observable frame acceptance/rejection and
  parse cursor semantics for supported protocol surface.
- `hardened` mode: allow bounded parser diagnostics only; no API-visible
  semantic drift.
- Unsupported or non-allowlisted protocol behavior must be `fail_closed`.

## Threat matrix

| Threat ID | Threat class | Attack/failure vector | Contract rows at risk | Strict expected outcome | Hardened expected outcome | Unit adversarial test | E2E abuse-path test | Required reason codes | Severity |
|---|---|---|---|---|---|---|---|---|---|
| `FR-P2C-002-T01` | Parser abuse | Truncated scalar/bulk/array payloads | `C01`, `C02`, `C04`, `C07` | Reject as incomplete/error; no fabricated frame | Same, plus bounded diagnostics | `FR-P2C-002-U004` | `FR-P2C-002-E004` | `protocol.incomplete_frame_detected` | Critical |
| `FR-P2C-002-T02` | Length poisoning | Malformed, negative, or overflow lengths | `C02`, `C05` | Deterministic reject | Same | `FR-P2C-002-U005` | `FR-P2C-002-E005` | `protocol.invalid_length_rejected` | Critical |
| `FR-P2C-002-T03` | Prefix confusion | Unknown or unsupported prefix dispatch | `C06`, `C08` | Reject invalid/unsupported prefix | Same | `FR-P2C-002-U006`, `FR-P2C-002-U007` | `FR-P2C-002-E006`, `FR-P2C-002-E007` | `protocol.invalid_prefix_rejected`, `protocol.resp3_unimplemented_fail_closed` | Critical |
| `FR-P2C-002-T04` | Cursor drift | Attribute/map/set recursion desynchronizes parser cursor | `C04`, `C09`, `C10` | Cursor must remain aligned and consumed span exact | Same | `FR-P2C-002-U008`, `FR-P2C-002-U009` | `FR-P2C-002-E008`, `FR-P2C-002-E009` | `protocol.attribute_cursor_drift`, `protocol.consumed_length_mismatch` | High |
| `FR-P2C-002-T05` | Null-semantics drift | Non-canonical null representations accepted | `C03` | Only canonical null encodings accepted | Same | `FR-P2C-002-U010` | `FR-P2C-002-E010` | `protocol.null_semantics_drift` | High |
| `FR-P2C-002-T06` | UTF-8 ambiguity | Invalid UTF-8 in scalar payload path | `C01` | Deterministic UTF-8 rejection semantics preserved | Same | `FR-P2C-002-U011` | `FR-P2C-002-E011` | `protocol.scalar_decode_mismatch` | Medium |
| `FR-P2C-002-T07` | Resource exhaustion | Deep nesting or large payload stress | `C04`, `C07`, `C10` | Reject or bound deterministically; no undefined behavior | Bounded resource clamp + diagnostics if allowlisted | `FR-P2C-002-U012` | `FR-P2C-002-E012` | `protocol.depth_or_size_resource_clamp` | High |
| `FR-P2C-002-T08` | Compatibility downgrade | Hardened mode silently changes accepted protocol surface | `I05`, `I06` | Strict parity behavior unchanged | Any divergence must be explicit and allowlisted; otherwise fail closed | `FR-P2C-002-U013` | `FR-P2C-002-E013` | `protocol.hardened_drift_not_allowlisted` | Critical |

## Fail-closed rules

1. Unsupported RESP3 frame families are rejected until explicitly implemented and allowlisted.
2. Invalid prefix, malformed length, and truncated frame conditions never degrade into successful parse.
3. Consumed-length mismatch on successful parse is a hard failure.
4. Any non-allowlisted hardened divergence from strict parser semantics is rejected.

## Audit-log requirements

All parser threat detections/rejections/recoveries must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` = `FR-P2C-002`
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

- `S0`: valid supported frame path
- `S1`: recoverable but suspicious parse condition
- `S2`: unsafe/unknown/unsupported protocol condition

### Actions

- `A0`: continue normal parse
- `A1`: bounded defensive handling + diagnostics
- `A2`: fail closed reject

### Loss matrix (lower is better)

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 7 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Decision policy:

- if posterior(`S2`) > `0.30`, enforce `A2` fail-closed.
- if posterior(`S1`) > `0.40` and deviation category is allowlisted, use `A1`.
- otherwise use `A0`.

## Calibration and fallback trigger

- Calibration metric: false-negative rate on adversarial parser suite `< 1%`.
- Fallback trigger: any unresolved strict-mode drift on critical parser rows (`C01`, `C02`, `C05`, `C06`, `C08`) blocks packet promotion.

## Replay commands

- Unit threat suite: `cargo test -p fr-protocol -- --nocapture FR_P2C_002`
- E2E threat suite: `cargo test -p fr-conformance -- --nocapture FR_P2C_002`

## Residual risks

- Full RESP3 parity remains open until downstream implementation beads are landed.
- Parser consumer parity (`call_reply`/Lua-bridge style recursion semantics) is high-risk until dedicated differential fixtures are in place.
