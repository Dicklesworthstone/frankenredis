# Pass 105: string canonicalization fast-gate rejection

## Bead

- `frankenredis-c4u8o`: `[perf] Fast-gate non-integer string canonicalization in SET hot path`
- Agent: `TealOtter`
- Profiled commit: `bbd58c2c6`
- Decision: reject under the Score>=2.0 keep gate

## Profile-backed target

Pass 104 rebuilt current head and profiled SET P16/1M. The top user-space store rows were:

- `fr_store::canonical_string_value`: 8.85% self
- `<fr_store::Store>::set_plain_borrowed`: 8.02% self

The candidate was restricted to one lever: avoid calling strict `parse_i64` when the value's first byte cannot start a Redis integer-encoded string.

## Candidate

Candidate-only scratch hunk:

- Add `integer_encoding_candidate(&[u8]) -> bool`.
- Keep `parse_i64` unchanged.
- In `canonical_string_value`, call `parse_i64` only for `0`, `[1-9]...`, or `-[1-9]...`.

## Isomorphism proof

- Ordering preserved: yes. This lever only changes whether impossible integer strings enter the strict parser; key ordering, command ordering, reply ordering, propagation, AOF, and replication paths are untouched.
- Tie-breaking unchanged: yes. No sorted-set score, lexicographic comparison, hash iteration, or ordering surface changed.
- Floating-point: N/A.
- RNG seeds: unchanged.
- Redis integer-encoding semantics: preserved. Candidate still uses the same `parse_i64` for all valid-looking integer candidates, so overflow, leading-zero rejection, `-0` rejection, and strict sign handling stay identical.
- Golden outputs: baseline and candidate RESP transcripts matched byte-for-byte for 27 commands covering integer-like values, non-integer values, leading zeros, `-0`, invalid `INCR`, TTL clearing on overwrite, ordered `MGET`, and `DEL`.

Golden transcript SHA-256:

```text
ebb4679a12c0269cd596406596e1cb28ffff9f52e8aa2ffd75fa54027da39afd  golden-baseline.resp
ebb4679a12c0269cd596406596e1cb28ffff9f52e8aa2ffd75fa54027da39afd  golden-candidate.resp
```

## Validation

- `rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`: passed in the scratch worktree after formatting.
- `rch exec -- cargo test -p fr-store setvalue_intset_semantics_and_fuzz_vs_reference -- --nocapture`: passed remotely on `ovh-a`.
- `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`: passed remotely on `ovh-a`.

Release-perf binary SHA-256:

```text
a1b8b342106db8b3dd5e5fb92eebb8d6972b7d80ec2f1c32738a39e8751fce3e  baseline frankenredis
afc42924ac41684849ad892c5dca8471d669258664fdef3a810a21dcc6004772  candidate frankenredis
2460e22715b61198d3f9f19c9098d88dff20afce138cbb1055bfebf9881d5e2d  baseline fr-bench
4ec586b2fb54186235ec2258174ce0106207a4622c8bb607c78a4010e8b24f80  candidate fr-bench
```

## Benchmarks

SET P16/300k, 50 clients, warmup 2, runs 10.

Paired order:

- Baseline: `364.6 ms +/- 14.5 ms`
- Candidate: `363.3 ms +/- 13.8 ms`
- Summary: candidate `1.00x +/- 0.06` faster
- Last-run throughput: baseline `802,869.64 ops/sec`; candidate `810,933.58 ops/sec`
- Last-run p99: baseline `1498us`; candidate `1503us`

Reversed order:

- Candidate: `376.9 ms +/- 23.4 ms`
- Baseline: `373.9 ms +/- 10.9 ms`
- Summary: baseline `1.01x +/- 0.07` faster
- Last-run throughput: candidate `805,878.57 ops/sec`; baseline `788,669.12 ops/sec`
- Last-run p99: candidate `1406us`; baseline `1662us`

## Score

- Impact: 0.1
- Confidence: 4.0
- Effort: 1.0
- Score: 0.4

The profile target was real and the proof was clean, but the paired and reversed hyperfine runs were noise/tie. The candidate source hunk was not applied to the shared checkout.

## Next route

Stop micro-tuning integer canonicalization. The pass 104 profile also shows syscall/IO pressure (`sendto`, `recvfrom`, `epoll_wait`) and store overwrite cost; the next pass should attack a structurally different output/read path or key-layout primitive only if fresh profile evidence supports it.
