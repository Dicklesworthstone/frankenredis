#!/usr/bin/env bash
set -euo pipefail
bin="/data/projects/frankenredis/target-local/debug/phase2c_schema_gate"
mapfile -t args < <(find /data/projects/frankenredis/artifacts/optimization/phase2c-gate/bench_packets -mindepth 1 -maxdepth 1 -type d | sort)
"$bin" "${args[@]}"
