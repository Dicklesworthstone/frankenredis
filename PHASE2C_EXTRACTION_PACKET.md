# PHASE2C_EXTRACTION_PACKET.md — FrankenRedis

Date: 2026-02-13

Purpose: convert Phase-2 analysis into direct implementation tickets with concrete legacy anchors, target crates, and oracle tests.

## 1. Ticket Packets

| Ticket ID | Subsystem | Legacy anchors (classes/functions) | Target crates | Oracle tests |
|---|---|---|---|---|
| `FR-P2C-001` | Event loop core | `aeCreateEventLoop`, `aeMain` in `src/ae.c`; backend files `ae_epoll.c`, `ae_kqueue.c`, `ae_select.c` | `fr-eventloop` | `tests/unit/*` for event-loop-sensitive command paths |
| `FR-P2C-002` | RESP parser contract | `parseBulk`, `parseSimpleString`, `parseError`, `parseArray`, `parseReply` family in `src/resp_parser.c` | `fr-protocol` | `tests/vectorset/*`, protocol unit tests |
| `FR-P2C-003` | Command dispatch core | `processInputBuffer`, `readQueryFromClient`, command-time snapshot and dispatch in `src/server.c`; registry generation in `src/commands.c`, `src/commands.def` | `fr-command`, `fr-protocol` | `tests/unit/*`, `tests/integration/*` |
| `FR-P2C-004` | ACL and auth policy | selector and rule machinery in `src/acl.c` (`ACLCreateSelector`, allowed-command/arg tracking) | `fr-config`, `fr-command` | ACL-focused unit/integration tests |
| `FR-P2C-005` | Persistence format and replay | `rdbSave*`/`rdbLoad*` families in `src/rdb.c`; manifest/write/read routines in `src/aof.c` | `fr-persist` | persistence integration tests and replay fixtures |
| `FR-P2C-006` | Replication state machine | replication channel and backlog flow in `src/replication.c` (`replication*`, rdb-channel routines) | `fr-repl` | `tests/integration/*` replication suites |
| `FR-P2C-007` | Cluster behavior (scoped) | payload/slot/restore paths in `src/cluster.c` | `fr-repl`, `fr-command` | `tests/cluster/*` |
| `FR-P2C-008` | Expiration + eviction | active expire logic in `src/expire.c`; eviction loops in `src/evict.c` | `fr-expire`, `fr-store` | TTL/eviction unit and integration tests |
| `FR-P2C-009` | TLS/config boundary | protocol/config validation in `src/tls.c` and config path in `src/config.c` | `fr-config` | TLS/config regression fixtures |

## 2. Packet Definition Template

For each ticket above, deliver all artifacts in the same PR:

1. `legacy_anchor_map.md`: path + line anchors + extracted behavior.
2. `contract_table.md`: input/output/error + protocol/ACL/replication semantics.
3. `fixture_manifest.json`: oracle mapping and fixture IDs.
4. `parity_gate.yaml`: strict + hardened pass criteria.
5. `risk_note.md`: boundary risks and mitigations.

## 3. Strict/Hardened Expectations per Packet

- Strict mode: exact scoped Redis-observable behavior.
- Hardened mode: same outward contract with bounded defensive checks (parser/config/auth limits).
- Unknown incompatible protocol/config/metadata path: fail-closed.

## 4. Immediate Execution Order

1. `FR-P2C-001`
2. `FR-P2C-002`
3. `FR-P2C-003`
4. `FR-P2C-004`
5. `FR-P2C-005`
6. `FR-P2C-008`
7. `FR-P2C-006`
8. `FR-P2C-007`
9. `FR-P2C-009`

## 5. Done Criteria (Phase-2C)

- All 9 packets have extracted anchor maps and contract tables.
- At least one runnable fixture family exists per packet in `fr-conformance`.
- Packet-level parity report schema is produced for every packet.
- RaptorQ sidecars are generated for fixture bundles and parity reports.

## 6. Per-Ticket Extraction Schema (Mandatory Fields)

Every `FR-P2C-*` packet MUST include:
1. `packet_id`
2. `legacy_paths`
3. `legacy_symbols`
4. `state_machine_contract`
5. `protocol_contract`
6. `command_acl_contract`
7. `persistence_replication_contract`
8. `error_contract`
9. `strict_mode_policy`
10. `hardened_mode_policy`
11. `excluded_scope`
12. `oracle_tests`
13. `performance_sentinels`
14. `compatibility_risks`
15. `raptorq_artifacts`

Missing fields => packet state `NOT READY`.

## 7. Risk Tiering and Gate Escalation

| Ticket | Risk tier | Why | Extra gate |
|---|---|---|---|
| `FR-P2C-001` | Critical | event loop state drives all runtime behavior | state-transition replay |
| `FR-P2C-002` | Critical | RESP parser drift breaks wire compatibility | protocol corpus lock |
| `FR-P2C-003` | Critical | command dispatch is central correctness path | command matrix parity gate |
| `FR-P2C-005` | Critical | persistence replay correctness is existential | replay witness gate |
| `FR-P2C-006` | High | replication drift causes cluster inconsistency | replication state witness |
| `FR-P2C-008` | High | eviction/expire affects durability semantics | TTL/eviction trace gate |
| `FR-P2C-009` | High | TLS/config policies are security-critical | hardened policy adversarial gate |

Critical tickets must pass strict drift `0`.

## 8. Packet Artifact Topology (Normative)

Directory template:
- `artifacts/phase2c/FR-P2C-00X/legacy_anchor_map.md`
- `artifacts/phase2c/FR-P2C-00X/contract_table.md`
- `artifacts/phase2c/FR-P2C-00X/fixture_manifest.json`
- `artifacts/phase2c/FR-P2C-00X/parity_gate.yaml`
- `artifacts/phase2c/FR-P2C-00X/risk_note.md`
- `artifacts/phase2c/FR-P2C-00X/parity_report.json`
- `artifacts/phase2c/FR-P2C-00X/parity_report.raptorq.json`
- `artifacts/phase2c/FR-P2C-00X/parity_report.decode_proof.json`

## 9. Optimization and Isomorphism Proof Hooks

Optimization allowed only after strict parity baseline.

Required proof block:
- protocol framing semantics preserved
- command/ACL semantics preserved
- persistence/replication semantics preserved
- fixture checksum verification pass/fail

## 10. Packet Readiness Rubric

Packet is `READY_FOR_IMPL` only when:
1. extraction schema complete,
2. fixture manifest includes happy/edge/adversarial paths,
3. strict/hardened gates are machine-checkable,
4. risk note includes compatibility + security mitigations,
5. parity report has RaptorQ sidecar + decode proof.
