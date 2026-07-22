# Benchmark Regression Gate

- status: PASS
- throughput_drop_pct threshold: 15.00
- p99_regression_pct threshold: 10.00
- report: `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/gate_report.json`

| workload | status | ops/sec delta | p99 delta | baseline | candidate |
| --- | --- | ---: | ---: | --- | --- |
| set | pass | +8015.26% | -98.64% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_set.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/candidate/set.json` |
| get | pass | +7067.08% | -98.44% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_get.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/candidate/get.json` |
| mixed | pass | +7711.19% | -98.74% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_mixed.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/candidate/mixed.json` |
| pipeline16 | pass | +86308.42% | -99.86% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_pipeline16.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/candidate/pipeline16.json` |
| incr | pass | +7966.04% | -99.04% | `/data/projects/frankenredis/baselines/frankenredis_v0.1.0_incr.json` | `/data/projects/frankenredis/artifacts/benchmark/gate-citbb-20260722/candidate/incr.json` |
