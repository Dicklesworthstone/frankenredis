# frankenredis-fmilu — EVAL env-rebuild: stdlib-table sharing (SUB-2.0, rejected as standalone perf lever)

## Lever
`LuaState::new` rebuilds the whole Lua env per EVAL. Shared the 7 pure stdlib
tables (math/string/table/cjson/cmsgpack/struct/bit) via a thread-local template
(`build_stdlib_template` + `LuaTable::new_unregistered`), Rc-cloned into each
script's globals instead of rebuilt (~150 fewer allocs/call). Required excluding
the readonly templates from `Drop for LuaState`'s `clear_table_recursive` cycle
sweep (it drains globals-reachable tables; draining a shared template empties it
for all later EVALs on the thread — the bug that surfaced as `bit.bor (a nil value)`
on the 2nd eval).

## Isomorphism (PASS)
Candidate vs baseline-fr, broad Lua stdlib battery (math/string/table/cjson/
cmsgpack/struct/bit + misc), filtered of inherently-nondeterministic lines
(function-pointer addresses, hash-ordered cjson multi-key encode):
byte-identical, sha256 43801ba4bc8d75309ef47155a68cce036ad6bdd70d01e3038c74b9f1ba61eb96.
All 1141 fr-command tests pass (incl. qqq17 cycle-leak test).

## Benchmark (FAIL Score>=2.0)
redis-benchmark -c1, best-of-3, paired baseline vs candidate (box load ~11):
  EVAL "return 1":                12262 -> 13822 req/s  = 1.13x
  EVAL small arithmetic loop:     10246 -> 10953 req/s  = 1.07x
Env rebuild is only ~13% of per-call cost — NOT the dominant factor the bead
hypothesized. The loop case (1.07x) confirms execution dominates for non-trivial
scripts.

## Diagnosis / next lever
The ~1.84x trivial-EVAL gap vs redis is distributed across MANY small per-call
costs: the 25 builtin globals, the per-call `redis` table build, the per-call
`_G` snapshot (install_g_table copies all globals), parse, LuaState struct init,
GC scope, RESP conversion, and store propagation-state reset. There is no single
2x lever here — redis is fast because it keeps a PERSISTENT lua_State reused
across calls. The real lever is a pooled/persistent LuaState (env built once,
store rebound per execution), target ~1.6x to close the full per-call gap.
Source hunk removed; patch retained here (stdlib-template-share.patch) as the
foundation for that refactor. Filed as the deeper lever.
