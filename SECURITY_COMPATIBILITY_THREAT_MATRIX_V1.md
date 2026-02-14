# FrankenRedis Security/Compatibility Threat Matrix v1

This document is the concrete deliverable for the frankenlibc/frankenfs-style doctrine:

- strict mode maximizes observable Redis-compatible behavior and fails closed on incompatible inputs.
- hardened mode preserves API contract but allows only explicit, bounded defensive behaviors.

## Mode Contract

- strict mode:
  - fail closed for unknown/incompatible protocol, metadata, config, and persistence conditions.
  - no repair paths that alter externally visible semantics.
- hardened mode:
  - permits only allowlisted deviation categories.
  - each deviation must emit deterministic evidence events and bounded-recovery metadata.

Hardened allowlist categories are represented in `crates/fr-config/src/lib.rs` via `HardenedDeviationCategory`.

## Threat Matrix

| Threat class | Example attack/failure | Strict mode | Hardened mode | Required evidence |
| --- | --- | --- | --- | --- |
| parser abuse | malformed RESP lengths, nested frame bombs | reject frame, fail closed | bounded parser diagnostics only | parse rejection event with reason and frame digest |
| metadata ambiguity | malformed packet metadata or sidecar fields | block packet and gate implementation | bounded metadata sanitization (allowlisted) | compatibility gate incident + sanitized-field ledger |
| version skew | schema/protocol version mismatch | block incompatible artifact | block incompatible artifact | version drift report + gate decision ledger |
| resource exhaustion | oversized arrays/bulk payloads, abusive cardinality | fail closed at compatibility gate | deterministic resource clamp (allowlisted) | gate action, clamp threshold, replay token |
| persistence tampering/state corruption | partial/invalid persistence artifacts, replay hazards | stop replay path and block implementation | bounded replay repair (allowlisted) with decode proof | repair ledger + decode proof + risk note |
| replication/order attacks | out-of-order/replayed replication events | reject and block promotion path | reject and emit forensic context | ordering violation event and replication audit trail |

## Hardened Deviation Policy

Only the following categories are valid in hardened mode:

- `BoundedParserDiagnostics`
- `BoundedReplayRepair`
- `ResourceClamp`
- `MetadataSanitization`

Any non-allowlisted behavior is an automatic fail-closed condition.

## Verification Requirements

- unit tests:
  - strict mode rejects each threat-class fixture.
  - hardened mode only allows allowlisted categories.
- e2e scripts:
  - deterministic replay for parser abuse, metadata ambiguity, and persistence tampering.
  - structured logs must include packet ID, mode, reason code, input/output digests, and replay command.
- conformance evidence:
  - differential oracle check for observable reply/ordering compatibility.
  - parity gate outputs and decision-ledger evidence for each packet family.
