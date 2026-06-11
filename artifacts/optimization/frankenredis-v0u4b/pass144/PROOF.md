# Pass 144 Proof: full Lua slot-resolved locals

Bead: `frankenredis-v0u4b`

## Target

Profile-backed target from pass 143: after compact Lua local frames, `EVAL`
loop/table workloads still spent material time in name-based local lookup and
cell cloning.

Fresh current-head `perf record` on `loop1000` confirmed the same target:

| Symbol | Self |
| --- | ---: |
| `__memmove_avx_unaligned_erms` | 13.94% |
| `LuaState::eval_expr` | 10.84% |
| `LuaValue::clone` | 5.31% |
| `LuaState::eval_call_args` | 5.09% |
| `LuaState::exec_stmt` | 4.92% |
| `Env::set_local` | 4.63% |
| `Env::set_existing_local` | 3.56% |

## Lever

One lever kept: parse-time resolution of provable lexical Lua locals to
`LocalSlotRef { depth, slot }`, preserving unresolved `Expr::Name` for globals,
environment-table fallback, and dynamic cases. Runtime local reads, writes,
table write-backs, builtin mutation write-backs, and call/error labels now use
slot-indexed lookup first, with the old name path retained as a semantic
fallback.

Resolver ordering preserves existing behavior:

- `local x = x` resolves the initializer before declaring the new local.
- `local function f() ... f() ... end` predeclares the recursive cell before
  capturing the function environment.
- numeric/generic `for` header expressions resolve before loop-local scope.
- `repeat ... until cond` resolves the condition inside the repeat scope.
- unresolved names stay global/env lookups.

## Baseline And Candidate

Both release-perf server binaries were built through RCH.

| Binary | SHA-256 |
| --- | --- |
| baseline | `2e357702eecc5a0e62a1a763d92e3f7151c7e4e963dac1a7d6b4f5b2576e39d0` |
| candidate | `3952606b2354fc767ca52ff7d80a7221e8543bea73222f4040d1a9022238e230` |

## Performance

Paired and reversed hyperfine runs used the pass 143 Lua harness with the same
iteration count and stable golden transcript.

| Gate | Baseline | Candidate | Speedup | Delta |
| --- | ---: | ---: | ---: | ---: |
| paired `loop1000` | 1.081852151s | 1.009675558s | 1.0715x | 6.67% |
| reversed `loop1000` | 1.073835276s | 1.000157657s | 1.0737x | 6.86% |
| paired `table200` | 0.721912404s | 0.680783219s | 1.0604x | 5.70% |
| reversed `table200` | 0.738190320s | 0.670927905s | 1.1003x | 9.11% |

## Isomorphism Proof

All baseline and candidate golden artifacts are byte-identical:

- golden JSON SHA-256:
  `522a3ab10859dcc45592ec5a323f1bc0f40b81db1662124aacb7a6d3e7bed005`
- embedded transcript SHA-256:
  `6e20e28314978053709dee6ae7958ababe6c7c76b73e7e136152696cabceda08`

The candidate changes only local-name resolution metadata and slot lookup. It
does not change command ordering, Redis reply serialization, floating-point
operations, RNG state, table iteration order, or global/environment fallback.

Regression pin added:

- `lua_local_slot_resolution_preserves_lexical_semantics_v0u4b`

It covers local shadowing, `local x = x`, closure capture, table reads/writes,
numeric loop locals, local recursive functions, and `loadstring` chunk
resolution.

## Gates

- `cargo fmt -p fr-command -- --check`: passed.
- RCH `cargo check -j 1 -p fr-command --all-targets`: passed.
- RCH `cargo clippy -j 1 -p fr-command --all-targets -- -D warnings`: passed
  on `vmi1152480`.
- RCH `cargo test -j 1 -p fr-command --lib lua_ -- --nocapture`: passed 201
  Lua-related tests on `vmi1153651`.
- `ubs crates/fr-command/src/lua_eval.rs`: nonzero due broad historical
  inventories in the large evaluator; its built-in fmt, clippy, cargo check,
  test-build, audit, and deny subchecks were clean.

## Score

Impact `1.0737` x Confidence `0.95` / Effort `0.50` = `2.04`.

Verdict: PRODUCTIVE / KEPT.

Next route: reprofile current main for the shifted Lua scripting hotspot before
selecting another Lua primitive. Do not repeat HashMap/frame-allocation or
slot-resolution micro-levers.
