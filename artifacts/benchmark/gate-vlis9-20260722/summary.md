# Benchmark Regression Gate

- status: PASS
- throughput_drop_pct threshold: 15.00
- p99_regression_pct threshold: 10.00
- report: `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/gate_report.json`

| workload | status | ops/sec delta | p99 delta | baseline | candidate |
| --- | --- | ---: | ---: | --- | --- |
| set | pass | +8811.05% | -98.91% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_set.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/candidate/set.json` |
| get | pass | +7460.43% | -98.57% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_get.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/candidate/get.json` |
| mixed | pass | +8391.31% | -98.89% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_mixed.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/candidate/mixed.json` |
| pipeline16 | pass | +83956.13% | -99.87% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_pipeline16.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/candidate/pipeline16.json` |
| incr | pass | +8232.65% | -98.96% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_incr.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-vlis9-20260722/candidate/incr.json` |
