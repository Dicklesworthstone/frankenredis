# coralox-pass181 keyed-values integer encoding

Bead: frankenredis-ohsk5.49

Target evidence:
- `scripts/perf_gap_dashboard.sh --bin target-coralox-pass181-baseline/release/frankenredis --no-build -n 400000 -P 16 -c 50 --reps 3`
- Baseline dashboard showed LPUSH as the top residual gap: Redis 1,069,518 req/s vs FrankenRedis 963,855 req/s, Redis/FR 1.11x.
- `perf_event_paranoid=4` blocked perf data capture. Child-owned GDB sampling during 5M pipelined LPUSH hit `__libc_send` / `TcpStream::write` and `epoll_wait`, routing the pass to the output/event-loop surface.

Lever attempted:
- Directly encode successful borrowed `LPUSH` / `RPUSH` / `SADD` integer replies into the connection output buffer, avoiding temporary `RespFrame::Integer` construction.
- One semantic hazard was found before benchmarking: `FastEncodedReply` must apply `CLIENT REPLY` suppression inside the runtime method. The candidate was fixed and tested for `SKIP` and `OFF` before release benchmarking.

Behavior proof:
- Focused tests passed:
  - `rch exec -- cargo test -p fr-protocol encode_integer_matches_frame_encoder_for_boundaries -- --nocapture`
  - `rch exec -- cargo test -p fr-runtime plain_keyed_values_borrowed -- --nocapture`
- Golden input SHA256: `a3b391acf4761d7c24bc47394af4ca0bb713740ff1f6729088494051e01c1022`
- Baseline output SHA256: `9008a2b68147558be1aafa86186ca7612dd801e237c24733274fe1546b154012`
- Candidate output SHA256: `9008a2b68147558be1aafa86186ca7612dd801e237c24733274fe1546b154012`
- Golden transcript covered integer replies, duplicate SADD, WRONGTYPE, `CLIENT REPLY SKIP`, `CLIENT REPLY OFF`, and final list/set state.
- Ordering/tie-breaking: pipelined command order and list element order matched byte-for-byte.
- Floating point/RNG: not exercised by this command surface.

Benchmark:
- Baseline release build: RCH remote `vmi1167313`.
- Candidate release build: RCH remote `vmi1227854`.
- Command: hyperfine, 9 runs, 2 warmups, fresh server per run, `redis-benchmark -t lpush -n 1000000 -c 50 -P 16 -r 100000 -q`.
- Baseline: `1.522 s +/- 0.030 s`.
- Candidate: `1.552 s +/- 0.040 s`.
- Result: baseline was `1.02x +/- 0.03x` faster than candidate.

Decision:
- Rejected. Score is below 2.0 and the candidate was slower.
- Source/test changes were reverted; only this evidence and bead closeout are retained.
- Next route: stop micro-tuning reply-frame construction for keyed-values writes and attack the deeper output/event-loop primitive: batched syscall/write-buffer flushing with a profile-backed target.
