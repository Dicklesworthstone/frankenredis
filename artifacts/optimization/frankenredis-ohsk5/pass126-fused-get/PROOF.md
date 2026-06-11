# frankenredis-ohsk5.24 proof

## Lever

Profile-backed target: GET/P16/C50 on current `main` still showed RESP argv parsing in the server hot path after `frankenredis-ohsk5.23` rejected the expiry-deadline guard. The kept lever adds a canonical RESP GET packet recognizer for:

```text
*2\r\n$3\r\nGET\r\n$N\r\n<key>\r\n
```

The recognizer only bypasses generic borrowed argv parsing. It still calls the existing `Runtime::execute_plain_get_borrowed_into`, so policy gates, active expiry, store lookup, command accounting, slowlog/latency/errorstats, output suppression, lazy-expiry propagation, RESP2/RESP3 encoding, and wrongtype handling stay on the pre-existing path. Noncanonical frames and disabled states fall back to `parse_command_args_borrowed_into`.

## Baseline

- Commit before candidate: `2e317a07f`
- Build: `rch exec -- cargo build -p fr-server -p fr-bench --profile release-perf`
- RCH mode: local fallback (`no admissible workers`)
- Baseline hyperfine GET/P16/C50/1M: `1.024 s +/- 0.053 s`
- Baseline server profile GET/P16/C50/3M: `1,417,827.366 ops/sec`, p50 `518us`, p95 `808us`, p99 `1032us`
- Baseline hot rows: `clock_gettime` `3.62%`, `execute_plain_get_borrowed_into` `1.64%`, `parse_command_args_borrowed_into` `1.32%`, `process_buffered_frames` `0.68%`

## Isomorphism

- Golden transcript covers GET hit, missing GET in RESP2, wrongtype GET, HELLO 3, missing GET in RESP3, and QUIT.
- Baseline response sha256: `a495e93f2a194534097dcb3b96bdc8a9fa52f27a305a14f57ad394fbb53adfb4`
- Candidate response sha256: `a495e93f2a194534097dcb3b96bdc8a9fa52f27a305a14f57ad394fbb53adfb4`
- Ordering: unchanged; the server loop consumes exactly one parsed packet and writes through the same `conn.write_buf`.
- Tie-breaking, floating point, and RNG: not involved in packet recognition. LFU RNG behavior is preserved because the candidate calls the same runtime/store GET executor after recognition.
- Expiry: unchanged; `execute_plain_get_borrowed_into` still runs the active-expire cycle and `Store::get_string_bytes`.

## Validation

- `rch exec -- cargo test -p fr-server borrowed_plain_get_packet_parser -- --nocapture`
- `rch exec -- cargo check -p fr-server --all-targets`
- `rch exec -- cargo clippy -p fr-server --all-targets -- -D warnings`
- `cargo fmt -p fr-server -- --check`
- `rch exec -- cargo build -p fr-server -p fr-bench --profile release-perf`
- `ubs crates/fr-server/src/main.rs` returned the pre-existing broad
  `fr-server` inventory (panic/unwrap/test helpers, token-comparison
  heuristics, TcpStream lifecycle inventory, etc.); no finding pointed at the
  new canonical GET packet recognizer or fallback helper.

Compiler, test, clippy, fmt, and release-perf validation passed. RCH fell back locally for the Rust commands because no worker was admissible.

## Performance

Paired GET/P16/C50/1M:

- Baseline: `964.2 ms +/- 56.0 ms`
- Candidate: `868.5 ms +/- 37.3 ms`
- Candidate speedup: `1.11x +/- 0.08`

Reversed GET/P16/C50/1M:

- Candidate: `924.6 ms +/- 46.0 ms`
- Baseline: `1.079 s +/- 0.020 s`
- Candidate speedup: `1.17x +/- 0.06`

Post-keep profile GET/P16/C50/3M:

- Candidate: `1,444,084.863 ops/sec`, p50 `509us`, p95 `801us`, p99 `1098us`
- `parse_command_args_borrowed_into` is gone from the top self list.
- Remaining visible user-space rows include `execute_plain_get_borrowed_into`, `Store::get_string_bytes`, `plain_borrowed_default_key_read_allows`, `encode_bulk_string_slice`, `foldhash::hash_one`, and `process_buffered_frames`.

Score: `6.0` (`Impact 3.0 x Confidence 4.0 / Effort 2.0`), above the `>=2.0` keep gate.

## Next route

Filed `frankenredis-ohsk5.26` for a single-probe GET read capsule that fuses lazy expiry decision, keyspace hit/miss accounting, entry touch/LFU, wrongtype check, and borrowed value exposure without changing observable Redis semantics.
