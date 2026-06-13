# frankenredis-par0e pass165 proof

Verdict: rejected, no production source kept.

## Target

Current `origin/main` `c975de28d` was rebuilt with:

```sh
rch exec -- env CARGO_TARGET_DIR=/data/tmp/frankenredis-coralox-pass165-main-target cargo build --release -p fr-server
```

Current-main dashboard, P16/C50/n300k, best-of-5:

```text
lpush redis=1030927 fr=993377 redis/fr=1.04x
rpush redis=1030927 fr=986842 redis/fr=1.04x
sadd  redis=925925  fr=903614 redis/fr=1.02x
spop  redis=1145038 fr=1107011 redis/fr=1.03x
```

LPUSH symbol profile was available (`perf_event_paranoid=1`). Main rows:

```text
execute_plain_keyed_values_write_borrowed 6.01% total / 2.10% self
TcpStream::write                         4.56%
UnixStream::write                        4.42%
__send                                   4.29%
parse_command_args_borrowed_into         3.91% total / 2.55% self
plain_borrowed_default_key_write_allows  3.05% total / 2.36% self
time reads                               ~2.2-2.4%
__memmove_avx_unaligned_erms             2.02%
process_buffered_frames                  1.95% total / 1.05% self
ListValue::push_front                    1.78% total / 1.13% self
Store::lpush                             1.68% total / 1.59% self
```

## Candidate

Moved the existing `SADD | LPUSH | RPUSH` borrowed keyed-values matcher in
`parse_borrowed_multibulk_action` ahead of unrelated INCR/DECR/APPEND probes.
This was a matcher-order-only command-packet routing trial: same parser, same
runtime handler, same store mutation, same reply encoding, same fallback paths.

## Behavior proof

Raw TCP golden covered `DEL`, multi-value `LPUSH`, `LRANGE` ordering, `RPUSH`,
`SADD` duplicate handling, `SCARD`, `CLIENT REPLY SKIP/ON`, another `LPUSH`,
and `QUIT`.

```text
input sha256     = b616e89b3d03d365c042ec2383a73c987a7dd43d54ce88c091a82d8ec7645068 (401 bytes)
baseline sha256  = c4a2d062abdffc07a2dc90066d20a53a049cffc259e708d0556a095fac0c5ade (122 bytes)
candidate sha256 = c4a2d062abdffc07a2dc90066d20a53a049cffc259e708d0556a095fac0c5ade (122 bytes)
byte_equal       = true
```

Ordering/tie-breaking/floating-point/RNG: matcher order only decides which
mutually exclusive existing fast-path predicate is tried first. The successful
LPUSH/RPUSH/SADD path, value order, list/set mutations, integer reply bytes,
reply suppression, and fallback semantics are unchanged. No floating-point or
RNG paths are involved.

## Benchmarks

Current-main baseline, LPUSH P16/C50/n1M:

```text
1.014 s +/- 0.036
```

Candidate standalone, same workload:

```text
1.030 s +/- 0.042
```

Paired same-window hyperfine:

```text
current   996.9 ms +/- 22.0 ms
candidate   1.003 s +/- 0.034 s
current 1.01x +/- 0.04 faster than candidate
```

Score: impact `0.0` x confidence `4.0` / effort `0.5` = `0.0`.

## Gates

- Baseline and candidate RCH `cargo build --release -p fr-server` passed.
- `cargo fmt -p fr-server -- --check` passed while the candidate was applied.
- Both builds had only pre-existing warnings in `fr-command` and `fr-runtime`.

## Next route

Do not repeat borrowed matcher ordering/router micro-tuning, exact LPUSH
parsers, direct reply encoding, write-buffer spare capacity, or list-front
storage/capacity variants. Pivot to the next ready profile-backed perf bead,
`frankenredis-x4tgs`, or require a fresh non-list profile before returning to
`fr-server`.
