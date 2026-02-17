# FR-P2C-006 Contract Table

Packet: `FR-P2C-006`  
Subsystem: replication state machine  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-006/legacy_anchor_map.md`

## Contract row schema (normative)

Each row defines:

- `trigger`: deterministic replication event.
- `preconditions`: required state before contract evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: bounded defensive behavior preserving API/ordering contract.
- `fail_closed_boundary`: mandatory hard-failure edge.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: deterministic diagnostics required on mismatch.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-006-C01` | Backlog creation lifecycle | First replica attached or master backlog boot path | Backlog offset origin and replid reset semantics match legacy rules. | Same; diagnostics may be richer. | Backlog created without required replid history reset preconditions. | `FR-P2C-006-U001` | `FR-P2C-006-E001` | `repl.backlog_lifecycle_contract_violation` |
| `FR-P2C-006-C02` | Master handles PSYNC with compatible replid/offset | Backlog exists and covers requested offset | Accept partial resync with `+CONTINUE` and send deterministic backlog delta. | Same. | Out-of-history request accepted as partial resync. | `FR-P2C-006-U001` | `FR-P2C-006-E001` | `repl.psync_partial_accept_mismatch` |
| `FR-P2C-006-C03` | Master cannot satisfy partial resync | Replid mismatch or offset outside backlog window | Reject partial path and drive deterministic full-resync setup. | Same. | Partial-resync continuation despite history mismatch. | `FR-P2C-006-U002` | `FR-P2C-006-E002` | `repl.psync_fullresync_fallback_mismatch`, `repl.psync_replid_or_offset_reject_mismatch` |
| `FR-P2C-006-C04` | Replica parses PSYNC response | PSYNC response line available | Parse `+FULLRESYNC`, `+CONTINUE`, `+RDBCHANNELSYNC`, transient errors with deterministic transition outcomes. | Same with bounded diagnostics. | Malformed fullresync metadata accepted. | `FR-P2C-006-U010` | `FR-P2C-006-E010` | `repl.fullresync_reply_parse_violation` |
| `FR-P2C-006-C05` | `syncWithMaster` handshake progression | Replica connection established | Ordered state machine: `PING -> AUTH? -> REPLCONF* -> PSYNC`, with fallback policy and explicit state transitions. | Same. | Out-of-order transition accepted. | `FR-P2C-006-U003` | `FR-P2C-006-E003` | `repl.handshake_state_machine_mismatch` |
| `FR-P2C-006-C06` | `REPLCONF ACK/FACK` processing | Sender is replica client | ACK/FACK offsets/time update strictly monotonic and drive stream-enable gates where required. | Same. | ACK path decreases offsets or skips required transition gates. | `FR-P2C-006-U004` | `FR-P2C-006-E004` | `repl.replconf_ack_offset_contract_violation` |
| `FR-P2C-006-C07` | Master-stream delta propagation | Command applied on master-client path | Only applied replication-stream delta is proxied to backlog/sub-replicas, preserving byte-order semantics. | Same. | Non-applied bytes propagated or applied bytes dropped/reordered. | `FR-P2C-006-U013` | `FR-P2C-006-E013` | `repl.applied_delta_proxy_violation` |
| `FR-P2C-006-C08` | Role transition to/from master | `REPLICAOF`/failover path invoked | `replicationSetMaster`/`UnsetMaster` transitions maintain replid history, reconnect/disconnect semantics, and AOF restart policy. | Same. | Role transition without replid-history safety update. | `FR-P2C-006-U007` | `FR-P2C-006-E007` | `repl.role_transition_replid_shift_mismatch` |
| `FR-P2C-006-C09` | `REPLICAOF` command handling | Cluster/failover mode constraints known | Enforce command restrictions and idempotent no-op semantics for same-master reconfiguration. | Same. | Disallowed mode accepts REPLICAOF request. | `FR-P2C-006-U008` | `FR-P2C-006-E008` | `repl.replicaof_mode_constraint_violation` |
| `FR-P2C-006-C10` | `ROLE` reply generation | Node role resolved | Reply shape and replication metadata fields are deterministic per role. | Same. | Role response format/field semantics drift. | `FR-P2C-006-U014` | `FR-P2C-006-E014` | `repl.role_reply_contract_violation` |
| `FR-P2C-006-C11` | `WAIT` command evaluation/unblock | Master mode, client write offset tracked | Immediate/blocking behavior reflects acknowledged replica offsets at/above target. | Same. | WAIT unblocks or returns counts with unsatisfied thresholds. | `FR-P2C-006-U005` | `FR-P2C-006-E005` | `repl.wait_ack_count_mismatch` |
| `FR-P2C-006-C12` | `WAITAOF` command evaluation/unblock | Master mode; local AOF policy known | Local fsync and replica AOF-ack thresholds are jointly enforced; reply tuple is deterministic. | Same. | WAITAOF accepts when local/replica thresholds unmet. | `FR-P2C-006-U006` | `FR-P2C-006-E006` | `repl.waitaof_ack_semantics_mismatch` |
| `FR-P2C-006-C13` | `beforeSleep` replication wake path | `get_ack_from_slaves` or fsynced reploff changes | GETACK batching and wake behavior must trigger prompt WAIT/WAITAOF progression without offset drift. | Same. | Pending WAIT/WAITAOF clients not woken when thresholds satisfy. | `FR-P2C-006-U006` | `FR-P2C-006-E006` | `repl.before_sleep_ack_wake_violation` |
| `FR-P2C-006-C14` | `replicationCron` maintenance | Replication active or backlog resident | Timeout, keepalive/newline, and backlog TTL/replid-rotation behavior follow deterministic policy. | Same; bounded clamp diagnostics allowed. | Timeout/backlog-rotation policy bypass causing unsafe PSYNC acceptance. | `FR-P2C-006-U011` | `FR-P2C-006-E011` | `repl.backlog_ttl_replid_reset_violation`, `repl.replication_cron_timeout_policy_violation` |
| `FR-P2C-006-C15` | RDB load replication metadata restore | RDB contains repl metadata | Replid/offset restore and backlog rebase semantics match role-dependent contract. | Same. | Metadata restore diverges and produces invalid offset lineage. | `FR-P2C-006-U009` | `FR-P2C-006-E009` | `repl.rdb_repl_metadata_restore_violation` |
| `FR-P2C-006-C16` | Hardened-mode non-allowlisted deviation candidate | Mode=`hardened` and deviation unresolved | Strict-equivalent fail-closed baseline unless deviation category is explicitly allowlisted. | Only allowlisted bounded defenses may proceed with policy evidence. | Non-allowlisted deviation changes outward replication semantics. | `FR-P2C-006-U012` | `FR-P2C-006-E012` | `repl.hardened_nonallowlisted_rejected`, `repl.hardened_policy_violation` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-006-I01` | Backlog/replid lifecycle safety | Required | Required |
| `FR-P2C-006-I02` | PSYNC partial/full split correctness | Required | Required |
| `FR-P2C-006-I03` | Replica handshake order integrity | Required | Required |
| `FR-P2C-006-I04` | REPLCONF ACK/FACK monotonicity | Required | Required |
| `FR-P2C-006-I05` | Applied-stream delta proxy correctness | Required | Required |
| `FR-P2C-006-I06` | Role transition replid history continuity | Required | Required |
| `FR-P2C-006-I07` | WAIT threshold semantics | Required | Required |
| `FR-P2C-006-I08` | WAITAOF local+replica threshold semantics | Required | Required |
| `FR-P2C-006-I09` | Cron timeout and backlog TTL safety | Required | Required |
| `FR-P2C-006-I10` | RDB repl metadata restore/rebase | Required | Required |
| `FR-P2C-006-I11` | Event-loop ACK wake behavior | Required | Required |
| `FR-P2C-006-I12` | Non-allowlisted hardened deviations | N/A (strict fail-closed baseline) | Reject non-allowlisted deviations |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: richer parser diagnostics for PSYNC/REPLCONF input without behavior drift.
- `MetadataSanitization`: bounded metadata sanitation only when strict-equivalent.
- `ResourceClamp`: bounded replication buffering/clamp behavior that does not alter visible ordering contract.

Non-allowlisted behavior differences are rejected and treated as `fail_closed`.

## Structured-log contract for FR-P2C-006 rows

Each contract-row verification result (pass/fail and divergence checks) must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-006`)
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

- Strict unit replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u001_psync_accepts_partial_resync_inside_window`
- Strict conformance replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_psync_adversarial_matrix_prefers_safe_fallbacks`
- Hardened conformance replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_waitaof_metamorphic_joint_threshold_semantics_hold`

## Alien-graveyard recommendation contract card

| Field | Value |
|---|---|
| `claim_id` | `FR-P2C-006-CLAIM-02` |
| `evidence_id` | `FR-P2C-006-EVID-CONTRACT-001` |
| Hotspot evidence | `C02`, `C05`, `C11` (PSYNC branch, handshake ordering, WAIT semantics) |
| Mapped graveyard section IDs | `AG-DET-04`, `AG-NET-06`, `AG-SEC-11` |
| Baseline comparator | Legacy Redis replication-state machine behavior |
| EV score | `2.8` |
| Priority tier | `S` |
| Adoption wedge | Implement handshake+PSYNC core first, then WAIT/WAITAOF and role transition envelopes |
| Budgeted mode defaults | Strict=`FailClosed`; Hardened=`BoundedDefense` allowlist only |
| Deterministic exhaustion behavior | On budget exhaustion force strict-equivalent fail-closed with `repl.hardened_budget_exhausted_failclosed` |
| Replay commands | `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-repl -- --nocapture fr_p2c_006_u001_psync_accepts_partial_resync_inside_window`; `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance -- --nocapture fr_p2c_006_f_waitaof_metamorphic_joint_threshold_semantics_hold` |

## Expected-loss decision model

States:

- `S0`: contract-preserving replication behavior
- `S1`: bounded recoverable condition (allowlisted)
- `S2`: unsafe history/order divergence condition

Actions:

- `A0`: continue normal path
- `A1`: apply allowlisted bounded defense + evidence emission
- `A2`: fail closed and block transition

Loss matrix:

| State \ Action | `A0` | `A1` | `A2` |
|---|---:|---:|---:|
| `S0` | 0 | 1 | 7 |
| `S1` | 8 | 2 | 4 |
| `S2` | 10 | 8 | 1 |

Posterior/evidence terms:

- `P(S1|e)`: handshake/parser anomaly rates without offset lineage drift.
- `P(S2|e)`: replid/offset divergence evidence and wait-threshold violations.

Calibration + fallback:

- Calibration metric target: Brier `<= 0.12`.
- Fallback trigger: two consecutive calibration breaches or critical row drift (`C02`, `C05`, `C11`, `C16`).
- Fallback behavior: disable hardened deviations and enforce strict fail-closed packet mode.

## One-lever extreme-optimization loop artifacts

Selected single optimization lever:

- `LEV-006-02`: deterministic ACK-threshold evaluator cache keyed by `(target_offset, replica_ack_epoch, fsync_epoch)` with invalidation on offset/apply updates.

Required artifacts:

- Baseline/profile evidence: `artifacts/phase2c/FR-P2C-006/baseline_profile.json`
- Chosen lever note: `artifacts/phase2c/FR-P2C-006/lever_selection.md`
- Post-change re-profile: `artifacts/phase2c/FR-P2C-006/post_profile.json`
- Behavior-isomorphism proof: `artifacts/phase2c/FR-P2C-006/isomorphism_report.md`

## Reproducibility/provenance pack references

- `artifacts/phase2c/FR-P2C-006/env.json`
- `artifacts/phase2c/FR-P2C-006/manifest.json`
- `artifacts/phase2c/FR-P2C-006/repro.lock`
- `artifacts/phase2c/FR-P2C-006/LEGAL.md` (required if IP/provenance risk is found)

## Traceability checklist

- Every contract row maps to at least one unit ID and one e2e ID.
- Every contract row declares deterministic `reason_code` values.
- Every contract row includes explicit strict/hardened expectations and fail-closed boundary.
- Replication user-visible outcomes are explicit for replies/ordering/offset contracts.
