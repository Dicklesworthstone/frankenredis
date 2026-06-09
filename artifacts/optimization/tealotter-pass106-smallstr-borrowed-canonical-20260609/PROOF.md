# Pass 106: borrowed SmallStr canonicalization rejection

## Bead

- `frankenredis-9ybcb`: `[perf] Borrowed SmallStr canonicalization for plain SET overwrite`
- Agent: `TealOtter`
- Base commit: `e921b0878`
- Decision: reject under the Score>=2.0 keep gate

## Profile-backed target

Fresh pass106 profile on current pushed `main` (`e921b0878`) showed that a reply/output wedge was not profile-backed:

- `RespFrame::encode_into`: about `0.26%` self
- `ClientConnection::try_flush`: about `0.09%` self
- `strace -f -c` SET P16/300k: `sendto` count `18,750`, which is exactly one send per 16-command pipeline batch for 300k commands

The material user-space rows remained in store overwrite/value layout:

- `<fr_store::Store>::set_plain_borrowed`: `9.67%` self
- `fr_store::canonical_string_value`: `9.06%` self

## Candidate

One lever only:

- Add `SmallStr::from_slice(&[u8])` so small borrowed strings can be built inline without a temporary `Vec`.
- Add `canonical_string_value_borrowed(&[u8])`, preserving the exact strict `parse_i64` rules.
- Use it only in `Store::set_plain_borrowed`'s existing-key overwrite path.

This was intentionally different from rejected `frankenredis-c4u8o`: it did not skip integer parsing; it removed the temporary allocation/layout hop for borrowed small strings.

## Isomorphism proof

- Ordering preserved: yes. No key ordering, command order, reply order, propagation, AOF, or replication path changed.
- Tie-breaking unchanged: yes. No sorted-set score, lexicographic comparison, hash iteration, SCAN order, or RANDOMKEY sidecar logic changed.
- Floating-point: N/A.
- RNG seeds: unchanged.
- Redis integer encoding: preserved. `canonical_string_value_borrowed` calls the same `parse_i64` over the same bytes before choosing `Value::Integer` vs `Value::String`.
- Sidecars preserved: focused test compared generic `set` and borrowed `set_plain_borrowed` for TTL clearing, LFU frequency, expiry counters, dirty count, state digest, and inline storage after a small non-integer overwrite.
- Golden outputs: baseline and candidate RESP transcripts matched byte-for-byte for integer and non-integer SET values, `OBJECT ENCODING`, `INCR` error identity, TTL clearing, `MGET`, `DEL`, and `QUIT`.

Golden transcript SHA-256:

```text
bdc37b9aa720451455c0cc174c31917de36121619a176b5d1087c69a57d4c61d  golden-baseline.resp
bdc37b9aa720451455c0cc174c31917de36121619a176b5d1087c69a57d4c61d  golden-candidate.resp
```

## Validation

- `rustfmt --edition 2024 --check crates/fr-store/src/lib.rs`: passed.
- `rch exec -- cargo test -p fr-store set_plain_borrowed_matches_set_for_existing_volatile_lfu_string -- --nocapture`: passed remotely on `vmi1227854`.
- `rch exec -- cargo build --profile release-perf -p fr-server -p fr-bench`: passed remotely on `ovh-a`.
- `sha256sum -c golden.sha256`: passed.

Release-perf binary SHA-256:

```text
f1f0b7667df7ba33ebdd24253680c84cae876fad9cc6aa73b607eaf692159f44  baseline frankenredis
831a9ed9d9cd23aab5ebad8c62e58f0f84e7f5499f3f45fd8bbab891643c91b3  candidate frankenredis
560a9b63f185778076be4d8c40589b74691af747307c85891f6febd1ccd7270a  baseline fr-bench
0a6a0012ff045e0a2e76bc2bc92cd250417990e9c62e5104c42e3414f4704585  candidate fr-bench
```

## Benchmarks

SET P16/300k, 50 clients, warmup 3, runs 10. Same baseline `fr-bench` binary drove both servers.

Paired order:

- Baseline: `365.2 ms +/- 14.0 ms`
- Candidate: `369.8 ms +/- 11.5 ms`
- Summary: baseline `1.01x +/- 0.05` faster
- Last-run throughput: baseline `786,710.57 ops/sec`; candidate `800,012.25 ops/sec`
- Last-run p99: baseline `1783us`; candidate `1654us`

Reversed order:

- Candidate: `359.0 ms +/- 14.6 ms`
- Baseline: `355.2 ms +/- 8.4 ms`
- Summary: baseline `1.01x +/- 0.05` faster
- Last-run throughput: candidate `872,374.34 ops/sec`; baseline `869,753.30 ops/sec`
- Last-run p99: candidate `1443us`; baseline `1265us`

## Score

- Impact: 0.0
- Confidence: 4.0
- Effort: 1.0
- Score: 0.0

The source hunk was proof-clean but did not produce a wall-clock win. It was not applied to shared `main`.

## Next route

Two consecutive store/canonicalization micro-levers (`c4u8o`, `9ybcb`) have failed. Pass107 should not continue this local family. Re-run alien-graveyard/artifact selection against the current profile and attack a structurally larger primitive, likely a key-layout/fingerprint or batch command-packet design that removes repeated key comparisons/probes as a class.
