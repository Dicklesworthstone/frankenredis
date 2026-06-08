# frankenredis-svgvb Proof

## Target

`frankenredis-svgvb`: `[perf] SETEX/PSETEX borrowed write fast path after APPEND keep`.

The bead allowed implementation only if the SETEX/PSETEX profile showed generic
argv/dispatch/materialization or command handling as a top relevant hotspot.

## Baseline

Baseline was captured with RCH-built release-perf binaries before retaining any
production source hunk:

- Baseline commit: `5be969172`.
- Baseline binary: `/tmp/codex-fr-1cbca-closeout-target2/release-perf/frankenredis`.
- SETEX P16/300k sequential hyperfine: `2.48120804574 s +/- 0.02382461910 s`.
- PSETEX P16/300k sequential hyperfine: `3.00091345710 s +/- 0.27607253947 s`.
- SETEX P16/1M profile: `59,614.65510797251 ops/sec`, p50 `6,924.165994860232 us`,
  p95 `37,985.414965078235 us`, p99 `92,438.34507651627 us`, zero lost perf samples.

## Profile Gate

Top flat SETEX profile samples:

- `fr_store::estimate_entry_memory_usage_bytes`: `9.90%` flat, mostly under
  `Store::record_ops_sec_sample`.
- `Store::run_active_expire_cycle`: `8.95%` flat.
- `__memcmp_avx2_movbe`: `8.94%` flat, with active-expiry branches visible.
- `BTreeMap<Vec<u8>, SetValZST>::insert`: `2.99%` flat.
- `Runtime::dispatch_with_client_context`: `0.94%` flat.
- `process_buffered_frames`: `0.32%` flat.
- `fr_protocol::parse_command_args_borrowed_into`: `0.27%` flat.
- `fr_command::setex`: `0.17%` flat.

The borrowed SETEX/PSETEX fast-path premise is not profile-backed on this run.
The measured hotspot is the volatile-write expiry/stat path.

## Candidate Status

The full candidate rejection is recorded in
`artifacts/optimization/frankenredis-svgvb/candidate/rejection-report.md`.
That run covered the conservative borrowed `SETEX`/`PSETEX` write fast path,
validated the hunk while applied, proved golden transcript parity, and rejected
the candidate because the confirmation benchmarks did not meet the Score>=2.0
keep gate.

## Isomorphism

No production source hunk is retained for `svgvb`, so final observable behavior
remains the baseline:

- RESP reply bytes and ordering unchanged.
- TTL parse, overflow, expiration side effects, and fallback states unchanged.
- Command stats, slowlog, latency histogram, AOF/replication/notification gates,
  and client tracking semantics unchanged.
- Floating-point and RNG state unchanged.

The candidate golden transcript matched baseline with SHA-256
`dc3d47345c58e9839e6aa57875e4b3473379bc218bcc240c5b45907f8cb00dd7`. Artifact
SHA-256 values are recorded in `artifacts.sha256`.

## Decision

Rejected. Candidate P16/300k was only `1.08x +/- 0.23`, and reversed P16/1M
favored baseline `1.03x +/- 0.05`; Score `0.0` because the confirmation run
favored baseline.

Next primitive: create/claim a profile-backed TTL index/active-expire bead.
The current profile does not justify another borrowed SETEX/PSETEX argv lever.
