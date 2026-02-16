# Conformance Fixtures

This folder stores normalized oracle-vs-target fixtures for fr-conformance.

- core_strings.json: first deterministic compatibility suite for `PING`, `SET`, `GET`, `DEL`, `INCR`, `EXPIRE`, and `PTTL`.
- core_errors.json: Redis-style error normalization suite for unknown command, arity, syntax, integer parse, and overflow paths.
- protocol_negative.json: malformed RESP corpus for parser fail-closed behavior and protocol error string contract.
- persist_replay.json: replay-oriented fixtures that execute AOF-shaped records and assert post-replay key state.
- adversarial_corpus_v1.json: versioned adversarial corpus manifest (suite mode, fixture path, risk focus, replay commands, and default route bead).
- smoke_case.json: legacy bootstrap fixture retained for backwards compatibility.

## Live Oracle E2E Orchestrator

Use the repository-level orchestrator script to run deterministic live Redis differential E2E suites
with replay/forensics artifacts:

```bash
./scripts/run_live_oracle_diff.sh --host 127.0.0.1 --port 6379
```

By default it uses local `cargo run` so the binary can reach your local Redis endpoint.
Set `FR_E2E_RUNNER=rch` only when the target Redis host is reachable from remote RCH workers.

The script runs `core_errors`, `core_strings`, and `protocol_negative` in a fixed order and writes a
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
