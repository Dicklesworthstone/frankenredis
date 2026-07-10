# LPOS P16 `instructions:u` attribution and dispatch-floor A/B

## Baseline versus vendored Redis 7.2.4

- Workload: seeded one-element list, then `LPOS l a`, `-c 50 -P 16 -n 1000000`.
- Profile workload: the same P16/C50 command stream, `perf record -F 997 -e instructions:u -g`.
- Quiescence and affinity: three seconds after seeding; server CPU 25; client CPUs 26,27.
- Host: `thinkstation1`, Linux `6.17.0-35-generic`, AMD Ryzen Threadripper PRO 5975WX.
- FrankenRedis symbolized release-perf snapshot SHA-256:
  `84090b5959b2396569f74343dee5542174afc881c1a97b40856958ae52147679`.
- Vendored Redis 7.2.4 SHA-256:
  `e837dbb2556cff6b777245f944c5f5601c144859ad9ea926d89c6596b6e32ec7`.
- `redis-benchmark` SHA-256:
  `8931ebb4de7ea5ae700bf4cf866ad246535743496318ad4e19b46af1c58d7a0b`.

The symbolized FrankenRedis snapshot contains the landed TTL floor but predates the OBJECT
IDLETIME floor and `fr-simd` AVX2 keeps. Those later changes do not alter LPOS parsing, dispatch,
list lookup, or reply encoding. This baseline is mechanism-attribution evidence, not candidate
keep evidence. The exact-current ORIG profile in the same-binary A/B independently confirms
`process_buffered_frames` at 24.30% self.

| engine | five `instructions:u` counts | mean | sample CV |
|---|---|---:|---:|
| FrankenRedis | 5,316,539,823; 5,314,656,685; 5,313,897,167; 5,315,713,017; 5,314,884,663 | **5,315,138,271.0** | **0.019120%** |
| Redis 7.2.4 | 4,180,630,604; 4,184,553,594; 4,178,364,904; 4,181,174,784; 4,184,044,726 | **4,181,753,722.4** | **0.061165%** |

FrankenRedis/Redis is **1.271030918x**. The mean gap is **1,133,384,548.6 instructions**, or
approximately 1,133.38 instructions per command.

The complete authoritative `>=0.1%` no-children tables are:

- `fr_lpos_ranked_self_frames.txt`: **108** ranked symbols, approximately
  10,627,211,683 sampled events, zero lost samples.
- `redis_lpos_ranked_self_frames.txt`: **129** ranked symbols, approximately
  8,370,692,410 sampled events, zero lost samples.

Largest FrankenRedis frames:

| rank | frame | self-time |
|---:|---|---:|
| 1 | `frankenredis::process_buffered_frames` | **25.11%** |
| 2 | `__memcmp_avx2_movbe` | **7.80%** |
| 3 | store entry `HashMap::get_mut` | 3.03% |
| 4 | vDSO time | 2.63% |
| 5 | `Runtime::execute_plain_lpos_borrowed` | 2.56% |
| 6 | `parse_borrowed_plain_lpos_packet` | 2.55% |
| 7 | `Runtime::plain_borrowed_default_key_read_allows` | 2.09% |
| 8 | `Store::lpos_full` | 1.86% |
| 9 | `parse_borrowed_plain_mset_packet` | 1.78% |
| 10 | `parse_borrowed_plain_keys_multi_packet` | 1.71% |
| 11 | `try_dispatch_floor_classified_action` | 1.71% |
| 12 | `parse_borrowed_plain_key_arg2_packet` | 1.66% |
| 13 | `parse_borrowed_plain_set_bulk` | 1.57% |
| 14 | `RespFrame::encode_into` | 1.44% |
| 15 | command-histogram `HashMap::get_mut` | 1.22% |
| 16 | `[u8]::hash` | 1.17% |
| 17 | `Runtime::drain_pending_pubsub` | 1.00% |
| 18 | `drain_pending_pubsub_to_connection` | 0.99% |

Largest Redis frames:

| rank | frame | self-time |
|---:|---|---:|
| 1 | `je_malloc_usable_size` | 11.11% |
| 2 | vDSO time | 8.78% |
| 3 | `__strcasecmp_l_avx2` | 6.58% |
| 4 | `call` | 5.54% |
| 5 | `__strchr_avx2` | 4.63% |
| 6 | `processMultibulkBuffer` | **3.77%** |
| 7 | `je_free` | 3.37% |
| 8 | `zmalloc` | 3.12% |
| 9 | `je_malloc` | 2.56% |
| 10 | `zfree` | 1.98% |
| 11 | `resetClient` | 1.70% |
| 12 | `decrRefCount.part.0` | 1.69% |
| 13 | `createEmbeddedStringObject` | 1.69% |
| 14 | `siphash_nocase` | 1.34% |
| 15 | `dictFind` | 1.23% |
| 16 | `listTypeEqual` | 1.23% |
| 17 | `lposCommand` | 1.11% |
| 18 | `processCommand` | 1.05% |

Applying the sampled shares to the five-trial means attributes approximately
**1,176,979,104.5 excess instructions**, or **103.85% of the net gap**, to the buffered
dispatch/parser frame alone. FrankenRedis `memcmp` contributes another approximately
**404,962,751.6 excess instructions** relative to Redis. The total can exceed the net gap because
Redis pays larger allocator, vDSO, and command-wrapper costs that partially offset FrankenRedis's
dispatch excess.

The profile-selected mechanism is therefore the open dispatch/parser chain. It is not writev:
replies remain coalesced and no flush frame reaches 0.1%. It is not a SIMD-site build defect:
the second frame is already AVX2 `memcmp`, and the selected frame is Rust dispatch. It is not the
owner-blocked store lane: `Store::lpos_full` is only 1.86% self. The prior generic
uppercase/matcher rejection lacks current binary provenance, exact changed-function self-time,
worker identity, and a per-function null, so it cannot close this structurally narrower floor.

## One lever

Recognize only exact three-argument `LPOS key member` at the existing borrowed dispatch floor,
then reuse the existing `parse_borrowed_plain_lpos_packet` and
`Runtime::execute_plain_lpos_borrowed`. LPOS forms with `RANK`, `COUNT`, or `MAXLEN`, wrong arity,
and malformed packets retain the previous borrowed cascade and fallback behavior.

## Same-binary null control and candidate versus ORIG

- Command: `RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec -- cargo test --profile
  release-perf -j 2 -p fr-server --features perf-ab-lpos-floor --test
  object_idletime_floor_ab -- --ignored --nocapture
  lpos_floor_same_binary_null_then_interleaved_instruction_ab`.
- RCH worker: `hz1`; worker hostname: `hetzner1`; allowed CPUs `0-7`; client CPU 0; server CPU 7.
- One executable for every null/control/candidate arm, SHA-256:
  `e7989e1517c1f9e0205141da76b20e68cd6e25d9237716095eb9073439f1f20d`.
- ORIG is a feature-only monomorph of the exact pre-LPOS-floor command classifier. Production
  builds compile the candidate directly without an environment lookup on this route.
- Every sample runs both arms inside one measured routine, with OCCO/COOC position balancing,
  P16/C50, three-second post-seed quiescence, and 750 ms perf-attach delay. Both packet input and
  complete replies cross `black_box` barriers.
- Profile reachability gate: zero lost samples; exact-current ORIG
  `process_buffered_frames` **24.30% self**; exact candidate helper
  `dispatch_floor_fast_lpos` **1.30% self**.

The mandatory per-function paired base/base null ran first:

| statistic | null base/base ratio |
|---|---:|
| median | **0.999992628** |
| p05 | **0.999967529** |
| p95 | **1.000031352** |
| ratio CV (informational) | 0.002371% |
| left/right CV (informational) | 0.006655% / 0.008195% |

Candidate A/B samples:

| sample | ORIG instructions | candidate instructions | candidate/ORIG |
|---:|---:|---:|---:|
| 1 | 1,361,886,471 | 616,052,578 | 0.452352374 |
| 2 | 1,361,877,628 | 616,011,944 | 0.452325474 |
| 3 | 1,361,915,679 | 616,069,197 | 0.452354875 |
| 4 | 1,361,273,909 | 616,270,507 | 0.452716021 |
| 5 | 1,361,551,100 | 616,207,913 | 0.452577882 |
| 6 | 1,361,318,663 | 616,142,838 | 0.452607354 |
| 7 | 1,361,075,554 | 616,004,621 | 0.452586647 |
| 8 | 1,361,460,259 | 616,133,082 | 0.452553116 |
| 9 | 1,361,770,425 | 616,183,655 | 0.452487177 |
| 10 | 1,361,634,816 | 616,149,435 | 0.452507110 |

Means are ORIG **1,361,576,450.4** and candidate **616,122,577.0** instructions. The decisive
candidate/ORIG median is **0.452530113** (p05 **0.452337579**, p95 **0.452667121**), or
**54.746989% fewer instructions / approximately 2.2098x reduction**. The median is far below the
null p05 and clears the 1% instruction ratchet. Informational CV is **0.021426%** ORIG,
**0.014112%** candidate, and **0.028306%** for the paired ratio.

## Final keep gates

- Exact mixed-case and wrong-arity classifier gate: **1/1 passed** on remote worker `ovh-a`.
- The same classifier gate with both measurement controls enabled: **1/1 passed** on remote worker
  `hz2` after making the controls composable.
- Full `fr-conformance`: **194/194** library tests, every auxiliary/doc target, **99/99** smoke
  tests, the **4,975-case** differential fixture matrix, and **116/116** live OBJECT cases passed
  on remote worker `ovh-a`.
- Workspace all-target check: passed on remote worker `ovh-b`.
- Feature-enabled `fr-server` all-target clippy with `-D warnings`: passed on remote worker `hz1`.
- Combined OBJECT-IDLETIME/LPOS measurement-feature all-target clippy with `-D warnings`: passed on
  remote worker `ovh-b`, proving the two one-binary controls compose.
- Workspace-wide clippy reached only the filed, cc-owned `fr-persist` excessive-precision baseline
  (`frankenredis-u0x5d`); no `fr-server` finding remained.
- UBS ran on the changed Cargo/server/harness files with Cargo-backed categories 12-14 disabled so
  it could not violate the disk constraint. Its nonzero inventory contained existing whole-file
  findings plus intentional fail-closed harness panics, bounded slices, and quantile indexes; it
  found no new production defect in the LPOS lever.
- Direct Rust 2024 rustfmt and source/doc diff checks passed. The two raw `perf report` tables
  intentionally preserve the profiler's tool-emitted column padding.

Verdict: **FINAL KEEP**. The candidate is outside the entire measured null spread, the exact
function is profile-live, behavior parity is green, and the source-scoped quality gates pass.
