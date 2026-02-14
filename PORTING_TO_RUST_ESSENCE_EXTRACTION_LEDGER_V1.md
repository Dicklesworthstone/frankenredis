# Porting-to-Rust Essence Extraction Ledger v1

Date: 2026-02-14
Status: Draft ready for packet implementation handoff
Owner bead: `bd-2wb.1`

## 1. Purpose

This ledger is the semantic source of truth for the clean-room Rust port.
It encodes what must remain behavior-isomorphic to legacy Redis in strict mode,
what bounded hardening is allowed, and what evidence is required before changes
are accepted.

Primary objectives:
- Preserve Redis-observable replies, side effects, and ordering across the full parity target surface.
- Separate compatibility (`strict`) from defensive safety (`hardened`) without silent drift.
- Provide implementation-ready contracts and replayable verification evidence.

## 2. Decision Contract (Alien-Artifact Baseline)

For every packet-level implementation decision:
- Define state/action/loss entries before code changes.
- Select exactly one optimization lever per change set.
- Require behavior-isomorphism proof against differential oracle fixtures.
- Record evidence IDs and artifact paths for independent replay.

### 2.1 Loss model (normalized)

| State | Action | Loss if wrong | Notes |
| --- | --- | --- | --- |
| Compatibility-uncertain | Preserve legacy behavior | 0.2 | Lower operational risk |
| Compatibility-uncertain | Harden behavior | 0.8 | High risk of user-visible drift |
| Security-uncertain | Preserve behavior blindly | 0.9 | Potential exploit path |
| Security-uncertain | Fail closed + bounded diagnostics | 0.3 | Preferred default |

Default policy:
- Unknown compatibility path => fail closed in both modes.
- Hardened-only deviations must be allowlisted and evidence-linked.

## 3. Global Invariants (Non-Negotiable)

- Deterministic command semantics and reply ordering.
- Deterministic expiration/eviction ordering invariants.
- Deterministic AOF/RDB recovery ordering invariants.
- Replication stream order integrity (no hidden reordering).
- Strict mode has zero behavior-altering repairs.
- Hardened mode may only use allowlisted bounded defenses.

## 4. Packet Essence Ledger

| Packet | Legacy anchors | Core observable invariants | Undefined/ambiguous zones to resolve | Strict policy | Hardened policy | Required unit/e2e/log evidence |
| --- | --- | --- | --- | --- | --- | --- |
| FR-P2C-001 Event loop core | `src/ae.c`, `ae_epoll.c`, `ae_kqueue.c`, `ae_select.c` | Event dispatch order, timer ordering, wakeup fairness within parity-target behavior | Backend-specific edge timing and starvation corner cases | Match legacy event ordering for parity-target fixtures | Allow bounded diagnostics only; no scheduler semantic drift | Unit: event-state transitions. E2E: timing-sensitive command flows. Logs: `packet_id`, event-cycle IDs, ordering digests |
| FR-P2C-002 RESP parser | parser paths in legacy RESP layer | Frame parsing semantics, error replies, bulk/string/array handling | Malformed nested frames, oversized lengths, parser corner overflow behavior | Byte-level compatibility for parity-target protocol corpus | Allowlisted parser diagnostics and bounded rejection context | Unit: protocol vectors nominal/edge/adversarial. E2E: malformed frame replay. Logs: input digest, parse reason code, replay command |
| FR-P2C-003 Command dispatch | `processInputBuffer`, `readQueryFromClient`, command registry | Command lookup/dispatch semantics, arity handling, side-effect ordering | Registry shadowing/alias ambiguities, unknown command edge responses | Preserve command routing and error semantics | Defensive guards on malformed internal state only | Unit: dispatch matrix/property invariants. E2E: multi-command session ordering. Logs: command IDs, dispatch path, output digest |
| FR-P2C-004 ACL/auth | `src/acl.c` selector and rule flow | Authorization semantics, command/category gating, denial behavior | Rule precedence and malformed ACL edge semantics | Preserve authorization decisions and denial outputs | Bounded policy diagnostics, no broad allow fallback | Unit: ACL selector precedence/property checks. E2E: auth workflows and denial replay. Logs: policy decision ID, reason code |
| FR-P2C-005 Persistence/replay | `src/rdb.c`, `src/aof.c` families | Snapshot/replay ordering, recovered state parity | Partial artifact scenarios, mixed-format corruption cases | Fail closed on incompatible/tampered artifacts | Allowlisted bounded replay repair with decode proof | Unit: serialization invariants. E2E: crash/restart replay suites. Logs: artifact digests, replay steps, decode proof refs |
| FR-P2C-006 Replication state machine | `src/replication.c` flow | Replication ordering, state transitions, backlog semantics | Retry/reconnect race edges, stale stream replay ambiguity | Preserve replication sequence behavior across parity-target paths | Defensive reject + forensic context on order violations | Unit: state machine/property invariants. E2E: primary-replica recovery. Logs: sequence IDs, transition traces |
| FR-P2C-007 Cluster behavior | `src/cluster.c` cluster paths | Slot/routing behavior for parity-target cluster surface | Migration edge semantics in advanced cluster modes | Preserve slot/route observable behavior for parity-target scenarios | Bounded diagnostics for malformed/out-of-contract paths | Unit: slot mapping invariants. E2E: cluster journey scripts. Logs: slot IDs, route decisions |
| FR-P2C-008 Expire/evict | `src/expire.c`, `src/evict.c` | TTL semantics, eviction ordering, side-effect consistency | Time-bucket edge races and simultaneous expiry/eviction collisions | Preserve expiration and eviction behavior across parity-target scenarios | Guard against pathological cardinalities via allowlisted clamps | Unit: TTL/eviction property suites. E2E: mixed workload TTL journeys. Logs: time buckets, key digests, eviction reason |
| FR-P2C-009 TLS/config boundary | `src/tls.c`, `src/config.c` | Config acceptance/rejection semantics and startup behavior | Invalid combo configs and downgrade-path ambiguities | Fail closed on unknown/incompatible config/protocol edges | Allowlisted metadata sanitization + explicit audit logs | Unit: config parser/rule invariants. E2E: startup/reload failure scripts. Logs: config digest, policy decision, replay pointer |

## 5. Explicit Non-Goals

- No permanent scope reduction away from full parity objective.
- No speculative behavior changes justified only by performance.
- No hidden auto-repair in strict mode.
- No undocumented compatibility deviations.

## 6. Verification Mapping Contract

Every packet change must map:
- At least one unit/property suite for each high-risk contract row.
- At least one deterministic e2e scenario per user-visible workflow/failure envelope.
- Structured logging with:
  - `ts_utc`
  - `suite_id`
  - `test_or_scenario_id`
  - `packet_id`
  - `mode`
  - `seed`
  - `input_digest`
  - `output_digest`
  - `duration_ms`
  - `outcome`
  - `reason_code`
  - `replay_cmd`
  - `artifact_refs`

## 7. Durability and Reproducibility Requirements

Durable artifacts requiring RaptorQ sidecars:
- conformance fixture bundles
- benchmark baseline bundles
- migration manifests
- reproducibility ledgers
- long-lived snapshots

For each recovery event:
- generate repair symbol manifest
- emit integrity scrub output
- emit decode proof artifact

## 8. Optimization Protocol (One Lever Per Change)

Required loop:
1. Baseline (`p50/p95/p99`, memory)
2. Profile hotspot evidence
3. Apply one optimization lever only
4. Run differential + invariant proofs
5. Re-baseline and store delta artifact

Acceptance gate:
- Any measurable speedup without behavior-isomorphism evidence is rejected.

## 9. asupersync/frankentui Operability Hooks

Planned integration requirements:
- expose deterministic artifact indexes for synchronization via `asupersync`
- expose replay/debug summaries for operator workflows via `frankentui`
- include reason-code centric forensic summaries consumable without chat context

## 10. Execution Handoff Checklist

Before packet implementation starts:
- Packet row in Section 4 has no unresolved ambiguity marker.
- Strict/hardened policy entries are explicit.
- Unit/e2e/log evidence rows are mapped to concrete test/script IDs.
- Threat-model and compatibility edges are linked to `SECURITY_COMPATIBILITY_THREAT_MATRIX_V1.md`.
- Replay commands and artifact paths are documented and runnable.
