# OBJECT IDLETIME P16 `instructions:u` attribution and floor A/B

## Baseline versus vendored Redis 7.2.4

- Workload: persistent-key `OBJECT IDLETIME k`, `-c 50 -P 16 -n 1000000`.
- Profile workload: 2,000,000 operations, `perf record -F 997 -e instructions:u -g -m 4`.
- Affinity: server CPU 25; client CPUs 26,27.
- Host: `thinkstation1`, Linux `6.17.0-35-generic`, AMD Ryzen Threadripper PRO 5975WX.
- FrankenRedis release-perf snapshot SHA-256:
  `84090b5959b2396569f74343dee5542174afc881c1a97b40856958ae52147679`.
- Vendored Redis 7.2.4 SHA-256:
  `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`.

The FrankenRedis snapshot is the symbolized post-TTL-floor binary. No intervening `fr-server` or
`fr-runtime` executable change touched this workload; later code changes were confined to SORT and
zset-DUMP paths. It is mechanism-attribution evidence, not the candidate/control keep proof.

| engine | five `instructions:u` counts | mean | sample CV |
|---|---|---:|---:|
| FrankenRedis | 6,174,019,036; 6,172,292,463; 6,172,272,170; 6,169,344,034; 6,172,263,278 | **6,172,038,196.2** | **0.027295%** |
| Redis 7.2.4 | 4,130,895,491; 4,151,133,900; 4,175,050,525; 4,183,723,050; 4,172,124,436 | **4,162,585,480.4** | **0.513643%** |

FrankenRedis/Redis is **1.482741490x**. The mean gap is **2,009,452,715.8 instructions**, or
approximately 2,009.45 instructions per command.

The complete authoritative `>=0.1%` no-children tables are:

- `fr_object_idletime_ranked_self_frames.txt`: **113** ranked symbols, approximately
  12,340,492,072 sampled events, zero lost samples.
- `redis_object_idletime_ranked_self_frames.txt`: **121** ranked symbols, approximately
  8,307,813,351 sampled events, zero lost samples.

Largest FrankenRedis frames:

| rank | frame | self-time |
|---:|---|---:|
| 1 | `frankenredis::process_buffered_frames` | **27.68%** |
| 2 | `__memcmp_avx2_movbe` | **7.17%** |
| 3 | `HashMap<Box<[u8]>, Entry>::contains_key` | 3.89% |
| 4 | `Runtime::execute_plain_object_stat_borrowed` | 2.88% |
| 5 | vDSO time | 2.79% |
| 6 | `parse_borrowed_plain_keys_multi_packet` | 2.25% |
| 7 | `parse_borrowed_plain_object_stat_packet` | 2.24% |
| 8 | `parse_borrowed_plain_key_arg2_packet` | 1.93% |
| 9 | `try_dispatch_floor_classified_action` | 1.86% |
| 10 | `parse_borrowed_plain_mset_packet` | 1.40% |
| 11 | `Runtime::plain_borrowed_default_key_read_allows` | 1.36% |
| 12 | `parse_borrowed_plain_hmset_packet` | 1.36% |
| 13 | `parse_borrowed_plain_hset_multi_packet` | 1.31% |
| 14 | `[u8]::hash` | 1.18% |
| 15 | `parse_borrowed_plain_object_encoding_packet` | 1.18% |
| 16 | command-histogram `HashMap::get_mut` | 1.14% |
| 17 | `__memmove_avx_unaligned_erms` | 1.14% |
| 18 | `parse_borrowed_plain_keyed_values1_packet` | 1.00% |

Largest Redis frames:

| rank | frame | self-time |
|---:|---|---:|
| 1 | vDSO time | 9.36% |
| 2 | `je_malloc_usable_size` | 6.91% |
| 3 | `__strcasecmp_l_avx2` | 6.21% |
| 4 | `processMultibulkBuffer` | **5.71%** |
| 5 | `__strchr_avx2` | 4.31% |
| 6 | `call` | 4.26% |
| 7 | `je_free` | 3.35% |
| 8 | `processCommand` | 3.19% |
| 9 | `dictFind` | 2.96% |
| 10 | `je_malloc` | 2.75% |
| 11 | `zmalloc` | 2.44% |
| 12 | `siphash_nocase` | 2.03% |
| 13 | `processInputBuffer` | 1.98% |
| 14 | `zfree` | 1.96% |
| 15 | `addReplyLongLongWithPrefix` | 1.96% |
| 16 | `createEmbeddedStringObject` | 1.44% |
| 17 | `__memmove_avx_unaligned_erms` | 1.43% |
| 18 | `_addReplyToBufferOrList` | 1.28% |
| 19 | `dictSdsKeyCompare` | 1.27% |
| 20 | `decrRefCount.part.0` | 1.10% |
| 21 | `ull2string` | 1.08% |
| 22 | vDSO `clock_gettime` | 1.05% |
| 23 | `resetClient` | 1.03% |

Applying the sampled self shares to the five-trial means attributes **1,470,736,542 excess
instructions (73.19% of the gap)** to buffered dispatch/parser work and **435,042,485 (21.65%)**
to `memcmp`. Combined, those families explain **1,905,779,027 instructions, or 94.84%** of the
gap. `Store::object_idletime` is only 0.17% self in this profile. The valid writev rejection is not
implicated: replies are already coalesced, reply encoding is only 0.87%, and no flush frame reaches
0.1%.

The top frame therefore selects the dispatch/parser chain. The prior blanket uppercase/matcher
rejection has no binary hash, changed-function self-time, worker identity, or CV and is inadmissible
under the current ledger rule. The selected one-lever experiment is an exact `OBJECT IDLETIME`
dispatch-floor route reusing the existing parser and executor.

## Same-binary, same-worker candidate versus ORIG

- Command: `RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec -- cargo test --profile
  release-perf -p fr-server --features perf-ab-object-idletime-floor --test
  object_idletime_floor_ab -- --ignored --nocapture`.
- Worker: `vmi1167313`; allowed CPUs `0-5`; client CPU 0; both server arms CPU 5.
- One executable for both arms, SHA-256:
  `90cf326cbf9e5d08cbc6c8deb59f0ce852aeca1b9808a9519ef9ad17ee4a0845`.
- ORIG is a feature-only monomorph of the exact pre-lever token and command classifiers. Candidate
  adds only the exact `OBJECT` + `IDLETIME` route. The production build has no environment lookup.
- Every sample ran both arms in one routine with OCCO/COOC alternation, P16/C50, 256,000 commands
  per arm, three-second post-seed quiescence, and 750 ms perf-attach delay. Both packet input and
  complete replies cross `black_box` barriers.
- Profile gate: zero lost samples; ORIG `process_buffered_frames` **23.82% self**; candidate exact
  `dispatch_floor_fast_object_idletime` **1.93% self**. The function under test is live.

| sample | order | ORIG instructions | candidate instructions | candidate/ORIG |
|---:|:---:|---:|---:|---:|
| 1 | OCCO | 1,565,175,048 | 673,035,742 | 0.430006690 |
| 2 | COOC | 1,565,169,336 | 673,050,280 | 0.430017548 |
| 3 | OCCO | 1,565,060,523 | 672,953,046 | 0.429985318 |
| 4 | COOC | 1,565,168,023 | 673,037,340 | 0.430009641 |
| 5 | OCCO | 1,565,210,985 | 673,045,139 | 0.430002821 |
| 6 | COOC | 1,565,229,552 | 673,090,852 | 0.430026926 |
| 7 | OCCO | 1,565,190,494 | 673,091,337 | 0.430037966 |
| 8 | COOC | 1,565,202,740 | 673,034,748 | 0.429998447 |

Means are ORIG **1,565,175,837.625** and candidate **673,042,310.500** instructions. Candidate/ORIG
is **0.430010670**, or **56.998933% fewer instructions / 2.325524x reduction**. CV is **0.003281%**
ORIG, **0.006384%** candidate, and **0.003859%** for the paired ratio.

The shared GETBIT guard was ORIG **1,286,327,861.750**, candidate **1,290,697,577.500**,
candidate/ORIG **1.003397047**, with **0.003304% / 0.003974% / 0.003664%** ORIG, candidate, and
paired-ratio CV. It clears the 1% neutrality gate.

## Final keep gates

- Exact mixed-case/sibling/arity classifier test: **1/1 passed** on remote worker `hz1`.
- Full `fr-conformance`: **194/194** library tests, every auxiliary and doc-test target,
  **99/99** smoke cases, **4,975/4,975** differential fixtures, and **116/116** live OBJECT cases
  passed on remote worker `ovh-b`.
- Workspace all-target check: passed on `hz1`.
- Feature-enabled `fr-server` all-target clippy with `-D warnings`: passed on `hz1`.
- Direct rustfmt and source/doc diff checks: passed. The two raw `perf report` frame tables
  intentionally preserve perf's tool-emitted column padding. Workspace-wide clippy stopped only on the filed
  `fr-persist` excessive-precision baseline (`frankenredis-u0x5d`) and concurrently owned
  `fr-store` test constants.
- UBS found no new production defect in this lever. Its scanner unexpectedly invoked a local Cargo
  shadow-worktree check; that output was discarded, and UBS was not rerun under the disk constraint.

Verdict: **FINAL KEEP**. The exact dispatch floor clears the 1% instruction ratchet by 55.999
percentage points while leaving sibling `OBJECT FREQ`, `OBJECT REFCOUNT`, and `OBJECT ENCODING` on
the existing borrowed cascade.
