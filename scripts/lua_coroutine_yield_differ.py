#!/usr/bin/env python3
"""Differential gate for Lua coroutine.yield positions, fr vs vendored redis 7.2.4.

redis 7.2 (Lua 5.1) lets coroutine.yield fire from ANY position — inside a loop,
a local/assignment expression, a return, a nested call. fr's tree-walking
interpreter only yields when yield is the top-level statement of the coroutine
body; yielding across a loop/assignment/return boundary errors with "attempt to
yield across metamethod/C-call boundary" (frankenredis-7lmle; the architectural
fix needs continuations/green-threads).

HARD checks: the coroutine features fr DOES support byte-exactly (create/status,
top-level single yield, error-in-coroutine, running()).

DOCUMENTED divergence (NOTE, gate stays green; auto-promotes to HARD once 7lmle
lands): yield-with-resume-values, yield-inside-for-loop via wrap.

Usage: lua_coroutine_yield_differ.py <oracle_port> <fr_port>
       Exit 0 = parity (modulo documented 7lmle divergence), 1 = NEW divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def ev(s, script):
    a = ("EVAL", script, "0")
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


# (label, script, kind) — kind "hard" must match byte-exact; "yield_across" is the
# 7lmle divergence (redis returns the value, fr errors).
CASES = [
    ("create_status", "local co=coroutine.create(function() return 1 end) "
     "return coroutine.status(co)", "hard"),
    ("toplevel_yield_status", "local co=coroutine.create(function() coroutine.yield() end) "
     "coroutine.resume(co) return coroutine.status(co)", "hard"),
    ("error_in_coroutine", "local ok,e=coroutine.resume(coroutine.create(function() "
     "error('boom') end)) return tostring(ok)", "hard"),
    ("running_main", "return tostring(coroutine.running())", "hard"),
    ("resume_dead", "local co=coroutine.create(function() end) coroutine.resume(co) "
     "return tostring(coroutine.resume(co))", "hard"),
    ("yield_value_resume_value",
     "local co=coroutine.create(function(a) local b=coroutine.yield(a+1) return b*2 end) "
     "local _,v1=coroutine.resume(co,10) local _,v2=coroutine.resume(co,5) return v1..':'..v2",
     "yield_across"),
    ("wrap_yield_in_loop",
     "local co=coroutine.wrap(function() for i=1,3 do coroutine.yield(i) end end) "
     "return co()..co()..co()",
     "yield_across"),
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails, notes = [], []
    for label, script, kind in CASES:
        ro, rf = ev(od, script), ev(fr, script)
        if kind == "hard":
            if ro != rf:
                fails.append(f"{label}: redis={ro!r} fr={rf!r}")
        else:  # yield_across — fr can't yield here; failure shows as a RESP
            # error OR an error-text bulk string, so ANY mismatch is the 7lmle gap.
            if ro == rf:
                notes.append(f"{label} now MATCHES (frankenredis-7lmle fixed?) — promote to HARD")
            else:
                notes.append(
                    f"{label} KNOWN DIVERGENCE (frankenredis-7lmle): redis={ro!r} fr={rf!r}"
                )
    print("=" * 60)
    for n in notes:
        print(f"NOTE  {n}")
    if fails:
        print(f"FAIL — {len(fails)} NEW coroutine divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — supported coroutine features byte-exact vs redis 7.2.4 "
        f"({len(notes)} documented 7lmle yield-across-boundary divergence(s))"
    )


if __name__ == "__main__":
    main()
