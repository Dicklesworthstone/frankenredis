## Pass 2 Keep: Borrowed Canonical SET Value Construction

Bead: `frankenredis-zhphm.1`

Parent context: `frankenredis-zhphm` remains open for full safe io-thread offload. This child closes one profile-backed store-hotspot slice.

### Profile Target

Baseline profile: `artifacts/optimization/frankenredis-zhphm/pass1/baseline/baseline-set-p16-1m-perf-self.txt`

- Workload: `fr-bench --workload set --clients 50 --pipeline 16 --requests 1000000 --datasize 3`.
- Hot symbol: `fr_store::canonical_string_value`, `11.68%` self.
- Call path: mostly under `Store::drop_if_expired` / borrowed plain `SET`.

Alien primitive:

- Region/arena-style hot-path allocation avoidance: do not materialize transient owned payloads when the current request already owns a valid borrowed byte slice.
- Small-object inline construction: preserve existing `SmallStr` inline layout while constructing from a slice without a temporary `Vec`.

### Lever

Construct `Value` directly from borrowed plain-`SET` payloads:

- Integer-looking bytes parse directly from `&[u8]` into `Value::Integer`.
- Non-integer bytes become the same `Value::String(SmallStr)` representation as before.
- Missing-key borrowed `SET` inserts directly instead of routing through the owned `set(Vec, Vec, None, ...)` path.
- Existing-key borrowed `SET` overwrites with the slice-backed canonical constructor.

### Isomorphism Proof

- Ordering: unchanged. The server still executes the same single-threaded command path in the same parser/dispatch order.
- Tie-breaking: none introduced or changed.
- Floating-point: none touched.
- RNG: none touched; LFU and random-key state are not sampled by the new constructor.
- Redis-visible encoding: golden transcript covers `OBJECT ENCODING` for numeric, nonnumeric, and noncanonical numeric values.
- Bytes: golden transcript covers `SET`, `GET`, `CLIENT REPLY SKIP`, `OBJECT ENCODING`, and `QUIT`.

Golden sha256:

- Transcript: `8cb1a5c7ac9e2863352f18c22a2c210d517e4d96aba6337646dac0bf5e317e57`
- Baseline output: `54c98f8078ffcd6529f9a9b2e555cbd965961e6548c21bcc85566356916210ba`
- Candidate output: `54c98f8078ffcd6529f9a9b2e555cbd965961e6548c21bcc85566356916210ba`

Focused parity tests:

- `cargo test -p fr-store set_plain_borrowed_matches_set -- --nocapture`
- `set_plain_borrowed_matches_set_for_existing_volatile_lfu_string`
- `set_plain_borrowed_matches_set_for_new_integer_and_string_values`

### Benchmark Evidence

Baseline/candidate binaries were built through `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`.

| Run | Baseline mean | Candidate mean | Ratio |
| --- | ---: | ---: | ---: |
| Standalone | 1.327554s +/- 0.062030s | 1.243681s +/- 0.048191s | 1.067x |
| Paired | 1.290546s +/- 0.041483s | 1.224995s +/- 0.028100s | 1.054x +/- 0.04 |
| Reversed | 1.273594s +/- 0.049710s | 1.258108s +/- 0.057628s | 1.012x +/- 0.06 |

The 16-run retry failed before producing timing evidence due repeated server-start harness flake; traced standalone baseline startup on a fresh port succeeded and was not counted as a performance datapoint.

### Re-profile After Keep

Candidate profile: `artifacts/optimization/frankenredis-zhphm/pass2/profile-after/candidate-set-p16-1m-perf-self.txt`

- `canonical_string_value_from_slice`: `1.68%` self.
- Remaining visible target: broader `Store::set_plain_borrowed` (`20.49%` self in this sample), especially propagation/no-expiry handling and time calls.

### Score

Impact `3.0` x Confidence `0.75` / Effort `1.0` = `2.25`.

Decision: keep.
