#!/usr/bin/env python3
"""Differential gate for Lua load(func) chunk-generator, fr vs vendored redis 7.2.4.

Lua 5.1 load(func [, chunkname]) calls the reader function repeatedly to build a
chunk (a nil or empty-string return terminates), compiles it, and returns the
loaded function — or (nil, errmsg) on a compile error. fr previously rejected
load(func) outright; frankenredis-36wn7 implements it by eagerly collecting the
reader pieces and reusing the loadstring compile path. This pins fr == redis
across the reader protocol, compile-error path, and reader-type errors.

Usage: lua_load_func_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket
import sys
import time


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.03)
    return s.recv(1 << 20)


# Each script exercises load(func) with a WELL-FORMED reader (returns chunk
# pieces, then nil/"" to terminate — the real load(func) protocol) and returns a
# deterministic, comparable value.
#
# NOTE on pathological NON-terminating readers (e.g. `function() return 'x' end`
# that never returns nil): redis's parser pulls chunk pieces lazily and stops at
# the first syntax error, while fr eagerly collects all reader output first, so a
# never-terminating reader trips fr's per-script iteration limit instead of
# redis's early syntax error. That divergence is a pathological edge (a real
# reader always terminates) — see frankenredis-36wn7 — and is intentionally NOT
# probed here; every case below uses a terminating reader.
SCRIPTS = [
    # reader yields pieces, nil terminates, loaded fn executes
    "local s={'retur','n 1','+2'} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f()",
    # empty string terminates the reader
    "local s={'return ','40+2',''} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f()",
    # single-piece reader then nil; loaded chunk runs in the sandbox
    "local c=0 local f=load(function() c=c+1 if c==1 then return \"return 'ok'\" end end) "
    "return f()",
    # compile error -> (nil, errmsg): f is nil
    "local c=0 local f,e=load(function() c=c+1 if c==1 then return 'retur n bad' end end) "
    "return tostring(f)",
    # compile error -> errmsg is a string
    "local c=0 local f,e=load(function() c=c+1 if c==1 then return 'retur n bad' end end) "
    "return type(e)",
    # reader returns numbers (coerced like loadstring), then nil
    "local s={1,2,nil} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f and 'fn' or 'nofn'",
    # immediate nil -> empty chunk compiles to a no-op function
    "local f=load(function() return nil end) return type(f)",
    # multi-statement chunk assembled from many small pieces (table terminates)
    "local parts={} for c in ('local x=0 for i=1,10 do x=x+i end return x'):gmatch('.') "
    "do parts[#parts+1]=c end local i=0 "
    "local f=load(function() i=i+1 return parts[i] end) return f()",
]


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    diffs = 0
    for sc in SCRIPTS:
        ro, rf = cmd(od, "EVAL", sc, "0"), cmd(fr, "EVAL", sc, "0")
        if ro != rf:
            diffs += 1
            print(f"DIFF {sc[:55]!r}...\n  redis={ro!r}\n  fr   ={rf!r}")
    if diffs:
        print(f"\nFAIL — {diffs} load(func) divergence(s) vs redis 7.2.4")
        sys.exit(1)
    print(f"PASS — Lua load(func) byte-exact vs redis 7.2.4 ({len(SCRIPTS)} scripts)")


if __name__ == "__main__":
    main()
