# `frankenredis-ohsk5` TTL dispatch-floor proof

Date: 2026-07-10

Decision: **KEEP** the exact `TTL key` dispatch-floor route.

## Mechanism selection

Both negative-evidence ledgers were searched before the attempt. Small-reply `writev`,
SORT decomposition, direct geohash encoding, short-key comparison, and cascade reordering
were already rejected or closed. `frankenredis-uhthd` is owner-blocked, and the
GETSET/GETDEL plus persistence lanes are cc-owned.

The complete pre-change self-frame tables at or above `0.1%` are:

- `profile/fr_ranked_self_frames.txt`
- `profile/redis_ranked_self_frames.txt`

For the persistent-key P16 `TTL k` row, control averaged `5.4633B` instructions and
Redis `3.1798B`, a `2.2835B` gap. Control's top self frames were
`process_buffered_frames` (`27.96%`, about `1.5275B` instructions) and
`__memcmp_avx2_movbe` (`9.05%`, about `0.4944B`). Those two estimates account for
`88.5%` of the total gap, selecting dispatch/search as the mechanism.

The single source lever adds exact `TTL key` to the command-token dispatch floor and
reuses the existing TTL parser and executor. Generic fallback remains authoritative for
wrong arity, parser-limit failures, malformed packets, and gated contexts.

## Measurement protocol

- Host-local live TCP benchmark.
- FrankenRedis/Redis server pinned to CPU 25; clients pinned to CPUs 26,27.
- `redis-benchmark -c50 -P16 -n1000000 TTL k` after identical setup and warmup.
- Five interleaved control/candidate/Redis trials.
- `perf stat -e instructions:u -p <server-pid>` is the decision metric.
- Every engine reported `connected_slaves:0`.
- Sample CV must be below `5%` for the scored metric.

Binary SHA-256 values:

| engine/build | sha256 |
| --- | --- |
| control | `e0dd924954b212c4f2bf62c452aad71e5fd6ad89942b585d1143325102cf8c24` |
| A/B candidate | `b7a9a1602b5b8295aefa34fd1746c2f85cadb6cff98376edf175fba31a460cbd` |
| candidate rebuild used for post-profile | `84090b5959b2396569f74343dee5542174afc881c1a97b40856958ae52147679` |
| vendored Redis 7.2.4 | `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7` |

The post-profile binary is a distinct same-source rebuild from a different target/path
context, not the byte-identical A/B binary. The A/B decision uses `b7a9...` only.

## A/B result

| engine | mean `instructions:u` | sample CV | mean req/s | rps sample CV |
| --- | ---: | ---: | ---: | ---: |
| control | 5,462,818,449.0 | 0.0235% | 866,738.42 | 10.529% |
| candidate | 2,075,859,219.8 | 0.1781% | 1,101,380.09 | 4.894% |
| Redis 7.2.4 | 3,199,552,215.2 | 4.1941% | 1,091,980.00 | 6.437% |

- Candidate/control instructions: `0.379998x` (`62.0002%` fewer, `2.6316x` fewer).
- Candidate/Redis instructions: `0.648797x`.
- Control/Redis instructions: `1.707370x`.
- Throughput is descriptive only because control and Redis exceed the `5%` CV gate.

Classifier guard costs remain inside the `1%` instruction ratchet:

| guard | control (sample CV) | candidate (sample CV) | delta |
| --- | ---: | ---: | ---: |
| `GET k` | 1,679,213,295.3 (`0.0492%`) | 1,689,886,918.0 (`0.1336%`) | `+0.6356%` |
| `SET k v` | 2,586,030,466.7 (`0.0408%`) | 2,594,899,007.0 (`0.1685%`) | `+0.3429%` |

## Mechanism confirmation and parity

The post-change profile lost zero samples and moved the intended frames:

| frame | pre self | post self |
| --- | ---: | ---: |
| `process_buffered_frames` | 27.96% | 5.67% |
| `__memcmp_avx2_movbe` | 9.05% | 2.53% |
| `try_dispatch_floor_classified_action` | below top rank | 7.66% |
| `execute_plain_keymeta_borrowed` | 3.16% | 5.91% |

The exact raw pipeline covers missing TTL, persistent TTL, mixed-case `tTl`, wrong arity,
deletion, and reply ordering. Control, candidate, and Redis reply bytes all hash to
`8c6680069e7f748b992d4b988d6cb5e49c13307a6f97344afa939f70e282ddd9`.

## Verification

- `cargo fmt --check`: pass.
- `cargo check --workspace --all-targets`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- Focused dispatch-floor classifier tests: 2 passed.
- `cargo test -p fr-conformance -- --nocapture`: pass (194 library tests, all auxiliary
  binaries, 99 smoke tests, doctests).
- UBS ran on `crates/fr-server/src/main.rs`: its existing whole-file inventory is nonzero;
  no finding intersects an added hunk line. Raw output: `ubs_fr_server.txt`.

RCH was invoked for each heavy build/test. Because the clean proof worktree is under
`/data/tmp`, RCH refused normalization outside canonical `/data/projects` and failed open
to local execution. This is a routing limitation, not a test or performance result.

Preserved harness setup failures are not scored evidence: an RCH-managed candidate target
was cleaned before its first launch; the first two post-profile records failed perf mmap
allocation until `-m 1` was used; and the first parity launch used the project working
directory and observed its existing `dump.rdb`. The valid parity rerun uses isolated
`runtime_*` directories and `*_retry` outputs. No failed setup row entered the A/B means.

The remaining workspace sweep found two unrelated baseline assertions:

- `frankenredis-tr2gd`: ACL script-denial error wording.
- `frankenredis-n4zi2`: stale nine-pair MSET generic-fallback expectation.

No source outside `crates/fr-server/src/main.rs` belongs to this lever.

## Retry boundary

Do not retry TTL executor/store micro-levers from this result. Any remaining TTL work must
start from a fresh ranked profile. `PTTL`, `EXPIRETIME`, and `PEXPIRETIME` need their own
exact-shape measurements before joining the floor.
