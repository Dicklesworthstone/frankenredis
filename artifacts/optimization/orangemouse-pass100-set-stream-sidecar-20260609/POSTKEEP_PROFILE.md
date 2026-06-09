# Pass100 Post-Keep Profile

Command:

```text
python3 artifacts/optimization/frankenredis-5srqd-pass67/profile_fr_bench_once.py --workload set --requests 1000000 --pipeline 16 ...
```

Result:

- Throughput: `722946.97 ops/sec`
- Latency: p50 `1060us`, p95 `1328us`, p99 `1680us`
- Perf samples: `817`, lost samples: `0`

Top flat rows:

- `Store::internal_entries_insert`: `8.86%` self / `10.75%` children
- `[vdso]`: `7.01%` self
- `Runtime::refresh_store_runtime_info_context`: `5.75%` self /
  `15.58%` children
- `foldhash::quality::RandomState::hash_one::<&Vec<u8>>`: `5.40%` self /
  `6.50%` children
- `Store::drop_if_expired`: `0.16%` self / `7.26%` children
- `parse_command_args_borrowed_into`: `1.33%` self / `2.87%` children

The pre-pass std-hash stream sidecar removals are no longer a visible target.
The next profile-backed primitive should attack `internal_entries_insert` /
keyspace hashing/comparison or `refresh_store_runtime_info_context` as a
deeper structural lever, not another stream-sidecar or output-limit micro-pass.
