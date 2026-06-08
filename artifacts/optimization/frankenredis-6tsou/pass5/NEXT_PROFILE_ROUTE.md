# frankenredis-6tsou Pass 5 - Next Profile Route

## Closeout

`frankenredis-6tsou` produced one kept lever and three rejected/no-edit routes:

- Keep: GETDEL exact memory-cache removal update in `fr-store`.
- Reject: SETNX borrowed fast path, below the keep gate.
- Reject: GETSET borrowed fast path before source edit, because the profile was
  dominated by shared runtime/output rows.
- Reject: conditional runtime-info refresh, byte-identical but no performance
  win.

## Fresh Shifted Profile Evidence

The GETDEL post-change profile moved the original memory-estimation row down:

- Baseline `fr_store::estimate_entry_memory_usage_bytes`: `3.31%` self.
- Candidate `fr_store::estimate_entry_memory_usage_bytes`: `1.74%` self.
- Candidate profile run: `150834.0 ops/sec`, p50/p95/p99
  `4142us / 11501us / 15951us`.

The broader clean-HEAD GETSET profile still shows the next shared rows:

- `Runtime::execute_frame_internal`: `2.99%` children / `1.35%` self.
- `process_buffered_frames`: `1.32%` children / `0.63%` self.
- `parse_command_args_borrowed_into`: `1.10%` children / `0.79%` self.
- `fr_command::classify_command`: `1.06%` children / `0.36%` self.
- `fr_command::command_table_index`: `1.05%` children / `0.41%` self.

## New Bead

Created: `frankenredis-6tsou.1`

Title: `[perf] Command metadata packet and RESP framing fusion after 6tsou`

The next pass must re-baseline before editing. The target is a deeper shared
parser/dispatch/framing primitive, not another one-command write fast path:

- Preferred route if fresh profile agrees: build a command metadata packet from
  the already-parsed borrowed command token and thread it through runtime and
  fr-command so classification, command-table lookup, canonical fullname, ACL,
  and effect metadata are computed once.
- Alternate route if fresh profile selects protocol/output: zero-copy RESP
  framing or inline small replies with byte-identical transcript proof.

## Proof Obligations For Child Bead

- Ordering, subcommand dispatch, commandstats names, ACL categories, arity
  errors, propagation metadata, Pub/Sub gates, floating-point formatting, and
  RNG must stay byte-identical.
- Golden raw RESP transcript SHA-256 against baseline is mandatory.
- Same-worker hyperfine/criterion keep gate remains Score `>=2.0`.
