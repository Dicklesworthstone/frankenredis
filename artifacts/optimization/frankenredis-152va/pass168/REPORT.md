# frankenredis-152va Pass 168 Report

## Scope

- Bead: `frankenredis-152va`
- Target: LPUSH residual beyond the write-syscall floor.
- Commit tested: `650ec9ac2` (`origin/main` at pass start).
- Decision: evidence-only; no production source change kept.

## Baseline

RCH-built current main was benchmarked with `fr-bench` on LPUSH P16/C50/n300k:

- Throughput: `521374.82 ops/sec`
- Latency: p50 `1241us`, p95 `2105us`, p99 `7059us`, p999 `27423us`
- Hyperfine LPUSH P16/C50/n1M: `2.916s +/- 0.762s` over 7 runs

## Profile Evidence

Syscall profile for LPUSH P16/C50/n300k:

- `sendto`: 42.52%, 18,750 calls
- `epoll_wait`: 26.81%, 8,496 calls
- `recvfrom`: 19.26%, 18,801 calls
- `epoll_ctl`: 0.16%, 104 calls

This matches the previous one-send-per-pipeline-batch output floor. The remaining direct output/coalescing/parser microfamilies are already rejected by earlier passes, so this pass did not make a source edit without userspace attribution.

Userspace sampling was blocked:

- `/proc/sys/kernel/perf_event_paranoid=4` blocks `perf` and `samply`.
- ptrace attach was blocked by host policy.
- `strace -k` was attempted as a child-owned fallback, but it reduced LPUSH P16/C50/n120k throughput to `4174.93 ops/sec`, so it is too distorted for keep/reject evidence.

## Golden Proof

- Input SHA256: `9c7598c7e7fa7a0f50ee535b2e3716967504e9bb61f545c192490e503762d15b`
- Output SHA256: `d79bfd8ca34229a740bd36ff60e16d412c03f5dde51ed571c6f63fd5879ef74d`

No production source changed, so reply ordering, list order, client reply suppression, RNG/tie-breaking, and floating-point behavior remain isomorphic to current main. The transcript is kept as the next-pass comparison anchor.

## Next Primitive

The next optimization pass should first restore userspace attribution, then choose one deeper safe-Rust lever:

- Region/arena command-batch metadata to remove per-frame temporary allocation.
- Zero-copy command packet layout that carries parsed RESP ranges through dispatch.
- Vectorized RESP delimiter scanning for multibulk frame boundaries.
- Event-loop batch scheduling tuned around observed `sendto`/`recvfrom`/`epoll_wait` balance.

Do not repeat direct integer reply encoding, chunk-front-shift, exact-command parser, borrowed member-copy, or output coalescing without a new profile proving that family is again dominant.
