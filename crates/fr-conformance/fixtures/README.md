# Conformance Fixtures

This folder stores normalized oracle-vs-target fixtures for fr-conformance.

- core_strings.json: first deterministic compatibility suite for `PING`, `SET`, `GET`, `DEL`, `INCR`, `EXPIRE`, and `PTTL`.
- core_errors.json: Redis-style error normalization suite for unknown command, arity, syntax, integer parse, and overflow paths.
- fr_p2c_001_eventloop_journey.json: packet-001 smoke journey fixture used by `fr_p2c_001_e2e_contract_smoke`.
- protocol_negative.json: packet-002 malformed RESP corpus used by `fr_p2c_002_e2e_contract_smoke`.
- fr_p2c_003_dispatch_journey.json: packet-003 dispatch journey fixture used by `fr_p2c_003_e2e_contract_smoke`.
- fr_p2c_004_acl_journey.json: packet-004 ACL/auth journey fixture used by `fr_p2c_004_e2e_contract_smoke`.
- fr_p2c_006_replication_journey.json: packet-006 replication journey fixture used by `fr_p2c_006_e2e_contract_smoke`.
- fr_p2c_007_cluster_journey.json: packet-007 cluster journey fixture used by `fr_p2c_007_e2e_contract_smoke`.
- fr_p2c_008_expire_evict_journey.json: packet-008 expiration/eviction journey fixture used by packet-008 differential + e2e smoke validation; paired optimization evidence lives under `artifacts/phase2c/FR-P2C-008/` and final parity evidence lives under `crates/fr-conformance/fixtures/phase2c/FR-P2C-008/`.
- fr_p2c_009_tls_config_journey.json: packet-009 TLS/config journey fixture used by `fr_p2c_009_e2e_contract_smoke`.
- persist_replay.json: replay-oriented fixtures that execute AOF-shaped records and assert post-replay key state.
- adversarial_corpus_v1.json: versioned adversarial corpus manifest (suite mode, fixture path, risk focus, replay commands, and default route bead).
- user_workflow_corpus_v1.json: versioned user-journey corpus mapping stable scenario IDs to unit/differential/e2e hooks and owner beads.
- smoke_case.json: legacy bootstrap fixture retained for backwards compatibility.

## Live Oracle E2E Orchestrator

Use the repository-level orchestrator script to run deterministic live Redis differential E2E suites
with replay/forensics artifacts:

```bash
./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379
```

By default it uses local `cargo run` so the binary can reach your local Redis endpoint.
Set `FR_E2E_RUNNER=rch` only when the target Redis host is reachable from remote RCH workers.

self-contained bundle. It runs a fixed scenario matrix in deterministic order:

- `core_strings` (golden)
- `fr_p2c_001_eventloop_journey` (golden)
- `fr_p2c_003_dispatch_journey` (golden)
- `core_errors` (regression)
- `fr_p2c_002_protocol_negative` (failure_injection, FR-P2C-002)

It writes a
self-contained bundle under:

```text
artifacts/e2e_orchestrator/<run-id>/
```

Bundle contents:

- `suite_status.tsv` (machine-readable suite status)
- `command_trace.log` (exact command trace)
- `live_logs/` (structured JSONL emitted via `live_log_root`)
- `suites/<suite>/stdout.log` (captured command output)
- `suites/<suite>/report.json` (`live_oracle_diff --json-out` output)
- `replay_all.sh` (deterministic replay commands for all suites)
- `replay_failed.sh` (deterministic replay commands for failures)

## Adversarial Corpus + Crash Triage Pipeline

Run the adversarial triage pipeline (defaults to `rch` for cargo execution):

```bash
./scripts/run_adversarial_triage.sh
```

This executes the suites listed in `adversarial_corpus_v1.json` using `fr-conformance` and emits:

- `artifacts/adversarial_triage/<run-id>/triage_report.json` (suite outcomes + routed regressions)
- `artifacts/adversarial_triage/<run-id>/triage_routes.tsv` (classification -> blocker bead routes)
- `artifacts/adversarial_triage/<run-id>/env.json` (execution metadata)
- `artifacts/adversarial_triage/<run-id>/manifest.json` (copied corpus manifest)
- `artifacts/adversarial_triage/<run-id>/repro.lock` (replay metadata)
- `artifacts/adversarial_triage/<run-id>/live_logs/` (structured JSONL logs)

## RaptorQ Artifact Gate

Generate deterministic RaptorQ sidecar + decode-proof artifacts for durability-critical evidence files:

```bash
./scripts/run_raptorq_artifact_gate.sh
```

The gate auto-discovers canonical `artifacts/phase2c/*` evidence packs (baseline/post profile,
lever/isomorphism docs, env/manifest/repro/legal) and validates:

- sidecar source-hash binding
- decode-proof source-hash binding + verified status
- corruption simulation detection (unless `--no-corruption`)

Outputs are written under:

```text
artifacts/durability/raptorq_runs/<run-id>/
```

## User Workflow Journey Corpus Gate

Validate that the versioned user workflow corpus remains stable and aligned with
golden log artifacts:

```bash
cargo run -p fr-conformance --bin user_journey_corpus_gate -- \
  --manifest crates/fr-conformance/fixtures/user_workflow_corpus_v1.json \
  --json-out artifacts/user_journey_corpus/report.json
```

The gate verifies:

- packet coverage for all `FR-P2C-001..FR-P2C-009` journeys
- stable `test_or_scenario_id` bindings against golden log JSONL entries
- explicit unit/differential/e2e hook mappings with owner-bead traceability

## CI Gate Topology (G1..G8)

`/.github/workflows/live-conformance-gates.yml` wires the foundation gate chain
for `bd-2wb.10` as follows:

- `G1`: `cargo fmt --check` + `cargo clippy --workspace --all-targets -- -D warnings`
- `G2`: `cargo test --workspace -- --nocapture`
- `G3+G5`: `./scripts/run_live_oracle_diff.sh` (differential + deterministic e2e matrix)
- `G4`: `./scripts/run_adversarial_triage.sh`
- `G6`: `cargo run -p fr-conformance --bin phase2c_schema_gate -- --optimization-gate`
- `G7`: corpus gate + packet schema gate (materialized packet dirs with `parity_report.json`) + deterministic failure-forensics index
- `G8`: `./scripts/run_raptorq_artifact_gate.sh`

The CI forensics index is emitted at:

```text
artifacts/failure_forensics/ci-live/index.json
```

with per-gate evidence pointers (including live-oracle coverage/failure envelopes,
adversarial triage outputs, schema/corpus gate outputs, and RaptorQ report paths).

## FR-P2C-003 Optimization Evidence Pack

The FR-P2C-003 profile/isomorphism evidence for `bd-2wb.14.8` is stored under:

```text
artifacts/phase2c/FR-P2C-003/
```

Artifacts include:

- `baseline_profile.json` (linear lookup baseline metrics)
- `post_profile.json` (optimized lookup metrics)
- `lever_selection.md` (selected optimization lever + hotspot evidence)
- `isomorphism_report.md` (behavior-preservation proof and replay commands)
- `env.json`, `manifest.json`, `repro.lock`, `LEGAL.md` (repro/provenance bundle)

## FR-P2C-004 Optimization Evidence Pack

The FR-P2C-004 profile/isomorphism evidence for `bd-2wb.15.8` is stored under:

```text
artifacts/phase2c/FR-P2C-004/
```

Artifacts include:

- `baseline_profile.json` (linear runtime special-command routing baseline metrics)
- `post_profile.json` (optimized routing metrics)
- `lever_selection.md` (selected optimization lever + hotspot evidence)
- `isomorphism_report.md` (behavior-preservation proof and replay commands)
- `env.json`, `manifest.json`, `repro.lock`, `LEGAL.md` (repro/provenance bundle)

## FR-P2C-007 Optimization Evidence Pack

The FR-P2C-007 profile/isomorphism evidence for `bd-2wb.18.8` is stored under:

```text
artifacts/phase2c/FR-P2C-007/
```

Artifacts include:

- `baseline_profile.json` (linear cluster-subcommand routing baseline metrics)
- `post_profile.json` (optimized cluster-subcommand routing metrics)
- `lever_selection.md` (selected optimization lever + hotspot evidence)
- `isomorphism_report.md` (behavior-preservation proof and replay commands)
- `env.json`, `manifest.json`, `repro.lock`, `LEGAL.md` (repro/provenance bundle)

## FR-P2C-008 Optimization Evidence Pack

The FR-P2C-008 profile/isomorphism evidence for `bd-2wb.19.8` is stored under:

```text
artifacts/phase2c/FR-P2C-008/
```

Artifacts include:

- `baseline_profile.json` (linear dispatch lookup baseline metrics)
- `post_profile.json` (optimized dispatch lookup metrics)
- `lever_selection.md` (selected optimization lever + hotspot evidence)
- `isomorphism_report.md` (behavior-preservation proof and replay commands)
- `env.json`, `manifest.json`, `repro.lock`, `LEGAL.md` (repro/provenance bundle)

## FR-P2C-008 Final Parity Evidence Pack

The FR-P2C-008 final packet parity evidence for `bd-2wb.19.9` is stored under:

```text
crates/fr-conformance/fixtures/phase2c/FR-P2C-008/
```

Artifacts include:

- `fixture_manifest.json`
- `parity_gate.yaml`
- `parity_report.json` (`readiness=READY_FOR_IMPL`)
- `parity_report.raptorq.json` (durability sidecar with `raptorq.decode_verified`)
- `parity_report.decode_proof.json` (decode-proof replay metadata)
