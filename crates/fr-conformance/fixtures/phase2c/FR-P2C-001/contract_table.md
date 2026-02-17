# FR-P2C-001 Contract Table

Packet: `FR-P2C-001`  
Subsystem: event loop core  
Depends on: `crates/fr-conformance/fixtures/phase2c/FR-P2C-001/legacy_anchor_map.md`

## Contract row schema (normative)

Each row below is interpreted as:

- `trigger`: deterministic event that initiates behavior.
- `preconditions`: state predicates that must hold before evaluation.
- `strict_contract`: Redis-observable behavior that must match legacy semantics.
- `hardened_contract`: allowed bounded defenses that must preserve external behavior contract.
- `fail_closed_boundary`: condition that must hard-fail with no undefined continuation.
- `unit_trace` / `e2e_trace`: required verification mapping.
- `reason_codes`: required diagnostic surface for failure analysis.

## Contract rows

| Contract ID | Trigger | Preconditions | Strict contract | Hardened contract | Fail-closed boundary | Unit trace | E2E trace | Reason codes |
|---|---|---|---|---|---|---|---|---|
| `FR-P2C-001-C01` | Event-loop cycle executes with both file and time events enabled | Event loop initialized; hooks installed | Dispatch order is `before_sleep -> poll -> after_sleep -> file callbacks -> time callbacks` | Same ordering; may emit additional diagnostics only | Unknown callback stage transition | `FR-P2C-001-U001` | `FR-P2C-001-E001` | `eventloop.dispatch.order_mismatch`, `eventloop.dispatch.stage_transition_invalid` |
| `FR-P2C-001-C02` | `AE_BARRIER` present for writable+readable fd | Same fd has both readiness bits set | Writable callback executes before readable callback for that fd in cycle | Same external callback ordering; internal metrics allowed | Barrier flag ignored when set | `FR-P2C-001-U002` | `FR-P2C-001-E002` | `eventloop.ae_barrier_violation` |
| `FR-P2C-001-C03` | Dont-wait mode requested by pending transport conditions | Event loop running; pending data or explicit no-wait flag | Poll timeout is zero; next cycle is not blocked on sleep | May clamp to deterministic zero timeout under defended conditions | Timeout computed negative/overflow or blocks despite no-wait | `FR-P2C-001-U003` | `FR-P2C-001-E003` | `eventloop.dont_wait_not_set`, `eventloop.timeout_invalid` |
| `FR-P2C-001-C04` | File event registration for fd beyond current array capacity | `fd < setsize` and loop active | Event arrays resize safely; registration succeeds and `maxfd` updates | Same; may enforce deterministic resource clamp with explicit diagnostics | Registration beyond `setsize` must return hard error and no partial registration | `FR-P2C-001-U004` | `FR-P2C-001-E004` | `eventloop.fd_resize_failure`, `eventloop.fd_out_of_range` |
| `FR-P2C-001-C05` | Accept path receives new connection | Listener registered; transport accepting | Admission control rejects over-`maxclients` with error and close; accepted path binds read handler and client context | Same external accept/reject semantics; optional bounded handshake diagnostics | Ambiguous accepted state (no handler/context) | `FR-P2C-001-U006` | `FR-P2C-001-E006` | `eventloop.accept.maxclients_reached`, `eventloop.accept.handler_bind_failure` |
| `FR-P2C-001-C06` | Client read path consumes socket bytes | Client connected; read enabled | Parse path respects query-buffer bounds, command queue semantics, and fatal-read closure behavior | Same observable closes/errors/replies; may record bounded parser diagnostics | Query-buffer over-limit without disconnect, or fatal read continues execution | `FR-P2C-001-U007` | `FR-P2C-001-E007` | `eventloop.read.querybuf_limit_exceeded`, `eventloop.read.fatal_error_disconnect` |
| `FR-P2C-001-C07` | Pending write queue drained before sleep re-entry | Clients pending write may exist | Synchronous flush attempted before write-handler arming; residual pending replies arm write handlers | Same; internal IO-thread scheduling decisions can differ if ordering and outputs preserved | Pending replies dropped or reordered across clients | `FR-P2C-001-U009` | `FR-P2C-001-E009` | `eventloop.write.flush_order_violation`, `eventloop.write.pending_reply_lost` |
| `FR-P2C-001-C08` | Blocked-mode progress processing invoked | `ProcessingEventsWhileBlocked > 0` | Up to bounded non-blocking iterations with progress check; vital subset work only | Same externally visible behavior; may add bounded recovery counters | Unbounded loop or full beforeSleep pipeline in blocked mode | `FR-P2C-001-U005` | `FR-P2C-001-E005` | `eventloop.blocked_mode_progress_stall`, `eventloop.blocked_mode_scope_violation` |
| `FR-P2C-001-C09` | Before/after sleep hooks wired during server init | Event loop created; init path active | `serverCron` timer + before/after sleep hooks installed before persistence-loading re-entry points | Same; hardening may validate hook pointers | Missing hook install when event loop starts | `FR-P2C-001-U010` | `FR-P2C-001-E010` | `eventloop.hook_install_missing`, `eventloop.server_cron_timer_missing` |
| `FR-P2C-001-C10` | Runtime enters main server loop | Listeners initialized; init complete | Main runtime enters event loop (`aeMain`) and exits only on stop path | Same; hardened mode cannot bypass main loop contract | Runtime serves requests without entering loop contract path | `FR-P2C-001-U011` | `FR-P2C-001-E011` | `eventloop.main_loop_entry_missing` |

## Strict vs hardened invariants

| Invariant ID | Invariant | Strict mode | Hardened mode |
|---|---|---|---|
| `FR-P2C-001-I01` | Callback phase order is deterministic | Required exact legacy order | Required exact legacy order |
| `FR-P2C-001-I02` | `AE_BARRIER` ordering | Required | Required |
| `FR-P2C-001-I03` | Accept over-capacity semantics | Must reject with deterministic error + close | Same |
| `FR-P2C-001-I04` | Query-buffer limit enforcement | Must disconnect over limit | Same |
| `FR-P2C-001-I05` | Blocked-mode bounded processing | Must not run full normal path | Same |
| `FR-P2C-001-I06` | File event registration bounds | Hard error beyond setsize; no partial side effects | Same + optional bounded diagnostics |
| `FR-P2C-001-I07` | Pending-write delivery | No silent drop or reordering drift | Same |
| `FR-P2C-001-I08` | Main loop entry | Runtime must pass through event-loop contract | Same |

## Allowed hardened deviations (bounded)

- `BoundedParserDiagnostics`: additional deterministic diagnostics for read/parse failures.
- `ResourceClamp`: deterministic safeguards on pathological resource pressure.

Non-allowlisted behavior differences are rejected (`fail_closed`) and must emit structured diagnostics.

## Structured-log contract for contract rows

Every failing or divergent contract-row assertion must emit:

- `ts_utc`
- `suite_id`
- `test_or_scenario_id`
- `packet_id` (`FR-P2C-001`)
- `mode` (`strict` or `hardened`)
- `seed`
- `input_digest`
- `output_digest`
- `duration_ms`
- `outcome`
- `reason_code` (one of row-defined codes above)
- `replay_cmd`
- `artifact_refs`

## Replay command templates

- Strict unit replay: `FR_MODE=strict FR_SEED=17 rch exec -- cargo test -p fr-eventloop -- --nocapture fr_p2c_001_u001_phase_order_is_deterministic`
- Hardened unit replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-runtime -- --nocapture fr_p2c_001_u005_runtime_blocked_mode_is_bounded`
- E2E replay: `FR_MODE=hardened FR_SEED=42 rch exec -- cargo test -p fr-conformance --test smoke -- --nocapture fr_p2c_001_e2e_contract_smoke`

## Traceability checklist

- Every row maps to at least one unit ID and one e2e ID in the table above.
- Every row defines at least one deterministic `reason_code`.
- Every row has explicit strict and hardened expectations plus fail-closed boundary.

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

- `LEV-001-OPT-01`: replace repeated per-file `is_file` checks with one deterministic directory scan plus file-presence bitmask.

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
