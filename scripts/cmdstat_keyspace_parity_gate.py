#!/usr/bin/env python3
"""cmdstat_keyspace_parity_gate.py — INFO commandstats + keyspace-stat parity
gate vs vendored redis 7.2.4.

The borrow-dispatch fast paths (GET/SET/MGET/MSET/GETRANGE/TTL/PTTL/TYPE/GETBIT/
BITCOUNT/EXPIRE/LPOS/OBJECT ENCODING/ECHO/PING/DBSIZE/INCR/...) bypass the generic
command machinery, so each MUST still increment the SAME deterministic counters
the generic path would: INFO commandstats `cmdstat_<name>:calls` (incl. the
container `object|encoding` parent|sub form) and INFO stats `keyspace_hits` /
`keyspace_misses`. A fast path that calls a subtly different store method can
diverge here (e.g. LPOS: store.lpos records a keyspace hit but store.lpos_full
does not — caught during development). This gate locks that invariant in.

Method: run an identical scripted command sequence (hitting present + missing +
wrong-type keys across the fast-pathed command set) against fr and redis, then
compare per-command `calls` counts and keyspace_hits/misses exactly. usec is
timing-dependent and ignored.

Usage: cmdstat_keyspace_parity_gate.py <oracle_port> <fr_port>
"""
import socket, sys, re

OR = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FRp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def mk(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=10)
    s.settimeout(10)
    return s


def enc(*a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


class Conn:
    def __init__(s, p):
        s.s = mk(p)
        s.buf = b""

    def _line(s):
        while b"\r\n" not in s.buf:
            s.buf += s.s.recv(65536)
        l, s.buf = s.buf.split(b"\r\n", 1)
        return l

    def read(s):
        l = s._line()
        t = l[:1]
        if t in (b"+", b":", b"-"):
            return l
        if t == b"$":
            n = int(l[1:])
            if n < 0:
                return None
            while len(s.buf) < n + 2:
                s.buf += s.s.recv(65536)
            d = s.buf[:n]
            s.buf = s.buf[n + 2:]
            return d
        if t in (b"*", b"~", b"%"):
            n = int(l[1:])
            if n < 0:
                return None
            return [s.read() for _ in range(n * 2 if t == b"%" else n)]
        return l

    def cmd(s, *a):
        s.s.sendall(enc(*a))
        return s.read()

    def info(s, section):
        s.s.sendall(enc("INFO", section))
        raw = s.read()
        return raw.decode("latin1") if isinstance(raw, (bytes, bytearray)) else ""


# Command sequence exercising the borrow fast paths across present / missing /
# wrong-type keys (deterministic keyspace hit/miss + call counts).
SEQ = [
    # CONFIG RESETSTAT zeroes commandstats + keyspace_hits/misses so the gate is
    # robust against a reused server with accumulated counters (FLUSHALL alone
    # does NOT reset stats).
    ["config", "resetstat"],
    ["flushall"],
    ["set", "s", "12345"], ["set", "s2", "hello world not an int"],
    ["rpush", "l", "a", "b", "c", "b"], ["sadd", "st", "1", "2", "3"],
    ["hset", "h", "f", "v"], ["zadd", "z", "1", "a"], ["setbit", "bm", "100", "1"],
    ["xadd", "x", "1-1", "f", "v"], ["set", "e", "v"], ["expire", "e", "100000"],
    # reads: hits
    ["get", "s"], ["get", "s2"], ["strlen", "s"], ["ttl", "s"], ["pttl", "s"],
    ["type", "s"], ["type", "l"], ["getbit", "bm", "100"], ["bitcount", "bm"],
    ["bitpos", "bm", "1"], ["bitpos", "bm", "0"],
    ["lpos", "l", "b"], ["object", "encoding", "s"], ["object", "encoding", "l"],
    ["mget", "s", "s2"], ["llen", "l"], ["scard", "st"], ["zcard", "z"], ["hlen", "h"],
    ["sismember", "st", "1"], ["smismember", "st", "1", "2", "9"],
    ["hexists", "h", "f"], ["zscore", "z", "a"], ["hget", "h", "f"],
    ["getrange", "s", "0", "2"], ["exists", "s"], ["echo", "hi"], ["ping"], ["dbsize"],
    # newer borrow fast paths (cold-cmd audit): MEMORY USAGE / COMMAND COUNT /
    # EXPIRETIME / PEXPIRETIME / XLEN / HSTRLEN
    ["memory", "usage", "s"], ["command", "count"], ["expiretime", "e"],
    ["pexpiretime", "e"], ["expiretime", "s"], ["xlen", "x"], ["hstrlen", "h", "f"],
    # reads: misses
    ["get", "nope"], ["ttl", "nope"], ["pttl", "nope"], ["type", "nope"],
    ["getbit", "nope", "0"], ["bitcount", "nope"], ["lpos", "nope", "x"],
    ["object", "encoding", "nope"], ["strlen", "nope"], ["llen", "nope"],
    ["sismember", "nope", "x"], ["getrange", "nope", "0", "1"], ["exists", "nope"],
    ["expiretime", "nope"], ["pexpiretime", "nope"], ["xlen", "nope"],
    ["hstrlen", "nope", "f"], ["memory", "usage", "nope"], ["bitpos", "nope", "1"],
    ["smismember", "nope", "a", "b"],
    # writes
    ["incr", "s"], ["expire", "s", "10000"], ["append", "s2", "!"],
    ["mset", "k1", "v1", "k2", "v2"], ["set", "k3", "v3"],
    # re-reads after writes
    ["get", "s"], ["ttl", "s"], ["dbsize"],
]


def kv_stats(info_text):
    out = {}
    for key in ("keyspace_hits", "keyspace_misses"):
        m = re.search(rf"^{key}:(\d+)", info_text, re.M)
        out[key] = int(m.group(1)) if m else None
    return out


def cmdstat_calls(info_text):
    calls = {}
    for m in re.finditer(r"^cmdstat_([^:]+):calls=(\d+)", info_text, re.M):
        calls[m.group(1)] = int(m.group(2))
    return calls


def run(port):
    c = Conn(port)
    for cmd in SEQ:
        c.cmd(*cmd)
    stats = kv_stats(c.info("stats"))
    calls = cmdstat_calls(c.info("commandstats"))
    c.s.close()
    return stats, calls


def main():
    o_stats, o_calls = run(OR)
    f_stats, f_calls = run(FRp)
    fails = []
    # keyspace hits/misses must match exactly
    for k in ("keyspace_hits", "keyspace_misses"):
        if o_stats[k] != f_stats[k]:
            fails.append(f"{k}: redis={o_stats[k]} fr={f_stats[k]}")
    # per-command call counts: every command we issued must have matching calls.
    # (INFO itself differs in count between the two runs because we call it
    # twice; ignore the 'info' row.)
    names = (set(o_calls) | set(f_calls)) - {"info"}
    for name in sorted(names):
        ov, fv = o_calls.get(name, 0), f_calls.get(name, 0)
        if ov != fv:
            fails.append(f"cmdstat_{name}:calls redis={ov} fr={fv}")
    print("=" * 60)
    if fails:
        for f in fails:
            print("DIVERGE", f)
        print(f"FAIL — {len(fails)} cmdstat/keyspace-stat divergence(s)")
        return 1
    print(f"PASS — cmdstat calls + keyspace hits/misses byte-exact vs redis 7.2.4")
    print(f"  keyspace_hits={f_stats['keyspace_hits']} keyspace_misses={f_stats['keyspace_misses']}"
          f"  ({len(names)} command rows compared)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
