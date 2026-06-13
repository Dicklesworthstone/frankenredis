#!/usr/bin/env bash
set -euo pipefail

variant="$1"
server_bin="$2"
bench_bin="$3"
port="$4"
key_prefix="$5"
artifact_dir="${6:-artifacts/optimization/frankenredis-coralox-pass173}"

server_log="$artifact_dir/${variant}-hset-p16-c50-n1m-paired-server.log"
bench_log="$artifact_dir/${variant}-hset-p16-c50-n1m-paired-last.log"
bench_json="$artifact_dir/${variant}-hset-p16-c50-n1m-paired-last.json"

"$server_bin" --bind 127.0.0.1 --port "$port" >"$server_log" 2>&1 &
pid="$!"
trap 'kill "$pid" >/dev/null 2>&1 || true; wait "$pid" >/dev/null 2>&1 || true' EXIT

for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    if nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
        break
    fi
    sleep 0.05
done

"$bench_bin" \
    --host 127.0.0.1 \
    --port "$port" \
    --clients 50 \
    --requests 1000000 \
    --pipeline 16 \
    --keyspace 10000 \
    --datasize 3 \
    --workload hset \
    --key-prefix "$key_prefix" \
    --json-out "$bench_json" >"$bench_log"
