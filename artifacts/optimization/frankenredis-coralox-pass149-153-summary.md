# FrankenRedis CoralOx Passes 149-153

Head: `ea390360428ffd9c3886d28f6fbb01fac7d9507d`

## Pass 149: current-main P16 residual profile

- Fresh release dashboard, P16/C50, best-of-3:
  - `set`: Redis `714286` qps, fr `641026` qps, Redis/fr `1.11x`
  - `get`: Redis `793651` qps, fr `914634` qps, Redis/fr `0.87x`
  - `incr`: Redis `892857` qps, fr `688073` qps, Redis/fr `1.30x`
  - `lpush`: Redis `967742` qps, fr `986842` qps, Redis/fr `0.98x`
  - `rpush`: Redis `986842` qps, fr `967742` qps, Redis/fr `1.02x`
  - `sadd`: Redis `828729` qps, fr `785340` qps, Redis/fr `1.06x`
  - `hset`: Redis `785340` qps, fr `769231` qps, Redis/fr `1.02x`
  - `zadd`: Redis `366748` qps, fr `424929` qps, Redis/fr `0.86x`
  - `spop`: Redis `986842` qps, fr `909091` qps, Redis/fr `1.09x`
- `perf` and ptrace/strace were blocked by host policy (`perf_event_paranoid=4`, `PTRACE_SEIZE` denied), so routing used command-level profile plus source inspection.

## Pass 150: direct-encoded INCR reply trial

- Bead: `frankenredis-ohsk5.31`
- Candidate: direct encode successful borrowed `INCR` integer replies.
- Result: rejected. RCH baseline `INCR` fr qps `781250`; candidate `INCR` fr qps `781250`. Ratio movement was Redis-side variance.

## Pass 151: borrowed argv scratch reuse trial

- Bead: `frankenredis-ohsk5.32`
- Candidate: reuse `Vec<&[u8]>` across borrowed multibulk parses.
- Result: rejected before benchmark. Safe Rust lifetime model retained immutable `read_buf` borrows across mutable connection handling (`E0502`). Next safe route is range-based parsing or output batching.

## Pass 152: SET direct-OK route check

- Bead: `frankenredis-ohsk5.33`
- Fresh RCH baseline showed `SET` was not a current gap: Redis `816326` qps, fr `956938` qps, Redis/fr `0.853x`.
- Result: no edit; redirected to `INCR`.

## Pass 153: exact borrowed INCR packet parser trial

- Bead: `frankenredis-ohsk5.34`
- Candidate: exact zero-copy RESP packet recognizer for plain `INCR`, analogous to the existing exact `GET`/`SET` packet parsers.
- Result: rejected. RCH best-of-5 baseline `INCR` fr qps `881057`; candidate `INCR` fr qps `847458`.

## Next route

Do not repeat reply-static, direct-integer, or exact-single-command parser micro-levers. The next structurally different safe-Rust primitive should be one of:

- range-based multibulk parser that carries `Range<usize>` metadata instead of borrowed slices, avoiding per-command argv heap allocation without retaining `read_buf` borrows;
- output/write batching primitive that targets syscall/coalescing cost if a fresh profile or worker-side trace can expose it;
- larger command packet layout that batches parse metadata for an entire read buffer while preserving serial command execution and reply order.
