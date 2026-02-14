# FrankenRedis Security/Compatibility Threat Matrix v1

Date: 2026-02-14
Owner bead: `bd-2wb.2`
Status: Working baseline for packet implementation gates

This document encodes the frankenlibc/frankenfs-style doctrine for FrankenRedis:
- strict mode maximizes observable Redis compatibility.
- hardened mode preserves outward contract while permitting only bounded, allowlisted defenses.
- unknown or incompatible paths fail closed.

## 1. Security-Compatibility Contract

### 1.1 Strict mode

- No behavior-altering repair.
- Unknown protocol/config/persistence feature => fail closed.
- Observable replies, side effects, and ordering must remain parity-compatible for full target APIs.

### 1.2 Hardened mode

- Only allowlisted deviation categories are legal.
- Deviations must remain API-contract compatible.
- Every deviation must emit deterministic evidence events and replay metadata.

Hardened categories are represented via `HardenedDeviationCategory` in `crates/fr-config/src/lib.rs`.

## 2. Decision-Theoretic Runtime Policy

For security-compatibility conflicts, runtime policy uses expected-loss minimization.

| Condition | Action | Expected loss (normalized) | Rationale |
| --- | --- | --- | --- |
| Compatibility uncertain, low exploit risk | Preserve strict legacy behavior | 0.25 | Avoid user-visible drift |
| Compatibility uncertain, high exploit risk | Fail closed + reasoned diagnostic | 0.35 | Safer default under uncertainty |
| Exploit signal high, compatibility known | Harden with allowlisted bounded defense | 0.30 | Preserve contract while containing blast radius |
| Unknown non-allowlisted hardened behavior needed | Reject path | 0.15 | Prevent silent policy drift |

Fail-safe default:
- If risk model is under-specified or calibration confidence is low, choose fail-closed action.

## 3. Packet-Level Compatibility/Security Surfaces

| Packet | Primary attack surface | Compatibility-critical invariants | Hardened-only bounded defenses |
| --- | --- | --- | --- |
| FR-P2C-001 Event loop core | scheduler starvation, timing abuse | dispatch ordering invariants for parity-target workflows | bounded diagnostics only (no scheduling semantic mutation) |
| FR-P2C-002 RESP parser | malformed frame trees, length abuse | parse/reply contract parity | parser diagnostics and bounded rejection context |
| FR-P2C-003 Dispatch | command confusion, malformed argument vectors | command routing and error semantics | bounded validation diagnostics |
| FR-P2C-004 ACL/auth | selector confusion, rule bypass attempts | authorization decision parity | explicit denial diagnostics; no permissive fallback |
| FR-P2C-005 Persistence/replay | tampered artifacts, partial writes | replay ordering parity and fail behavior | bounded replay repair with decode proof only |
| FR-P2C-006 Replication | order/replay injection, stale stream replay | replication order semantics | reject + forensic metadata emission |
| FR-P2C-007 Cluster | slot/routing abuse in cluster paths | route/slot semantics for parity-target cluster behavior | bounded diagnostics for malformed/out-of-contract paths |
| FR-P2C-008 Expire/evict | resource exhaustion and cardinality spikes | TTL/eviction observable semantics | deterministic resource clamp with explicit reason code |
| FR-P2C-009 TLS/config | invalid or conflicting config graphs | startup/config accept-reject semantics | metadata sanitization on allowlisted fields only |

## 4. Threat Matrix

| Threat class | Example attack/failure | Strict mode | Hardened mode | Required evidence |
| --- | --- | --- | --- | --- |
| parser abuse | malformed RESP lengths, nested frame bombs | reject frame, fail closed | bounded parser diagnostics only | parse rejection event with reason + frame digest |
| metadata ambiguity | malformed packet metadata or sidecars | block packet and gate implementation | bounded metadata sanitization (allowlisted) | compatibility incident + sanitized-field ledger |
| version skew | schema/protocol version mismatch | block incompatible artifact | block incompatible artifact | version drift report + gate decision ledger |
| resource exhaustion | oversized arrays/bulk payloads, abusive cardinality | fail closed at compatibility gate | deterministic resource clamp (allowlisted) | clamp threshold event + replay token |
| persistence tampering | partial/invalid persistence artifacts, replay hazards | stop replay path and block implementation | bounded replay repair (allowlisted) with decode proof | repair ledger + decode proof + risk note |
| replication/order attacks | out-of-order/replayed replication events | reject and block promotion path | reject and emit forensic context | ordering violation event + replication audit trail |
| auth policy confusion | conflicting ACL selectors or malformed grants | deny path, fail closed | deny path with policy trace | ACL decision ledger + selector trace |
| config downgrade abuse | insecure fallback request on startup/reload | reject downgrade | reject downgrade + explain policy code | config decision log + replay command |

## 5. Hardened Deviation Allowlist (Exclusive)

Only these are valid in hardened mode:
- `BoundedParserDiagnostics`
- `BoundedReplayRepair`
- `ResourceClamp`
- `MetadataSanitization`

Hard constraints:
- No allowlisted category may alter externally visible command semantics.
- Any non-allowlisted deviation is an automatic fail-closed condition.

## 6. Compatibility Drift Gates

For each packet, acceptance requires:
- Strict-mode differential drift = `0` for parity-target behavior.
- Hardened-mode differences are explicitly allowlisted and evidence-linked.
- No unclassified divergence remains.

Drift severity:
- `S0`: strict parity violation on critical path (release blocker).
- `S1`: strict parity violation on non-critical parity-target path (blocker).
- `S2`: hardened-mode unallowlisted divergence (blocker).
- `S3`: evidence/log schema omission (blocker for sign-off).

## 7. Structured Forensic Logging Contract

Every threat-handling event must log:
- `ts_utc`
- `packet_id`
- `mode`
- `threat_class`
- `decision_action`
- `reason_code`
- `input_digest`
- `output_digest`
- `state_digest_before`
- `state_digest_after`
- `replay_cmd`
- `artifact_refs`

Log invariants:
- deterministic serialization order
- stable field presence for machine parsing
- replay command must reconstruct failure path without manual edits

## 8. Verification Matrix (Unit + E2E + Differential)

| Threat class | Unit/property obligations | E2E obligations | Differential obligations |
| --- | --- | --- | --- |
| parser abuse | malformed frame corpus, parser state invariants | malformed session scripts with deterministic seeds | strict parity on valid sub-cases, deterministic fail on malformed |
| metadata ambiguity | metadata schema/property guards | artifact ingestion failure workflows | no unallowlisted hardened drift |
| version skew | compatibility version gate checks | startup/reload incompatible artifact scenarios | explicit gate outcome classification |
| resource exhaustion | boundary cardinality/property checks | sustained-load scripts with clamp thresholds | strict mode fail-closed parity |
| persistence tampering | decode/repair invariant checks | crash/restart + tampered artifact replay | replay ordering parity on valid artifacts |
| replication/order attacks | sequence/order invariants | primary/replica fault-injection scripts | stream-order parity and reject semantics |
| auth policy confusion | ACL selector precedence invariants | login/command denial journeys | deny semantics parity |
| config downgrade abuse | config graph invariants | startup/reload downgrade attempts | strict parity on reject behavior |

## 9. RaptorQ-Everywhere Outputs

Long-lived security/compatibility artifacts requiring sidecars:
- threat-model manifests
- compatibility drift reports
- adversarial fixture bundles
- replay evidence ledgers

Each recovery event must include:
- repair-symbol generation manifest
- integrity scrub report
- decode proof artifact

## 10. Implementation Exit Criteria

This matrix is implementation-ready for a packet only when:
- packet row in Section 3 has no unresolved ambiguity notes
- threat classes in Section 4 are mapped to concrete test IDs
- log schema in Section 7 is wired in both unit and e2e flows
- drift gates in Section 6 are connected to CI gate outputs
- all hardened deviations are allowlisted and claim/evidence linked
