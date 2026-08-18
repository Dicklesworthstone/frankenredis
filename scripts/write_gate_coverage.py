#!/usr/bin/env python3
"""Which of the write-gate derivers are actually CONVERTIBLE?

`740196d77` retracted my claim that the write-gate vein was closed, on a raw source count of
"80 executors still deriving". THAT NUMBER WAS ALSO WRONG, in the other direction: a CONVERTED
route keeps the gate call as a fallback for callers passing None, so grepping for the call counts
converted routes as open. Excluding routes that take the parameter:

    gate calls in routes that do NOT take the parameter   64
    routes taking it (converted; call remains as fallback) 51

64 is the population. But it is still not a work estimate, because a route can only be converted
if some CALLER holds a per-pass cache to hand it. Both recent levers on this pattern turned on
exactly that: for UNWATCH (`9069c9eb0`) and DEL/UNLINK (`bc05733bf`) the cache already existed
and only the last hop was missing, and for the deep `parse_borrowed_multibulk_action` fallback
there is no cache at all and `None` is correct.

So this bounds the convertible subset, from source only, with no build:

  CONVERTIBLE   an fr-server caller's enclosing function holds a write-gate cache
  VIA-WRAPPER   no fr-server caller, but an in-runtime caller takes the parameter or is itself
                reachable, so the cache can arrive one hop further in
  FLOOR-HELPER  every fr-server caller is a `dispatch_floor_*` helper that does not hold a cache
                but is itself called from `try_dispatch_floor_classified_action`, which does --
                one helper signature away, the `dispatch_floor_fast_del` fix from `bc05733bf`.
                Keyed on the caller's ROLE (its name), because graph reachability is vacuous
                here: almost every fr-server fn is reachable from a cache holder, including the
                generic fallback, so "could receive one" classifies everything as convertible
  FALLBACK-PATH the only fr-server callers are the generic fallback (parse_borrowed_multibulk_
                action) or the sharded worker, where passing None may well be correct
  UNREACHABLE   no fr-server caller and no in-runtime caller that could forward a value

The read-gate sibling (`read_gate_coverage.py`) does more than this — three derivation forms,
shape coverage, unclassified detection — and is left alone rather than refactored, because it is
a working tool other agents run and the write gate has only one derivation form to find.
"""
import argparse
import re
from pathlib import Path

GATE_CALL = "plain_borrowed_default_key_write_allows(now_ms)"
SUPPLIED_PARAM = "default_write_allowed: Option<bool>"
# The names a caller can hold a per-pass cached write gate under.
CACHE_NAMES = ("plain_write_gate_cache", "write_gate_cache", "cached_plain_write_gate")


def fn_spans(text, indented=True):
    """(start, end, name) for each fn definition, brace matched."""
    pat = r"\n(?:    )?(?:pub )?fn (\w+)\(" if indented else r"\nfn (\w+)\("
    out = []
    for m in re.finditer(pat, text):
        bs = text.find("{", m.end())
        if bs < 0:
            continue
        d, k = 0, bs
        while k < len(text):
            if text[k] == "{":
                d += 1
            elif text[k] == "}":
                d -= 1
                if d == 0:
                    break
            k += 1
        out.append((m.start(), k, m.group(1)))
    return out


def enclosing(spans, pos):
    """Innermost fn containing pos."""
    best = None
    for a, b, n in spans:
        if a <= pos <= b and (best is None or (b - a) < best[0]):
            best = (b - a, n)
    return best[1] if best else None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runtime", default="crates/fr-runtime/src/lib.rs")
    ap.add_argument("--server", default="crates/fr-server/src/main.rs")
    ap.add_argument("--list", action="store_true", help="list every route, not just the counts")
    args = ap.parse_args()

    rt = Path(args.runtime).read_text(errors="replace")
    sv = Path(args.server).read_text(errors="replace")
    rt_spans = fn_spans(rt)
    sv_spans = fn_spans(sv, indented=False)

    # A CONVERTED route keeps the gate call as a fallback for callers that pass None:
    #     default_write_allowed.unwrap_or_else(|| self.plain_borrowed_...(now_ms))
    # so "contains the call" counts converted routes as deriving. That false positive is how the
    # first run of this script reported `execute_plain_del_borrowed` and
    # `can_execute_plain_lrem_borrowed` as open when I had converted both myself. A route is only
    # DERIVING if it does not take the parameter.
    takers = set()
    for a, b, n in rt_spans:
        sig_end = rt.find(")", rt.find("(", a))
        if SUPPLIED_PARAM.split(":")[0] in rt[a:rt.find("{", a)]:
            takers.add(n)

    derivers = {}
    for m in re.finditer(re.escape(GATE_CALL), rt):
        owner = enclosing(rt_spans, m.start())
        if not owner or owner == "plain_borrowed_default_key_write_gate":
            continue
        if owner in takers:
            continue
        derivers.setdefault(owner, 0)
        derivers[owner] += 1

    supplied = len(re.findall(re.escape(SUPPLIED_PARAM), rt))

    # which fr-server functions hold a cache -- and which can RECEIVE one.
    #
    # Holding is not the whole story. `dispatch_floor_fast_del` did not hold a cache until
    # `bc05733bf` threaded one in from `try_dispatch_floor_classified_action`, which did. Three
    # more floor helpers -- xack_missing, xdel_missing, xtrim_minid_noop -- are in exactly that
    # position now, and classifying them as blocked would have hidden three levers behind a
    # word. So cache-reachability propagates through fr-server callers the same way it does
    # through fr-runtime ones.
    cache_fns = set()
    for a, b, n in sv_spans:
        if any(c in sv[a:b] for c in CACHE_NAMES):
            cache_fns.add(n)

    sv_names = {n for _, _, n in sv_spans}
    sv_callers = {}
    for n in sv_names:
        callers = set()
        for m in re.finditer(r"(?<![\w])" + re.escape(n) + r"\(", sv):
            owner = enclosing(sv_spans, m.start())
            if owner and owner != n:
                callers.add(owner)
        sv_callers[n] = callers

    cache_reachable_sv = set(cache_fns)
    changed = True
    while changed:
        changed = False
        for n in sv_names:
            if n not in cache_reachable_sv and (sv_callers[n] & cache_reachable_sv):
                cache_reachable_sv.add(n)
                changed = True

    # ---- cache reachability over EVERY runtime fn, not just the derivers.
    #
    # The first version of this propagation only followed callers that were themselves derivers,
    # and so reported VIA-WRAPPER 0 / UNREACHABLE 21. That was wrong: a wrapper can neither
    # derive nor take the parameter and still be called from a cache-holding fn.
    # `can_execute_plain_append_borrowed` is the case that exposed it -- its only in-runtime
    # caller is `execute_plain_append_borrowed`, which IS called from `process_buffered_frames`
    # and `try_dispatch_floor_classified_action`. Reachability is a property of the CALL GRAPH,
    # so it has to be computed over all of it.
    rt_names = {n for _, _, n in rt_spans}
    server_cache_reached = set()
    for _, _, n in rt_spans:
        for m in re.finditer(r"(?<![\w])" + re.escape(n) + r"\(", sv):
            if enclosing(sv_spans, m.start()) in cache_fns:
                server_cache_reached.add(n)
                break

    rt_callers = {}
    for n in rt_names:
        callers = set()
        for m in re.finditer(r"(?<![\w])" + re.escape(n) + r"\(", rt):
            owner = enclosing(rt_spans, m.start())
            if owner and owner != n:
                callers.add(owner)
        rt_callers[n] = callers

    reachable = set(server_cache_reached)
    changed = True
    while changed:
        changed = False
        for n in rt_names:
            if n in reachable:
                continue
            if rt_callers[n] & reachable:
                reachable.add(n)
                changed = True

    verdicts = {}
    for fn in derivers:
        sv_callers = {enclosing(sv_spans, m.start())
                      for m in re.finditer(r"(?<![\w])" + re.escape(fn) + r"\(", sv)}
        sv_callers.discard(None)
        if sv_callers & cache_fns:
            verdicts[fn] = ("CONVERTIBLE", sv_callers & cache_fns)
        elif fn in reachable:
            verdicts[fn] = ("VIA-WRAPPER", rt_callers[fn] & reachable)
        elif sv_callers and all(c.startswith("dispatch_floor_") for c in sv_callers):
            # Caller is a floor helper that does not hold a cache but IS called from one that
            # does -- precisely where `dispatch_floor_fast_del` sat before `bc05733bf` threaded
            # it. One helper signature, then the supply. Not blocked.
            verdicts[fn] = ("FLOOR-HELPER", sv_callers)
        elif sv_callers:
            verdicts[fn] = ("FALLBACK-PATH", sv_callers)
        else:
            verdicts[fn] = ("UNREACHABLE", rt_callers[fn])

    counts = {}
    for v, _ in verdicts.values():
        counts[v] = counts.get(v, 0) + 1

    print(f"gate calls in NON-taking routes : {sum(derivers.values())}")
    print(f"routes taking the param (converted, call remains as fallback): {len(takers)}")
    print(f"distinct executors deriving : {len(derivers)}")
    print(f"routes already supplied     : {supplied}")
    print(f"fr-server fns HOLDING a cache   : {len(cache_fns)}  {sorted(cache_fns)}")
    print(f"fr-server fns that can RECEIVE one: {len(cache_reachable_sv)}")
    print()
    for v in ("CONVERTIBLE", "VIA-WRAPPER", "FLOOR-HELPER", "FALLBACK-PATH", "UNREACHABLE"):
        print(f"  {v:16s} {counts.get(v, 0):>4}")
    if args.list:
        print()
        for fn in sorted(verdicts, key=lambda f: (verdicts[f][0], f)):
            v, who = verdicts[fn]
            print(f"  {v:16s} {fn[:58]:58s} {sorted(who)[:2]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
