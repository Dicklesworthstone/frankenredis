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

- Unit: `cargo test -p fr-protocol -- --nocapture FR_P2C_002`
- Integration/E2E: `cargo test -p fr-conformance -- --nocapture FR_P2C_002`

## Traceability checklist

- Every row `C01..C10` maps to at least one unit and one e2e ID.
- Every row has at least one deterministic `reason_code`.
- Every row defines strict/hardened behavior plus fail-closed boundary.
