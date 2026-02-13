#!/usr/bin/env bash
set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-6379}"

redis-cli -h "$HOST" -p "$PORT" ping >/dev/null

echo "running live oracle command diff: core_errors.json"
cargo run -p fr-conformance --bin live_oracle_diff -- command core_errors.json "$HOST" "$PORT"

echo "running live oracle command diff: core_strings.json"
cargo run -p fr-conformance --bin live_oracle_diff -- command core_strings.json "$HOST" "$PORT"

echo "running live oracle protocol diff: protocol_negative.json"
cargo run -p fr-conformance --bin live_oracle_diff -- protocol protocol_negative.json "$HOST" "$PORT"

echo "live oracle diffs passed"
