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

## Cross-Crate Unit/Property Convention Contract (v1)

This section defines the minimum style contract all crates must follow when
emitting verification evidence intended to satisfy `bd-2wb.5`.

### Test/Scenario Naming

1. Unit and adversarial contract tests should use stable packet-scoped IDs:
   - `fr_p2c_<packet>_u<nnn>_<short_description>`
2. E2E and differential scenarios should use stable IDs in fixtures/logs:
   - `<packet-family>::<scenario_name>`
3. `test_or_scenario_id` in structured logs must exactly match executed test/scenario IDs.

### Determinism Rules

1. Every replay-capable verification path must carry an explicit `seed`.
2. `mode` must always be explicit (`strict` or `hardened`), never implied.
3. `input_digest` and `output_digest` must be deterministic for identical fixture + seed + mode.
4. Failures must include a stable `reason_code` and a runnable `replay_cmd`.

### Evidence-Link Minimums

Each emitted event must include `artifact_refs` that point to:

1. schema or contract source (`TEST_LOG_SCHEMA_V1.md` and/or packet contract doc),
2. reproducibility metadata (`manifest.json`, `env.json`, `repro.lock` where applicable),
3. fixture/scenario artifact used to produce the event.

### Reason-Code Namespace Guidance

Use packet/subsystem-prefixed reason codes where possible:

- `protocol.*` or parser-specific canonical names (e.g. `protocol_parse_failure`)
- `auth.*`, `repl.*`, `cluster.*`, `persist.*`, `expireevict.*`, `tlscfg.*`, `eventloop.*`

Avoid ad-hoc generic reason codes that cannot be mapped back to packet contracts.

### Naming Adoption Snapshot (2026-02-16)

Current `#[test]` naming adoption for packet-scoped prefix convention (`fr_p2c_...`):

| File | Total tests | `fr_p2c_*`-prefixed tests |
| --- | ---: | ---: |
| `crates/fr-command/src/lib.rs` | 34 | 0 |
| `crates/fr-config/src/lib.rs` | 15 | 10 |
| `crates/fr-conformance/src/lib.rs` | 25 | 3 |
| `crates/fr-eventloop/src/lib.rs` | 17 | 16 |
| `crates/fr-expire/src/lib.rs` | 2 | 0 |
| `crates/fr-persist/src/lib.rs` | 5 | 0 |
| `crates/fr-protocol/src/lib.rs` | 7 | 0 |
| `crates/fr-repl/src/lib.rs` | 10 | 9 |
| `crates/fr-runtime/src/lib.rs` | 26 | 19 |
| `crates/fr-store/src/lib.rs` | 22 | 0 |

Interpretation:

1. packet-heavy subsystems (`fr-eventloop`, `fr-repl`, `fr-runtime`) already have strong packet-ID naming adoption.
2. foundation crates with generic data-structure/protocol unit tests still use descriptive local names and should be bridged to packet traceability at fixture/log level when applicable.
3. full harmonization means either:
   - migrating compatible tests to packet-scoped IDs, or
   - adding explicit traceability mapping in docs/fixtures where generic names are intentionally retained.

Reproduction command for this snapshot:

```bash
awk 'FNR==1{file=FILENAME} /#\\[test\\]/{in_test=1; next} in_test && /fn[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\\(/ {match($0,/fn[[:space:]]+([A-Za-z0-9_]+)/,m); if(m[1]!=""){print file","m[1]} in_test=0}' crates/*/src/lib.rs \
| awk -F, '{total[$1]++; if($2 ~ /^fr_p2c_/) pref[$1]++} END {for (f in total) printf "%s,%d,%d\\n", f, total[f], (pref[f]+0)}' \
| sort
```

## Emission Coverage Matrix (Current)

| Path | Emits structured events | JSONL persistence toggle | Notes |
| --- | --- | --- | --- |
| `run_fixture` | yes | yes (`live_log_root`) | per-case threat + frame checks |
| `run_protocol_fixture` | yes | yes (`live_log_root`) | protocol-negative contract coverage |
| `run_replay_fixture` | yes | yes (`live_log_root`) | replay + assertion path |
| `run_live_redis_diff` | yes | yes (`live_log_root`) | requires live redis server |
| `run_live_redis_protocol_diff` | yes | yes (`live_log_root`) | requires live redis server |

## Close-Gate Checklist for `bd-2wb.5`

1. Schema remains versioned and validated (`fr_testlog_v1`).
2. Golden logs exist for all packet families and validate.
3. All harness entry points above can emit and persist structured logs when `live_log_root` is set.
4. Replay command templates remain deterministic and mode/seed aware.
5. Cross-crate tests adopt stable packet-scoped naming and reason-code mapping discipline.

## Verification Evidence Snapshot (2026-02-16)

All compute-intensive cargo commands were offloaded with `rch`.

```bash
rch doctor
rch exec -- cargo test -p fr-conformance --test log_contract_goldens -- --nocapture
rch exec -- cargo test -p fr-conformance --test log_contract_live -- --nocapture
rch exec -- cargo test -p fr-conformance log_contract -- --nocapture
```

Observed outcomes:

1. `rch doctor` passed (worker fleet healthy).
2. `log_contract_goldens` passed (`1 passed, 0 failed`).
3. `log_contract_live` passed (`2 passed, 0 failed`).
4. `log_contract`-filtered test run passed (`7 passed, 0 failed` in `fr_conformance` unit tests, plus filtered integration bins/tests).

These commands provide deterministic replay anchors for schema/golden validation and
live JSONL append semantics without relying on local compilation.

## CPU Offload Note (`rch`)

For reproducible and resource-safe execution in multi-agent sessions, run
heavy cargo commands via `rch`:

```bash
rch exec -- cargo test -p fr-conformance -- --nocapture
rch exec -- cargo check --workspace --all-targets
rch exec -- cargo clippy --workspace --all-targets -- -D warnings
```
