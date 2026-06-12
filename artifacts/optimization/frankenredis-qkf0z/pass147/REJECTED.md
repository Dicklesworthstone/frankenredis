# frankenredis-qkf0z pass147 rejection

## Profile target

Remote profile on `vmi1152480`, release-perf `fr-server`, `redis-benchmark`
SPOP after SADD prefill:

- `GenericSet::pop_index`: 21.04% self
- `mi_free`: 10.89% self
- `fr_store::estimate_entry_memory_usage_bytes`: 8.56% self
- `SetValueIter::next`: 6.45% self
- `Runtime::execute_plain_keyed_pop_borrowed`: 5.69% self

The tested lever targeted the periodic sampler cost:
`Store::record_ops_sec_sample` read RSS first and only called
`estimate_memory_usage_bytes` as a fallback when `/proc` RSS was unavailable.

## Behavior proof

The source edit was reverted after measurement, so the committed tree preserves
the prior behavior exactly.

For the candidate itself, the intended isomorphism was:

- Ordering and tie-breaking: unchanged; no set iteration, hash order, sorted-set
  comparator, command dispatch order, or reply order was modified.
- RNG: unchanged; no `next_rand` call site was touched.
- Floating point: unchanged; no numeric representation path was touched.
- Observable memory commands: unchanged in explicit `INFO memory` and `MEMORY`
  paths; only periodic RSS/peak sampling order was changed, with logical memory
  still used as the fallback if RSS read failed.

Golden workload command shape:

- SADD prefill: `redis-benchmark -t sadd -n 1500000 -c 50 -P 16 -r 20000000`
- SPOP gate: `redis-benchmark -t spop -n 1000000 -c 50 -P 16 -r 20000000`

The committed tree is source-identical to the pre-lever store code; therefore
golden output SHA is unchanged by construction for all command replies.

## Benchmark result

Baseline current binary:

- `4.934 s +/- 0.256 s`, 8 runs.

Candidate binary:

- `5.167 s +/- 0.293 s`, 8 runs.

Paired rerun on the same worker window:

- Current: `5.076 s +/- 0.272 s`, 10 runs.
- Candidate: `5.120 s +/- 0.420 s`, 10 runs.
- Hyperfine summary: current ran `1.01x +/- 0.10x` faster than candidate.

Score: `Impact 0.0 x Confidence 0.75 / Effort 1.0 = 0.0`.

Decision: reject and revert. The next profile-backed primitive is deeper set
removal work around `GenericSet::pop_index` / removal allocation churn, not more
periodic-sampler tuning.
