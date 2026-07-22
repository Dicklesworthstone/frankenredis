# Benchmark Regression Gate

- status: PASS
- throughput_drop_pct threshold: 15.00
- p99_regression_pct threshold: 10.00
- report: `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/gate_report.json`

| workload | status | ops/sec delta | p99 delta | baseline | candidate |
| --- | --- | ---: | ---: | --- | --- |
| set | pass | +8152.44% | -98.55% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_set.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/candidate/set.json` |
| get | pass | +8309.00% | -98.39% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_get.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/candidate/get.json` |
| mixed | pass | +7495.17% | -98.63% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_mixed.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/candidate/mixed.json` |
| pipeline16 | pass | +85847.52% | -99.86% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_pipeline16.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/candidate/pipeline16.json` |
| incr | pass | +8213.21% | -98.90% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_incr.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-pipsm-20260722/candidate/incr.json` |
