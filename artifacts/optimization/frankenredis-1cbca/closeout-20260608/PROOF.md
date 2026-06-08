# frankenredis-1cbca closeout proof

## Target

- Bead: `frankenredis-1cbca`.
- Claim: pipeline=16 throughput gap came from per-reply `write()` syscalls and
  should be attacked with scatter/gather `writev`.
- Current source: `fb9a8f245` built from a minimal clean source copy at
  `/data/projects/.scratch/frankenredis-1cbca-min2-fb9a8f245-20260608T0839`.
- Build command: `rch exec -- env CARGO_TARGET_DIR=/tmp/codex-fr-1cbca-closeout-target2 cargo build --profile release-perf -p fr-server -p fr-bench`.
- Binaries:
  - `frankenredis` sha256 `8bb4111bfff9ec49d793ee0a5672a2acb3586fe705d04bc0f6269bf46d177e48`.
  - `fr-bench` sha256 `dd38e2551c28502d027a7594d8ee4dd1834bf5a2d2288e892c08c504a38ef81b`.

## Fresh Baseline

GET P16 / 300k / 50 clients / keyspace 10000:

- Hyperfine mean: `529.2 ms +/- 21.0 ms`.
- Artifact: `baseline-get-p16-300k-hyperfine.json`.

GET P16 / 1M profile run:

- Throughput: `710915.00 ops/sec`.
- p50: `1057 us`.
- p95: `1495 us`.
- p99: `1827 us`.
- Perf samples: `14596`, lost samples `0`.
- Children profile: `ClientConnection::try_flush` `22.68%`, `__send`
  `22.47%`.
- Flat profile: `Runtime::refresh_store_runtime_info_context` `5.26%`,
  `Value::string_owned` `4.99%`, `_mi_page_malloc_zero` `3.90%`,
  `parse_command_args_borrowed_into` `1.06%`.

SET P16 / 100k syscall count:

- `sendto` calls: `6250`.
- Expected if batched at pipeline depth 16: `100000 / 16 = 6250`.
- This disproves the stale per-reply-write premise for the current server: the
  server already emits one send per coalesced pipeline batch here, not one send
  per reply.
- Artifact: `baseline-set-p16-100k-server-strace.txt`.

## Behavior Proof

Golden command stream:

- `FLUSHALL`
- `PING`
- `SET fr1cbca:golden-key bar`
- `GET fr1cbca:golden-key`
- `QUIT`

SHA-256:

- Request: `c764e005ccba31757550a4e105bd038985b265fa5640c3133bcdc8237055cba2`.
- Output: `6a4fff4d2af35aef8a0d06970dffb1e4f12afc4b0f5fe0880be4fc727ed26acd`.

Observed output bytes are, in order:

- `+OK`
- `+PONG`
- `+OK`
- bulk string `bar`
- `+OK`

Isomorphism: no production source hunk is retained for this bead, so final
runtime behavior is the measured baseline behavior. Reply ordering, byte
content, command side effects, tie-breaking, floating-point, RNG, expiry, and
replication-visible command ordering are unchanged.

## Prior Lever Evidence

The direct safe-Rust `write_vectored` response-fragment family was already
tested in `artifacts/optimization/icywolf-perf-20260603-pass29-response-segments/REJECTION_PROOF.md`:

- Baseline SET P16 paired hyperfine: `1.660 s +/- 0.028 s`.
- Candidate response-segment `write_vectored`: `1.772 s +/- 0.049 s`.
- Baseline was `1.07x +/- 0.03x` faster.
- Golden output sha256 matched, but the performance gate failed.

## Decision

Reject the `frankenredis-1cbca` writev/scatter-gather lever for current main and
close the bead. Score `0.5 = Impact 1 x Confidence 1 / Effort 2`: the profile
still shows send cost, but syscall count proves this bead's specific premise is
obsolete, and the already-tested vectored-reply implementation regressed.

Next route is not another writev wrapper. Attack a different profile-backed
primitive: per-command CPU and allocation surfaces in the P16 path, especially
runtime metadata refresh, value materialization, parser/command metadata, or a
safe arena/slab reply model only if a fresh profile shows it removes whole-class
work instead of splitting an already-coalesced buffer.
