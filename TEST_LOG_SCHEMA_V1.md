# Structured Test Log Schema v1

Date: 2026-02-14
Owner bead: `bd-2wb.5`
Schema id: `fr_testlog_v1`

## Purpose

Canonical structured logging contract for unit/property and e2e verification paths.
This schema is designed for deterministic replay, forensics, and machine parsing.

## Required Fields

| Field | Type | Description |
| --- | --- | --- |
| `schema_version` | `string` | Must equal `fr_testlog_v1`. |
| `ts_utc` | `string` | Deterministic UTC timestamp for evidence ordering. |
| `suite_id` | `string` | Stable suite identifier (example: `unit::fr-p2c-002`). |
| `test_or_scenario_id` | `string` | Exact unit/property test ID or e2e scenario ID. |
| `packet_id` | `string` | Packet family ID (for now: `FR-P2C-001..009`). |
| `mode` | `enum(strict,hardened)` | Runtime mode used during verification. |
| `verification_path` | `enum(unit,property,e2e)` | Verification tier. |
| `seed` | `u64` | Deterministic random seed used by test/scenario. |
| `input_digest` | `string` | Digest of canonicalized input. |
| `output_digest` | `string` | Digest of canonicalized output/result. |
| `duration_ms` | `u64` | Duration in milliseconds. |
| `outcome` | `enum(pass,fail)` | Execution outcome. |
| `reason_code` | `string` | Stable reason code (`parity_ok`, `protocol_parse_failure`, etc.). |
| `replay_cmd` | `string` | Single command line to replay the same path. |
| `artifact_refs` | `string[]` | Evidence artifact references for postmortem and audit. |

## Optional Fields

| Field | Type | Description |
| --- | --- | --- |
| `fixture_id` | `string` | Fixture identifier for data-driven tests. |
| `env_ref` | `string` | Path to environment snapshot artifact (`env.json`). |

## Redaction Policy

1. Never log raw command payload bytes that may contain secrets.
2. Store only digests (`input_digest`, `output_digest`) for sensitive payloads.
3. Keep `reason_code` stable and non-sensitive.
4. Keep `artifact_refs` path-based; never embed credentials or tokens.

## Replay Command Templates

Unit/property template:

```bash
FR_MODE=<strict|hardened> FR_SEED=<seed> cargo test -p <crate> <test_or_scenario_id> -- --nocapture
```

E2E template:

```bash
FR_MODE=<strict|hardened> FR_SEED=<seed> cargo test -p fr-conformance --test smoke -- --nocapture <test_or_scenario_id>
```

## Golden Artifacts

Golden logs are emitted under:

- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-001.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-002.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-003.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-004.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-005.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-006.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-007.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-008.golden.jsonl`
- `crates/fr-conformance/fixtures/log_contract_v1/FR-P2C-009.golden.jsonl`

Each file contains exactly:

1. one `unit` event
2. one `e2e` event

Regeneration command:

```bash
cargo run -p fr-conformance --bin emit_log_contract_goldens
```

## Harness Persistence Paths

When `HarnessConfig.live_log_root` is set in `fr-conformance`, structured log lines are appended
to deterministic JSONL files using:

```text
<live_log_root>/<sanitized suite_id>/<sanitized fixture name>.jsonl
```

Current harness entry points that support this persistence path:

- `run_fixture(...)`
- `run_protocol_fixture(...)`
- `run_replay_fixture(...)`
- `run_live_redis_diff(...)`
- `run_live_redis_protocol_diff(...)`
