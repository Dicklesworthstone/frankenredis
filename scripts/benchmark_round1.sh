#!/usr/bin/env bash
set -euo pipefail

mkdir -p baselines

hyperfine \
  --warmup 2 \
  --runs 5 \
  --export-json baselines/round1_conformance_baseline.json \
  'cargo test -p fr-conformance smoke_report_is_stable --test smoke -- --exact --nocapture'

strace -c -o baselines/round1_conformance_strace.txt \
  cargo test -p fr-conformance smoke_report_is_stable --test smoke -- --exact --nocapture

echo "wrote baselines/round1_conformance_baseline.json"
echo "wrote baselines/round1_conformance_strace.txt"
