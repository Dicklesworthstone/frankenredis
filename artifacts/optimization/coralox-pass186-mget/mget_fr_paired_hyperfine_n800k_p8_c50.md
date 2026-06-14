| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `python3 artifacts/optimization/coralox-pass186-mget/run_mget_bench_once.py --kind fr --bin target-coralox-pass186-baseline/release/frankenredis --port 24967 --requests 800000 --clients 50 --pipeline 8` | 3.446 ± 0.120 | 3.283 | 3.634 | 1.02 ± 0.05 |
| `python3 artifacts/optimization/coralox-pass186-mget/run_mget_bench_once.py --kind fr --bin target-coralox-pass186-candidate/release/frankenredis --port 24968 --requests 800000 --clients 50 --pipeline 8` | 3.371 ± 0.091 | 3.286 | 3.540 | 1.00 |
