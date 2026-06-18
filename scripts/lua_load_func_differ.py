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


# Each script exercises load(func) and returns a deterministic, comparable value.
SCRIPTS = [
    # basic: reader yields pieces, nil terminates, loaded fn executes
    "local s={'retur','n 1','+2'} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f()",
    # empty string terminates the reader
    "local s={'return ','40+2',''} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f()",
    # loaded chunk sees KEYS/ARGV-free globals + can call redis (sandbox inherited)
    "local f=load(function() return \"return 'ok'\" end) return f()",
    # compile error -> (nil, errmsg): check the function slot is nil
    "local f,e=load(function() return 'retur n bad' end) return tostring(f)",
    # compile error -> errmsg is a string
    "local f,e=load(function() return 'retur n bad' end) return type(e)",
    # reader returns a non-string, non-nil -> error surfaced via pcall
    "local ok,e=pcall(function() return load(function() return {} end) end) "
    "return tostring(ok)",
    # reader returns numbers (coerced like loadstring)
    "local s={1,2,nil} local i=0 "
    "local f=load(function() i=i+1 return s[i] end) return f and 'fn' or 'nofn'",
    # immediate nil -> empty chunk compiles to a no-op function returning nothing
    "local f=load(function() return nil end) return type(f)",
    # multi-statement chunk assembled from many small pieces
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
