# frankenredis-iror0 — EVAL interpreter loop gap (5-9x); scope-reuse micro-lever rejected (1.2x)

## Gap (real, vs redis)
release-perf, redis-benchmark -c1, quiet window:
  EVAL "return 1":                          fr 16563 vs redis 21164 = 1.28x  (env-rebuild gap closed by fmilu)
  EVAL "local x=0 for i=1,1000 do x=x+i end": fr 1971 vs redis 17391 = 8.8x
  EVAL table build+sum (200):               fr 3234 vs redis 16227 = 5.0x
fr's tree-walking interpreter resolves EVERY variable access by hashing the name
STRING and walking Vec<Scope{ locals: HashMap<String, Rc<RefCell<LuaValue>>> }>
(lua_eval.rs get_local/set_local ~2910). `x=x+i` does ~4 HashMap string lookups
per iteration; each set_local also allocs a fresh Rc cell + name.to_string() +
lua_gc_register_cell. Redis runs Lua 5.1 BYTECODE with register slots.

## Micro-lever tried & REJECTED (1.2x, sub-gate)
Reuse the numeric-for body scope's HashMap backing across iterations (`clear`
instead of pop+push every turn). Byte-exact: 1141 fr-command tests pass, and the
loop-variable closure-capture stays distinct (`for i=1,3 do t[i]=function()return
i end end` -> {1,2,3} on both fr and redis, because a captured cell survives the
clear via its closure's Rc). But best-of-6: loop1000 1.19x, tableops 1.12x —
the per-ACCESS HashMap string lookup, NOT the scope allocation, dominates. Patch
retained here (loop-scope-reuse.patch). No production code kept.

## Real lever (the big swing)
Add a resolution pass mapping each Expr::Name to a (scope_depth, slot_index) at
parse time; change Scope to Vec<cell> indexed by slot. Runtime variable access
becomes an O(1) array index — no string hashing, no per-access HashMap. Plus move
lua_gc_register_cell from every set_local to closure-capture time (only captured
cells can form Rc cycles). Target 5-9x on EVAL loops, byte-exact. Multi-session
interpreter refactor.

## Update — second micro-lever also rejected (decisive)
Tried a SECOND allocation lever: reuse the loop-variable CELL in place across
iterations (one push_scope; each turn updates `*cell.borrow_mut()` instead of a
fresh `Rc::new` + `name.to_string()` + `lua_gc_register_cell` + HashMap insert).
Verified it is NOT closure-safe (it makes `for i=1,3 do t[i]=function()return i
end end` return {3,3,3} instead of {1,2,3}) — so the real fix would gate it on a
"body has no closure" check. Measured upper bound (always-reuse, best-of-6):
  loop1000:  1941 -> 2403 req/s = 1.24x
  tableops:  3264 -> 3776 req/s = 1.16x

CONCLUSION (two independent experiments): eliminating BOTH the per-iteration
scope HashMap alloc (1.19x) AND the per-iteration cell alloc (1.24x) gives only
~1.2x. Neither allocation is the dominant cost. The per-ACCESS variable lookup
(string-hash + Vec<Scope> walk in get_local/set_existing_local, ~4 per loop turn)
is what dominates. STOP trying allocation/scope micro-levers — go straight to the
slot-resolution refactor (parse-time Expr::Name -> (depth,slot); Scope ->
Vec<cell>; O(1) array index, no string hashing). That is the only lever that
reaches the 5-9x. Also confirmed this session: the rest of the command surface
(big-arg MSET/HSET/MGET/SADD/DEL batches, geo/bit/string/sampling/aggregate-store,
streams) is at parity-or-faster vs redis — the Lua interpreter is the sole gap.
