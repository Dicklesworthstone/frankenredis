#!/usr/bin/env python3
"""Differential: COMMAND GETKEYS / GETKEYSANDFLAGS vs vendored redis 7.2.4.

The per-key flag derivation (RO/RW/OW/RM/access/update/insert/delete/...) and the
movable-/keyspec-key extraction (STORE targets, NUMKEYS-counted variadics, GEO
STORE, *STORE algebra, EVAL/XREAD) are a historically fertile divergence surface
(generic write-fallback mis-flags movable / per-keyspec commands). This probes
both subcommands across a broad command set and diffs the raw replies.

Usage: getkeys_flags_differ.py <oracle_port> <fr_port>
Exit 0 = parity; 1 = divergence(s).
"""
import socket, sys

def conn(p): return socket.create_connection(("127.0.0.1", p), timeout=10)

def cmd(s, *args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str): a = a.encode()
        elif isinstance(a, int): a = str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(out)
    return read(s)

def read(s):
    buf = b""
    while b"\r\n" not in buf:
        buf += s.recv(65536)
    line, rest = buf.split(b"\r\n", 1)
    t, body = line[:1], line[1:]
    if t in (b'+', b':'): return line.decode()
    if t == b'-': return line.decode()
    if t == b'$':
        n = int(body)
        if n < 0: return None
        while len(rest) < n + 2: rest += s.recv(65536)
        # stash leftover back
        s._lo = rest[n+2:]
        return rest[:n]
    if t in (b'*', b'~', b'>'):
        n = int(body)
        # re-buffer rest by prepending; simplest: use a stateful reader
        raise RuntimeError("use Reader")
    return line.decode()

# proper buffered reader
class R:
    def __init__(s, p): s.s = conn(p); s.buf = b""
    def _line(s):
        while b"\r\n" not in s.buf: s.buf += s.s.recv(65536)
        l, s.buf = s.buf.split(b"\r\n", 1); return l
    def _n(s, n):
        while len(s.buf) < n + 2: s.buf += s.s.recv(65536)
        d = s.buf[:n]; s.buf = s.buf[n+2:]; return d
    def read(s):
        l = s._line(); t = l[:1]
        if t in (b'+', b':'): return l.decode()
        if t == b'-': return l.decode()
        if t == b'$':
            n = int(l[1:]); return None if n < 0 else s._n(n)
        if t in (b'*', b'~', b'>'):
            n = int(l[1:]); return None if n < 0 else [s.read() for _ in range(n)]
        return l.decode()
    def cmd(s, *args):
        o = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, str): a = a.encode()
            elif isinstance(a, int): a = str(a).encode()
            o += b"$%d\r\n%s\r\n" % (len(a), a)
        s.s.sendall(o); return s.read()

# (subcommand, *command-args) — args are the full command whose keys we extract
PROBES = [
    ["set", "k", "v"], ["get", "k"], ["getex", "k"], ["getdel", "k"],
    ["mset", "k1", "v1", "k2", "v2"], ["mget", "k1", "k2", "k3"],
    ["exists", "k1", "k2"], ["unlink", "k1", "k2"],
    ["expire", "k", "100"], ["copy", "src", "dst"], ["copy", "src", "dst", "REPLACE"],
    ["copy", "src", "dst", "DB", "1"], ["copy", "src", "dst", "DB", "1", "REPLACE"],
    ["move", "src", "1"],
    ["rename", "a", "b"], ["smove", "s", "d", "m"],
    ["lmpop", "2", "k1", "k2", "LEFT"], ["zmpop", "2", "z1", "z2", "MIN"],
    ["blmpop", "0", "2", "k1", "k2", "LEFT"], ["bzmpop", "0", "2", "z1", "z2", "MIN"],
    ["blpop", "l1", "l2", "0"], ["brpop", "l1", "l2", "0"],
    ["brpoplpush", "src", "dst", "0"], ["blmove", "src", "dst", "LEFT", "RIGHT", "0"],
    ["bzpopmin", "z1", "z2", "0"], ["bzpopmax", "z1", "z2", "0"],
    ["sort", "k"], ["sort", "k", "STORE", "dst"],
    ["sort_ro", "k"],
    ["georadius", "k", "0", "0", "1", "m"],
    ["georadius", "k", "0", "0", "1", "m", "STORE", "dst"],
    ["georadius", "k", "0", "0", "1", "m", "STOREDIST", "dst"],
    ["geosearchstore", "dst", "src", "FROMLONLAT", "0", "0", "BYRADIUS", "1", "m", "ASC"],
    ["zrangestore", "dst", "src", "0", "-1"],
    ["zadd", "k", "1", "m"], ["zrangebyscore", "k", "0", "1"],
    ["sinterstore", "dst", "s1", "s2"], ["sunionstore", "dst", "s1", "s2"],
    ["zunionstore", "dst", "2", "z1", "z2", "WEIGHTS", "1", "2"],
    ["zinterstore", "dst", "2", "z1", "z2"],
    ["zdiff", "2", "z1", "z2"], ["zunion", "2", "z1", "z2"],
    ["bitop", "AND", "dst", "s1", "s2"],
    ["pfcount", "h1", "h2"], ["pfmerge", "dst", "s1", "s2"],
    ["lcs", "k1", "k2"], ["lcs", "k1", "k2", "LEN"],
    ["eval", "return 1", "2", "k1", "k2", "a", "b"],
    ["evalsha", "abc", "1", "k1"],
    ["fcall", "f", "2", "k1", "k2", "a"],
    ["xread", "COUNT", "1", "STREAMS", "s1", "s2", "0", "0"],
    ["xreadgroup", "GROUP", "g", "c", "STREAMS", "s1", "0"],
    ["xadd", "s", "*", "f", "v"], ["xlen", "s"],
    ["object", "encoding", "k"], ["memory", "usage", "k"],
    ["ttl", "k"], ["pttl", "k"], ["persist", "k"], ["touch", "k1", "k2"], ["type", "k"],
    ["getrange", "k", "0", "-1"], ["setrange", "k", "0", "x"],
    ["setbit", "k", "7", "1"], ["bitcount", "k"],
    ["lpush", "k", "a"], ["linsert", "k", "BEFORE", "a", "b"],
    ["incrby", "k", "1"], ["append", "k", "x"],
    ["watch", "k1", "k2"], ["sintercard", "2", "s1", "s2"],
]

def diff(od, fr, sub):
    bad = 0
    for args in PROBES:
        ro = od.cmd("command", sub, *args)
        rf = fr.cmd("command", sub, *args)
        if ro != rf:
            bad += 1
            print(f"DIVERGE [COMMAND {sub} {' '.join(args)}]\n  oracle: {ro}\n  fr    : {rf}")
    return bad

def main():
    op = int(sys.argv[1]); fp = int(sys.argv[2])
    od, fr = R(op), R(fp)
    total = 0
    for sub in ("getkeys", "getkeysandflags"):
        total += diff(od, fr, sub)
    print("-" * 60)
    if total:
        print(f"FAIL — {total} divergence(s)")
        return 1
    print(f"PASS — COMMAND GETKEYS/GETKEYSANDFLAGS byte-exact across {len(PROBES)} commands")
    return 0

if __name__ == "__main__":
    sys.exit(main())
