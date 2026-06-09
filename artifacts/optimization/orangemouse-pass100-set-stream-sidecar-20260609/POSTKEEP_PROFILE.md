# Pass100 Post-Keep Profile

Command:

```text
python3 artifacts/optimization/frankenredis-5srqd-pass67/profile_fr_bench_once.py --workload set --requests 1000000 --pipeline 16 ...
```

Exact head: `707abfd5a` (includes the peer `950112bf9` stats-event fix on top
of pass100).

Result:

- Throughput: `686031.30 ops/sec`
- Latency: p50 `1074us`, p95 `1596us`, p99 `2145us`
- Perf samples: `886`, lost samples: `0`

Top flat rows:

- `Store::internal_entries_insert`: `9.61%` self / `11.05%` children
- `[vdso]`: `5.26%` self
- `Runtime::refresh_store_runtime_info_context`: `5.17%` self /
  `11.06%` children
- `foldhash::quality::RandomState::hash_one::<&Vec<u8>>`: `4.34%` self /
  `5.01%` children
- `Store::drop_if_expired`: `0.16%` self / `4.67%` children
- `parse_command_args_borrowed_into`: `2.19%` self / `3.51%` children

The pre-pass std-hash stream sidecar removals are no longer a visible target.
The next profile-backed primitive should attack `internal_entries_insert` /
keyspace hashing/comparison or `refresh_store_runtime_info_context` as a
deeper structural lever, not another stream-sidecar or output-limit micro-pass.
