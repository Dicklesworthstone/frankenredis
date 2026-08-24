#!/usr/bin/env python3
"""Differential gate for Lua coroutine.yield positions, fr vs vendored redis 7.2.4.

redis 7.2 (Lua 5.1) lets coroutine.yield fire from positions that require the
interpreter to suspend and later continue the surrounding statement. The
frankenredis-7lmle continuation fix covers the direct loop/assignment/return
cases below, so this gate now treats those probes as hard parity checks.

HARD checks: create/status, top-level single yield, error-in-coroutine,
running(), resume-dead behavior, yield-with-resume-values, and direct
yield-across-boundary continuations.

Usage: lua_coroutine_yield_differ.py <oracle_port> <fr_port>
       Exit 0 = parity, 1 = divergence.
"""
import sys
from contextlib import contextmanager

from _respread import cmd
from _respread import conn as _conn


@contextmanager
def conn(p):
    """The shared-reader socket, closed deterministically on scope exit."""
    socket_handle = _conn(p)
    try:
        yield socket_handle
    finally:
        socket_handle.close()


def ev(s, script):
    return cmd(s, "EVAL", script, "0")


# (label, script) — every case must match byte-exactly.
CASES = [
    ("create_status", "local co=coroutine.create(function() return 1 end) "
     "return coroutine.status(co)"),
    ("toplevel_yield_status", "local co=coroutine.create(function() coroutine.yield() end) "
     "coroutine.resume(co) return coroutine.status(co)"),
    ("error_in_coroutine", "local ok,e=coroutine.resume(coroutine.create(function() "
     "error('boom') end)) return tostring(ok)"),
    ("running_main", "return tostring(coroutine.running())"),
    ("resume_dead", "local co=coroutine.create(function() end) coroutine.resume(co) "
     "return tostring(coroutine.resume(co))"),
    ("yield_value_resume_value",
     "local co=coroutine.create(function(a) local b=coroutine.yield(a+1) return b*2 end) "
     "local _,v1=coroutine.resume(co,10) local _,v2=coroutine.resume(co,5) return v1..':'..v2"),
    ("yield_in_assignment",
     "local co=coroutine.create(function() local x=0 x=coroutine.yield(4) return x+1 end) "
     "local _,v=coroutine.resume(co) local _,r=coroutine.resume(co,8) return v..':'..r"),
    ("yield_in_return",
     "local co=coroutine.create(function() return coroutine.yield('a','b') end) "
     "local _,a,b=coroutine.resume(co) local _,x,y=coroutine.resume(co,'x','y') "
     "return a..b..':'..x..y"),
    ("wrap_yield_in_loop",
     "local co=coroutine.wrap(function() for i=1,3 do coroutine.yield(i) end end) "
     "return co()..co()..co()"),
    ("yield_in_for_local_assign",
     "local co=coroutine.create(function() local total=0 for i=1,2 do "
     "local value=coroutine.yield(i) total=total+value end return total end) "
     "local _,a=coroutine.resume(co) local _,b=coroutine.resume(co,10) "
     "local _,total=coroutine.resume(co,20) return a..':'..b..':'..total"),
]


def _record_mismatch(fails, label, oracle, fr):
    if oracle != fr:
        fails.append(f"{label}: redis={oracle!r} fr={fr!r}")


def _self_test():
    """A wrong continuation result must be reported by the live predicate."""
    fails = []
    _record_mismatch(fails, "planted_yield_result", b"$3\r\n1:3\r\n", b"$3\r\n1:4\r\n")
    if len(fails) != 1 or "planted_yield_result" not in fails[0]:
        print(f"SELF-TEST FAIL: planted coroutine mismatch was not reported: {fails!r}")
        return 1
    print("SELF-TEST PASS: coroutine gate catches a planted wrong reply")
    return 0


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    with conn(op) as od, conn(fp) as fr:
        fails = []
        for label, script in CASES:
            ro, rf = ev(od, script), ev(fr, script)
            _record_mismatch(fails, label, ro, rf)
        print("=" * 60)
        if fails:
            print(f"FAIL — {len(fails)} coroutine divergence(s) vs redis 7.2.4:")
            for x in fails:
                print(f"  {x}")
            sys.exit(1)
        print(
            "PASS — coroutine yield continuation features byte-exact vs redis 7.2.4 "
            f"({len(CASES)} hard checks)"
        )


if __name__ == "__main__":
    raise SystemExit(_self_test() if "--self-test" in sys.argv else main())
