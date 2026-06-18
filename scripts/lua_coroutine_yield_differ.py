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
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []
    for label, script in CASES:
        ro, rf = ev(od, script), ev(fr, script)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")
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
    main()
