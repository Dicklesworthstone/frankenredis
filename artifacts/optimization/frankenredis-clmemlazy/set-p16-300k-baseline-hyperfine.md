| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `python3 artifacts/optimization/frankenredis-11a0n/run_frbench_once.py --server-bin /tmp/tealotter-fr-clmemlazy-baseline-target/release-perf/frankenredis --bench-bin /tmp/tealotter-fr-clmemlazy-baseline-target/release-perf/fr-bench --port 21310 --requests 300000 --clients 50 --pipeline 16 --keyspace 10000 --datasize 3 --workload set --json-out artifacts/optimization/frankenredis-clmemlazy/baseline-set-p16-300k-last.json --key-prefix clmemlazy-baseline-set` | 483.6 ± 43.1 | 453.1 | 587.0 | 1.00 |
