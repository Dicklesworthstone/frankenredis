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

  CONVERTIBLE     an fr-server caller's enclosing function holds a write-gate cache
  NO-CACHE        every fr-server caller is somewhere with no cache in scope
  NO-SERVER-CALL  reached only from inside fr-runtime, so the question is one level deeper

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

    # which fr-server functions hold a cache
    cache_fns = set()
    for a, b, n in sv_spans:
        body = sv[a:b]
        if any(c in body for c in CACHE_NAMES):
            cache_fns.add(n)

    verdicts = {}
    for fn in derivers:
        callers = set()
        for m in re.finditer(r"(?<![\w])" + re.escape(fn) + r"\(", sv):
            owner = enclosing(sv_spans, m.start())
            if owner:
                callers.add(owner)
        if not callers:
            verdicts[fn] = ("NO-SERVER-CALL", callers)
        elif callers & cache_fns:
            verdicts[fn] = ("CONVERTIBLE", callers & cache_fns)
        else:
            verdicts[fn] = ("NO-CACHE", callers)

    counts = {}
    for v, _ in verdicts.values():
        counts[v] = counts.get(v, 0) + 1

    print(f"gate calls in NON-taking routes : {sum(derivers.values())}")
    print(f"routes taking the param (converted, call remains as fallback): {len(takers)}")
    print(f"distinct executors deriving : {len(derivers)}")
    print(f"routes already supplied     : {supplied}")
    print(f"fr-server fns holding a cache: {len(cache_fns)}  {sorted(cache_fns)}")
    print()
    for v in ("CONVERTIBLE", "NO-CACHE", "NO-SERVER-CALL"):
        print(f"  {v:16s} {counts.get(v, 0):>4}")
    if args.list:
        print()
        for fn in sorted(verdicts, key=lambda f: (verdicts[f][0], f)):
            v, who = verdicts[fn]
            print(f"  {v:16s} {fn[:58]:58s} {sorted(who)[:2]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
