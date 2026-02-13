#!/usr/bin/env bash
set -euo pipefail

mkdir -p baselines

hyperfine \
  --warmup 1 \
  --runs 3 \
  --export-json baselines/round2_protocol_negative_baseline.json \
  'cargo test -p fr-conformance tests::conformance_protocol_fixture_passes -- --exact --nocapture'

strace -c -o baselines/round2_protocol_negative_strace.txt \
  cargo test -p fr-conformance tests::conformance_protocol_fixture_passes -- --exact --nocapture

echo "wrote baselines/round2_protocol_negative_baseline.json"
echo "wrote baselines/round2_protocol_negative_strace.txt"
