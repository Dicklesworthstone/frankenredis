#!/usr/bin/env python3
"""Differential gate: INFO used_memory tracks the keyspace down on FLUSHALL/DEL,
vs redis 7.2.4.

fr's used_memory is a cached O(n) estimate recomputed every ~64 mutations. FLUSHALL
empties the keyspace but only bumps the mutation counter by 1 — so without an
explicit cache reset, estimate_memory_usage_bytes kept returning the pre-flush PEAK
(e.g. used_memory=53MB with 0 keys vs redis ~2MB). This gate loads a batch of keys,
records the peak used_memory, FLUSHALLs, and asserts used_memory COLLAPSES to a
small fraction of the peak on BOTH servers (redis to its ~2MB baseline, fr to its
keyspace-only ~0) — catching a stuck/stale aggregate-memory cache. It also checks
the post-FLUSH value is far below peak after a DEL-heavy path.

Exact byte parity is NOT asserted (fr's used_memory models the keyspace without
redis's fixed server-overhead baseline); the invariant is the DROP.

Usage: info_memory_flush_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=divergence.
"""
import socket, sys
def C(p): return socket.create_connection(("127.0.0.1", p), timeout=10)
class R:
    def __init__(s, p): s.s=C(p); s.buf=b""
    def _l(s):
        while b"\r\n" not in s.buf: s.buf+=s.s.recv(1<<20)
        l,s.buf=s.buf.split(b"\r\n",1); return l
    def _n(s,n):
        while len(s.buf)<n+2: s.buf+=s.s.recv(1<<20)
        d=s.buf[:n]; s.buf=s.buf[n+2:]; return d
    def read(s):
        l=s._l(); t=l[:1]
        if t in (b'+',b':',b'-'): return l.decode('latin1')
        if t==b'$': n=int(l[1:]); return None if n<0 else s._n(n).decode('latin1')
        if t==b'*': n=int(l[1:]); return None if n<0 else [s.read() for _ in range(n)]
        return l.decode('latin1')
    def cmd(s,*a):
        o=b"*%d\r\n"%len(a)
        for x in a:
            x=x.encode() if isinstance(x,str) else x
            o+=b"$%d\r\n%s\r\n"%(len(x),x)
        s.s.sendall(o); return s.read()

def used_memory(srv):
    info = srv.cmd("info", "memory")
    for line in info.split("\r\n"):
        if line.startswith("used_memory:"):
            return int(line.split(":")[1])
    return -1

div = 0
def check(label, cond, detail):
    global div
    if not cond:
        div += 1; print(f"DIVERGE {label}: {detail}")

def run(srv, name, n=50000):
    srv.cmd("flushall")
    base = used_memory(srv)
    for i in range(n):
        srv.cmd("set", f"memflushkey:{i}", "x"*40)
    peak = used_memory(srv)
    srv.cmd("flushall")
    after = used_memory(srv)
    # rebuild + delete via DEL (not flush) to exercise the per-key adjust path
    for i in range(n // 2):
        srv.cmd("set", f"dk:{i}", "y"*40)
    peak2 = used_memory(srv)
    for i in range(n // 2):
        srv.cmd("del", f"dk:{i}")
    after_del = used_memory(srv)
    return dict(base=base, peak=peak, after=after, peak2=peak2, after_del=after_del)

def main():
    od=R(int(sys.argv[1])); fr=R(int(sys.argv[2]))
    o = run(od, "redis"); f = run(fr, "fr")
    for name, m in (("redis", o), ("fr", f)):
        grew = m["peak"] > m["base"] + 500_000
        check(f"{name}-grew", grew, f"peak={m['peak']} base={m['base']} (expected growth on 50k keys)")
        # after FLUSHALL used_memory must collapse to <20% of peak (the bug left it at ~100%)
        collapsed = m["after"] < m["base"] + (m["peak"] - m["base"]) * 0.20
        check(f"{name}-flush-collapse", collapsed,
              f"after-flush={m['after']} peak={m['peak']} base={m['base']} (used_memory stayed near peak)")
        # after DEL-all used_memory must also drop well below peak2
        del_dropped = m["after_del"] < m["base"] + (m["peak2"] - m["base"]) * 0.30
        check(f"{name}-del-drop", del_dropped,
              f"after-del={m['after_del']} peak2={m['peak2']} base={m['base']}")
    if div:
        print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print(f"OK: INFO used_memory collapses on FLUSHALL + DEL on both fr and redis "
          f"(redis {o['peak']//1048576}->{o['after']//1048576}MB, fr {f['peak']//1048576}->{f['after']//1048576}MB)")

if __name__=="__main__": main()
