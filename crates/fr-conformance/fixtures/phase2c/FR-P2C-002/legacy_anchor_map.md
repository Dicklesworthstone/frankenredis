# FR-P2C-002 Legacy Anchor Map

Packet: `FR-P2C-002`  
Subsystem: RESP parser contract  
Target crate: `crates/fr-protocol`  
Prepared by: `PeachKnoll`  
Source baseline: `legacy_redis_code/redis/src`

## Scope and intent

This artifact extracts line-anchored RESP parsing semantics and callback-driven
collection behavior from Redis legacy code, then maps them to planned
FrankenRedis verification rows for normal, edge, and adversarial paths.

## Legacy anchor map

| Anchor ID | Legacy anchor | Extracted behavior contract | Current Rust coverage |
|---|---|---|---|
| `FR-P2C-002-A01` | `legacy_redis_code/redis/src/resp_parser.c:10-35` | Parser is callback-driven and explicitly intended for trusted Redis-generated replies; caller continues collection parsing recursively. | Partial; `fr-protocol` uses direct AST parse, not callback parser. |
| `FR-P2C-002-A02` | `legacy_redis_code/redis/src/resp_parser.h:17-64` | Contract surface includes RESP2 and RESP3 callbacks (`$`, `+`, `-`, `:`, `*`, `~`, `%`, `#`, `,`, `_`, `(`, `=`, `|`). | Partial; only RESP2 frame types implemented in `fr-protocol`. |
| `FR-P2C-002-A03` | `legacy_redis_code/redis/src/resp_parser.c:42-59` | Bulk string parse: parse length, handle null bulk (`$-1`), consume payload and trailing CRLF. | Implemented for RESP2 in `crates/fr-protocol/src/lib.rs:129-154`. |
| `FR-P2C-002-A04` | `legacy_redis_code/redis/src/resp_parser.c:61-85` | Simple string, error, integer all consume up to CRLF and emit typed callbacks. | Implemented for RESP2 in `crates/fr-protocol/src/lib.rs:100-122`. |
| `FR-P2C-002-A05` | `legacy_redis_code/redis/src/resp_parser.c:153-166` | Array parse supports null array (`*-1`) and callback-based recursive continuation. | Implemented for RESP2 array recursion in `crates/fr-protocol/src/lib.rs:156-176`. |
| `FR-P2C-002-A06` | `legacy_redis_code/redis/src/resp_parser.c:168-188` | RESP3 set (`~`) and map (`%`) are first-class parse entry points. | Missing in `fr-protocol`. |
| `FR-P2C-002-A07` | `legacy_redis_code/redis/src/resp_parser.c:87-109`, `111-151` | RESP3 attribute (`|`), verbatim string (`=`), big number (`(`), null (`_`), double (`,`), bool (`#`) parse paths are supported. | Missing in `fr-protocol`. |
| `FR-P2C-002-A08` | `legacy_redis_code/redis/src/resp_parser.c:191-209` | Top-level dispatch is prefix-driven; unknown prefix calls parser error callback and returns `C_ERR`. | Equivalent unknown-prefix reject exists in `crates/fr-protocol/src/lib.rs:125-126`. |
| `FR-P2C-002-A09` | `legacy_redis_code/redis/src/call_reply.c:123-139`, `240-269` | Collection parsing in module call replies recursively invokes `parseReply` and computes proto span from updated parser cursor. | Missing equivalent callback-mode API in `fr-protocol`. |
| `FR-P2C-002-A10` | `legacy_redis_code/redis/src/call_reply.c:141-159` | Attribute parsing consumes metadata pairs, then resumes parsing the wrapped reply. | Missing in `fr-protocol`. |
| `FR-P2C-002-A11` | `legacy_redis_code/redis/src/script_lua.c:198-220`, `328-413` | Lua bridge recursively parses map/set/array and intentionally ignores attribute payload by parsing through it. | Missing in `fr-protocol`/runtime bridges. |
| `FR-P2C-002-A12` | `crates/fr-protocol/src/lib.rs:67-190` | FrankenRedis parser currently validates UTF-8/integer/length and incomplete frames; supports RESP2 only. | Present baseline; RESP3 and callback parity gaps remain. |

## Behavior extraction ledger

| Scenario ID | Path class | Trigger | Observable contract | Planned unit test ID | Planned e2e scenario ID | Required `reason_code` on failure |
|---|---|---|---|---|---|---|
| `FR-P2C-002-B01` | Normal | Parse RESP2 simple/integer/error frames | Typed decode and exact consumed length across CRLF boundaries. | `FR-P2C-002-U001` | `FR-P2C-002-E001` | `protocol.scalar_decode_mismatch` |
| `FR-P2C-002-B02` | Normal | Parse bulk string and null bulk | `$-1` yields null; positive length yields exact payload and cursor advancement. | `FR-P2C-002-U002` | `FR-P2C-002-E002` | `protocol.bulk_decode_mismatch` |
| `FR-P2C-002-B03` | Normal | Parse nested arrays with mixed RESP2 elements | Recursive parse preserves element order and full byte consumption. | `FR-P2C-002-U003` | `FR-P2C-002-E003` | `protocol.array_recursion_mismatch` |
| `FR-P2C-002-B04` | Edge | Truncated payload or missing CRLF | Parser must return incomplete/error and never fabricate frames. | `FR-P2C-002-U004` | `FR-P2C-002-E004` | `protocol.incomplete_frame_detected` |
| `FR-P2C-002-B05` | Edge | Invalid bulk/multibulk length encoding | Reject invalid lengths deterministically. | `FR-P2C-002-U005` | `FR-P2C-002-E005` | `protocol.invalid_length_rejected` |
| `FR-P2C-002-B06` | Edge | Unknown RESP prefix byte | Reject with deterministic invalid-prefix error path. | `FR-P2C-002-U006` | `FR-P2C-002-E006` | `protocol.invalid_prefix_rejected` |
| `FR-P2C-002-B07` | Adversarial | RESP3 set/map/attribute frames on strict parser path | Strict mode must fail closed until RESP3 contract support is implemented. | `FR-P2C-002-U007` | `FR-P2C-002-E007` | `protocol.resp3_unimplemented_fail_closed` |
| `FR-P2C-002-B08` | Adversarial | Attribute wrapper with nested payload | Parser must not desynchronize cursor; attribute handling must preserve wrapped reply boundaries. | `FR-P2C-002-U008` | `FR-P2C-002-E008` | `protocol.attribute_cursor_drift` |

## Traceability requirements for downstream beads

For all `FR-P2C-002-U*` and `FR-P2C-002-E*` tests, logs must include:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-002`)
- `mode` (`strict` or `hardened`)
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code`
- `replay_cmd`
- `artifact_refs`

Suggested suite IDs:

- Unit/property: `fr_protocol_phase2c_packet_002`
- E2E/integration: `fr_runtime_phase2c_packet_002`

## Sequencing boundary notes

- This artifact provides extraction and behavior mapping only.
- `bd-2wb.13.2` must convert this into machine-checkable contract rows.
- `bd-2wb.13.3+` will carry threat model, implementation plan, and validation artifacts.

## Confidence notes

- High confidence for RESP2 path anchors and dispatch behavior (direct line extraction).
- High confidence for RESP3 callback surface existence and current FrankenRedis coverage gaps.
- Medium-high confidence for collection/attribute cursor semantics due cross-file parser consumer behavior (`call_reply.c`, `script_lua.c`).
