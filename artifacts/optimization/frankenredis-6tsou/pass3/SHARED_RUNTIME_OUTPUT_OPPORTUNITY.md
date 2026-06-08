# frankenredis-6tsou Pass 3 - Shared Runtime/Output Opportunity

## Scope

Mission: shared output/runtime hotspot review only.

Production Rust files were intentionally not edited in this pass. This note
uses the Pass 2 clean-HEAD GETSET profile as the baseline evidence because it
shows command-specific GETSET work is cold and the remaining weight is shared.

Baseline profile source:

- Clean HEAD: `1288f679e15d275b1a971f86a7c292118e379928`
- Worker: `vmi1149989`
- Binary sha256: `1f706f06749c15a6fe7b733b85a05608c952a806af3d317cc6f933b87e05679d`
- Workload: `getset-hit`, 300000 requests, 50 clients, pipeline 16, keyspace 10000
- Hyperfine: `2.350s +/- 0.307`, median `2.219s`
- Profile run: `98,038.7 ops/sec`, p50/p95/p99 pipeline latency
  `6984us / 17347us / 25757us`

## Profile-backed hotspots

Pass 2 server children rows:

| Hotspot | Children | Self | Read |
| --- | ---: | ---: | --- |
| `ClientConnection::try_flush` | 6.21% | 0.00% | mostly `__send` / syscall path |
| `Runtime::execute_frame_internal` | 2.99% | 1.35% | shared runtime dispatch |
| `Runtime::refresh_store_runtime_info_context` | 2.64% | 1.45% | unconditional per-command INFO context refresh |
| `Store::internal_entries_insert` | 1.35% | 0.63% | real write/store work |
| `process_buffered_frames` | 1.32% | 0.63% | shared parse/dispatch loop |
| `dispatch_with_client_context` | 1.31% | 0.59% | store client context bridge |
| `parse_command_args_borrowed_into` | 1.10% | 0.79% | shared RESP multibulk parse |
| `fr_command::classify_command` | 1.06% | 0.36% | repeated command classification |
| `fr_command::command_table_index` | 1.05% | 0.41% | command metadata lookup |

Important negative evidence:

- `try_flush` is not showing Rust-side buffer manipulation as the expensive
  part. Its children collapse through `__send` / `__syscall_cancel`, while
  `Vec::drain`/memmove is not a top self row.
- The server already coalesces all currently buffered pipeline replies before
  attempting a nonblocking flush from `handle_readable`, and only arms
  `WRITABLE` for partial/WouldBlock cases.
- A write-buffer cursor/ring lever would target a non-hot local copy cost, not
  the profiled syscall cost.

## Opportunity Matrix

| Lever | Impact | Confidence | Effort | Score | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Conditional/on-demand `refresh_store_runtime_info_context` for observability commands | 2 | 4 | 3 | 2.67 | Recommend next |
| Output buffer cursor to avoid `Vec::drain` after partial writes | 1 | 2 | 2 | 1.00 | Reject |
| Defer all readable-path flushing to writable events | 2 | 2 | 4 | 1.00 | Reject for now |
| Command metadata/classification packet threaded through runtime + fr-command | 3 | 3 | 5 | 1.80 | Route to Pass 4 design/profiling |
| Specialized GETSET/SETNX/GETDEL borrowed write fast paths | 1 | 2 | 3 | 0.67 | Already rejected family |

## Recommended implementation

Implement one lever next: make `Runtime::refresh_store_runtime_info_context`
conditional and on-demand.

Current shape:

- `Runtime::execute_dispatch` increments `stat_total_commands_processed`, updates
  session timing, then calls `refresh_store_runtime_info_context()` for every
  command.
- `refresh_store_runtime_info_context()` recomputes INFO-facing tracking counts,
  client-tracking observer totals, maxmemory/persistence flags, and replication
  backlog memory estimates.
- Those refreshed store fields are consumed by observability replies such as
  `INFO` and `MEMORY STATS`, not by ordinary GETSET/SET/GET traffic.

Exact implementation recommendation:

1. Keep the hot-path updates that must remain per command:
   `stat_total_commands_processed`, session `last_command_name`,
   `last_argv_len_sum`, `last_interaction_ms`, `is_read_only_replica`, and
   read/write processed counters.
2. Move the unconditional `refresh_store_runtime_info_context()` call behind a
   predicate such as `command_needs_runtime_info_context(argv)`.
3. Return true for `INFO` and `MEMORY` initially. Over-matching all `MEMORY`
   subcommands is acceptable because the command is cold and it avoids missing
   `MEMORY STATS`/future memory observability paths.
4. Consider true for `CLIENT` subcommands only if implementation inspection
   finds a CLIENT reply consuming the refreshed store fields. Current hot
   evidence does not require CLIENT to be in the first lever.
5. Leave config/maxmemory mutation sites responsible for maintaining
   `store.maxmemory_bytes_live`; current code already updates this in
   maxmemory configuration paths. The on-demand refresh remains the observation
   safety net before `INFO memory`.

Why this is the best next lever:

- It directly targets a top shared profile row: `2.64%` children / `1.45%` self.
- It removes work from every ordinary command without changing command
  execution, reply construction, key ordering, expiration ordering, AOF
  propagation, or RNG/floating-point behavior.
- The proof boundary is narrow: observability commands must see the same
  refreshed values at the moment they produce output.

## Proof obligations

Isomorphism requirements for the implementation pass:

- Ordering preserved: ordinary command execution order unchanged; INFO/MEMORY
  field ordering unchanged because the same command handlers emit the same
  strings/arrays.
- Tie-breaking unchanged: no data-structure ordering changes; no blocked-client
  or key iteration order changes.
- Floating-point: unchanged except existing INFO/MEMORY formatting paths; prove
  byte-identical golden outputs for those paths.
- RNG seeds: N/A.
- Expiration/TTL: unchanged; `drop_if_expired` and active-expire paths are not
  modified.
- Replication/AOF: unchanged; only observation-context refresh timing changes.
- Client tracking: INFO `tracking_clients`, `tracking_total_keys`,
  `tracking_total_items`, and `tracking_total_prefixes` must be refreshed
  immediately before INFO emission.
- Memory stats: INFO memory/persistence/stats and `MEMORY STATS` must still see
  live client memory aggregates, maxmemory, persistence flags, and replication
  backlog counters.

Golden proof plan:

1. Build a clean HEAD baseline and candidate via crate-scoped RCH:
   `rch exec -- env CARGO_TARGET_DIR=/tmp/... cargo build --profile release-perf -p fr-server`.
2. Capture byte-for-byte RESP transcripts for a deterministic script containing:
   `SET`, `GETSET`, `INFO clients`, `INFO stats`, `INFO memory`,
   `INFO persistence`, `MEMORY STATS`, `CLIENT TRACKING ON BCAST`, a write that
   creates a tracking invalidation, and another `INFO stats`.
3. Hash raw transcript bytes with `sha256sum` for clean HEAD and candidate.
4. Run focused Rust tests that pin `refresh_store_runtime_info_context` being
   invoked before INFO/MEMORY but not for a plain write command.
5. Run crate-scoped proof commands only:
   `rch exec -- cargo test -p fr-runtime <focused-test>`,
   `rch exec -- cargo test -p fr-command info memory`,
   and `rch exec -- cargo test -p fr-server <focused-output-test>` if the TCP
   harness is extended.

Benchmark plan:

1. Reuse the Pass 2 `getset-hit` workload as the primary shared-runtime gate.
2. Add one read-heavy workload (`append` or `set`) only as a secondary sanity
   check if it uses the same clean/candidate worker and binary paths.
3. Keep the lever only if same-worker paired hyperfine shows a real win and the
   Score remains at least `2.0`.

## Rejected for this pass

Output flushing is not the next safe implementation target without lower-level
I/O evidence. `try_flush` is a real shared hotspot, but the profile shows kernel
send cost, not a Rust buffer-copy hotspot. A correct deeper output primitive
would need a separate syscall/latency profile, likely around batching policy or
network event scheduling, before it can clear the gate.

Command classification/metadata fusion is plausible but should be a later,
larger primitive. It touches runtime/fr-command contracts and must prove exact
ACL, arity, stats, commandstats, and propagation metadata behavior. Its current
Score is below the keep gate until Pass 4 profiles whether the repeated
classification rows can be collapsed as one structural change.
