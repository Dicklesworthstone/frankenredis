# frankenredis-ohsk5.19 pass122 rejection

Target: GET/P16/C50 output/parser path after SET control-path rejections.

Baseline current main `63432a805`:
- Build: `rch exec -- cargo build -p fr-server -p fr-bench --profile release-perf`
- Hyperfine GET/P16/C50/1M: `721.5 ms +/- 31.8 ms`
- Last bench run: `1644704.3889497465 ops/sec`, p50 `449us`, p95 `690us`, p99 `931us`

Profile current main, GET/P16/C50/3M:
- `1703567.0949948633 ops/sec`, p50 `437us`, p95 `639us`, p99 `890us`
- Top flat rows: kernel unresolved `3.86%`, vdso `3.30%`, `parse_command_args_borrowed_into` `1.69%`, `execute_plain_get_borrowed_into` `1.29%`, `process_buffered_frames` `1.28%`, `plain_borrowed_default_key_read_allows` `1.21%`, `CommandHistogramTracker::record_canonical_with_kind` `1.13%`, `Store::get_string_bytes` `0.76%`, `encode_bulk_string_slice` `0.69%`.

Candidate:
- Add direct canonical `*2 GET key` packet parser before the generic borrowed argv parser.
- Source hunk saved at `candidate/rejected-source-hunk.patch`.
- Production hunk removed after failing the Score gate.

Correctness:
- Focused parser tests passed while candidate was applied: `cargo test -p fr-server borrowed_plain_get_packet_parser -- --nocapture`.
- Raw RESP transcript covered SET setup, canonical GET hit, lowercase GET hit, GET miss, CLIENT REPLY OFF suppressed GET, CLIENT REPLY ON, and PING.
- Request sha256: `a31ff9968e7bcb14bf1633977229d1ef67e916023831355c3d9971d1369207b0` (`268` bytes).
- Current response sha256: `0f2a76c0720ee2c804019a644ddbc43e6786c7133cc8dd53799b3081aa81deb9` (`40` bytes).
- Candidate response sha256: `0f2a76c0720ee2c804019a644ddbc43e6786c7133cc8dd53799b3081aa81deb9` (`40` bytes).
- Byte-identical: yes.

Benchmark:
- Paired current then candidate: current `785.1 ms +/- 68.4 ms`, candidate `746.0 ms +/- 31.7 ms`, candidate `1.05x +/- 0.10`.
- Reversed candidate then current: candidate `685.8 ms +/- 9.3 ms`, current `757.1 ms +/- 36.3 ms`, candidate `1.10x +/- 0.06`.

Decision:
- Reject. The best confirmed signal is below the required `>=1.20x` target and below Score `2.0`.
- Next route should not repeat direct single-command parser microlevers. The GET profile points at broader syscall/output batching, command accounting, or a different network/output ownership model.
