# Benchmark Regression Gate

- status: PASS
- throughput_drop_pct threshold: 15.00
- p99_regression_pct threshold: 10.00
- report: `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/gate_report.json`

| workload | status | ops/sec delta | p99 delta | baseline | candidate |
| --- | --- | ---: | ---: | --- | --- |
| set | pass | +8482.12% | -98.73% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_set.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/candidate/set.json` |
| get | pass | +6040.39% | -97.97% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_get.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/candidate/get.json` |
| mixed | pass | +7260.63% | -98.73% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_mixed.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/candidate/mixed.json` |
| pipeline16 | pass | +81767.66% | -99.86% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_pipeline16.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/candidate/pipeline16.json` |
| incr | pass | +6983.14% | -98.75% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_incr.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-c47rl-20260723/candidate/incr.json` |
