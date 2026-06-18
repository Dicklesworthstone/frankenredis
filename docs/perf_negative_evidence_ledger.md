# Performance Negative-Evidence Ledger (frankenredis vs redis 7.2.4)

Purpose: stop the perf agents (cc, cod-b, CrimsonFalcon, …) from re-treading levers
already proven to NOT win, and record where the real residual gaps live + who owns them.
Append measured results; never delete a row — a "tried, didn't win" entry is the point.

Convention: ratios are fr/redis (>1.0 = fr slower / more RAM). "Measured" = ran a real
release A/B; "Reasoned" = algorithmic certainty without a release bench (cargo-check-only
turns). Keep claims honest — mark which.

## Established baseline (do NOT re-litigate)
- **Throughput is parity-or-faster** on hot commands (GET ~1.2x faster, SET ~1.3x faster,
  HSET 1.13x the lone residual). The historical "fr 2x slower pipelined" (ohsk5) headline
  is CLOSED. Tools: scripts/profile_hot_path.sh, scripts/perf_gap_dashboard.sh.
- **Cold-command dispatch**: ~20 cold commands were the real pipelined-CPU gap (per-cmd
  dispatch machinery, not alloc) — all converted to borrowed fast paths; now 0.81–1.95x.
  Pattern: add a PlainXCmd enum + execute_plain_X_borrowed. (See git log perf(fr-runtime).)
- **Reply path is mature**: borrowed zero-copy args, itoa2 two-digit table (push_i64/usize),
  direct-encode header helpers (push_array_header/push_map_header), 30+ borrowed_plain_*
  fast paths in fr-server. decimal_*_len now branchless ilog10 (e4fu8).

## Rejected levers — measured REGRESSION or no-win (do NOT retry)
| Lever | Result | Why |
|---|---|---|
| Hand-rolled large-buffer reuse / malloc-avoidance | 0.77–0.93x (REGRESSION) | mimalloc (fr default) already recycles large buffers; hand reuse fights it. A/B before trusting any malloc-avoidance lever. |
| ChunkedList → VecDeque / decode-path rewrite for list RESTORE | 0.53x (SLOWER) | per-element alloc is the cost; VecDeque didn't help. Real lever = packed-listpack-node ChunkedList (99fwc), not container swap. |
| SWAR/SIMD on memory-bound byte loops (max/copy/fill, HLL register-max) | ~1.0x (0.94x for HLL) | only COMPUTE-bound loops win (popcount/CRC/bitwise = 4–13x). Check compute-vs-memory first. Clean-crate compute kernels already done. |
| used_memory via counting-allocator | ~7% throughput hit + wrong target | estimate_memory_usage_bytes MODELS redis; counting-alloc measures fr's actual RAM (a different number). RSS lags frees. Don't "fix" the model with real accounting. |
| zadd 8% pipelined gap | WONTFIX (x1zbp) | distributed across dispatch, no single hot spot. |

## Real residual gaps (structural; mind ownership before touching)
| Gap | Ratio | Owner / bead | Note |
|---|---|---|---|
| Keyspace dict RAM | ~4.5–5.4x | cod-b / uhthd, also project_keyspace_ram_gap | ordered_keys is the COST of deterministic sorted SCAN (encoded in core_scan.json + tests). Cutting it to ~2x = a SCAN-semantics design reversal (hash-order + reverse-binary cursor), multi-day, all-or-nothing. Not a clean opt. |
| zset RAM | ~1.54x | CoralOx / uybhq | peni2 (Arc<[u8]> shared members) shipped; residual is structural (dict IndexMap + ordered BTreeMap each hold score+node overhead). Measure fresh-process RSS, not used_memory. |
| list DUMP | ~1.7–2.7x | open / 99fwc | ChunkedList re-synthesizes Owned chunks per DUMP. Lever = make push build Listpack chunks (infra exists). BIG. |
| Per-cmd CPU (deep pipeline, remaining) | varies | cod-b / ohsk5 | the cold-cmd vein is done; remaining is in their domain. |
| "Close ALL gaps, pure safe Rust" | — | CrimsonFalcon / gu5nf | broad; coordinate before hot-path edits to fr-store/fr-runtime/fr-server. |

## Methodology gotchas (cost real hours)
- **A stray PSYNC replica on single-threaded fr craters ALL bench ratios** — always check
  `INFO replication | connected_slaves:0` before trusting a regression.
- **rch builds the LOCAL tree** (perpetually behind origin when committing from worktrees)
  and does NOT sync the linked binary — copy from /data/tmp/cargo-target, sanity-check vs a
  known-fixed behavior before trusting any "divergence".
- **Config pollution on the shared oracle** (e.g. list-max-listpack-size set by a peer) →
  false parity-gate fails. Reset config / launch a fresh oracle.
- **Pipeline the DUMP/throughput bench** — single-conn is syscall-masked and hides the gap.
- release builds only for perf; debug ratios are meaningless.

## Open clean levers (unclaimed, reasoned-promising)
- decimal_*_len branchless ilog10 — DONE (e4fu8, reasoned; criterion A/B pending batch).
- (add here as found) — prefer clean crates (fr-protocol, fr-persist non-LZF) not under a
  peer's active reservation; bench A/B in release before claiming a win.
