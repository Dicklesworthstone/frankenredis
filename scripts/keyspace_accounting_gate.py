#!/usr/bin/env python3
"""keyspace_accounting_gate.py — per-command keyspace_hits / keyspace_misses
accounting differential vs vendored redis 7.2.4.

This guards a recurring bug class: a command whose store path calls (or fails to
call) record_keyspace_lookup, so INFO `keyspace_hits` / `keyspace_misses` diverge
from upstream. Three real bugs were found this way and fixed — LPOS (recorded
NEITHER hit nor miss), BITCOUNT and BITPOS (DOUBLE-counted hits via a key_type
precheck + a counting store call). The cmdstat_keyspace_parity_gate checks the
*aggregate* totals across a fixed sequence; this gate is the *per-command* delta
view (flushall + reseed before each), which pinpoints exactly which command drifts.

Invariants checked:
  * READ commands: present key -> +1 hit, missing key -> +1 miss (BITCOUNT/BITPOS
    etc. exactly once, not twice).
  * WRITE commands: 0 hit / 0 miss — upstream lookupKeyWrite does NOT touch the
    keyspace hit/miss counters, on present OR missing keys.

Usage: keyspace_accounting_gate.py <oracle_port> <fr_port>
"""
import socket, re, sys

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

    def _l(s):
        while b"\r\n" not in s.buf:
            s.buf += s.s.recv(65536)
        l, s.buf = s.buf.split(b"\r\n", 1)
        return l

    def read(s):
        l = s._l()
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
            return None if n < 0 else [s.read() for _ in range(n)]
        return l

    def cmd(s, *a):
        s.s.sendall(enc(*a))
        return s.read()

    def hm(s):
        raw = b""
        s.s.sendall(enc("INFO", "stats"))
        while b"keyspace_misses" not in raw:
            raw += s.s.recv(65536)
        s.buf = b""
        return (
            int(re.search(rb"keyspace_hits:(\d+)", raw).group(1)),
            int(re.search(rb"keyspace_misses:(\d+)", raw).group(1)),
        )


SEED = [
    ["set", "k", "hello"], ["set", "n", "10"], ["sadd", "sx", "1", "2", "3"],
    ["sadd", "sy", "2", "3", "4"], ["hset", "hx", "f", "v"], ["rpush", "lx", "a", "b"],
    ["zadd", "zx", "1", "a", "2", "b"], ["zadd", "zy", "2", "b", "3", "c"],
    ["setbit", "bx", "20", "1"],
    ["xadd", "xs", "1-1", "f", "v"], ["set", "e", "v"],
    ["pfadd", "hll", "x", "y", "z"], ["geoadd", "g", "13.36", "38.11", "p1"],
    ["geoadd", "g", "15.08", "37.5", "p2"],
]

# (args, expected_hit_delta, expected_miss_delta) — expectations are asserted
# against redis at runtime too (we compare fr-delta == redis-delta), the literals
# document intent.
READS_HIT = [
    ["get", "k"], ["strlen", "k"], ["getrange", "k", "0", "2"], ["ttl", "e"],
    ["pttl", "e"], ["expiretime", "k"], ["pexpiretime", "k"], ["type", "k"],
    ["getbit", "bx", "20"], ["bitcount", "bx"], ["bitpos", "bx", "1"],
    ["llen", "lx"], ["lindex", "lx", "0"], ["lrange", "lx", "0", "-1"],
    ["lpos", "lx", "a"], ["scard", "sx"], ["sismember", "sx", "1"],
    ["smismember", "sx", "1", "9"], ["smembers", "sx"], ["hlen", "hx"],
    ["hget", "hx", "f"], ["hexists", "hx", "f"], ["hmget", "hx", "f"],
    ["hstrlen", "hx", "f"], ["hgetall", "hx"], ["zcard", "zx"], ["zscore", "zx", "a"],
    ["zmscore", "zx", "a", "b"], ["zrank", "zx", "a"], ["xlen", "xs"],
    ["object", "encoding", "k"], ["sintercard", "2", "sx", "sy"], ["mget", "k", "n"],
    ["hscan", "hx", "0"], ["sscan", "sx", "0"], ["zscan", "zx", "0"],
    # read-only set / zset algebra (record_source_key_lookups over each source key)
    ["sinter", "sx", "sy"], ["sunion", "sx", "sy"], ["sdiff", "sx", "sy"],
    ["zdiff", "2", "zx", "zy"], ["zinter", "2", "zx", "zy"],
    ["zintercard", "2", "zx", "zy"], ["zunion", "2", "zx", "zy"],
    ["pfcount", "hll"], ["geodist", "g", "p1", "p2"], ["geopos", "g", "p1"],
    ["geohash", "g", "p1"], ["sort_ro", "lx", "ALPHA"], ["zrange", "zx", "0", "-1"],
    ["zrangebyscore", "zx", "0", "10"], ["zrangebylex", "zx", "-", "+"],
    ["zrevrange", "zx", "0", "-1"], ["zrevrangebyscore", "zx", "10", "0"],
    ["zrevrangebylex", "zx", "+", "-"], ["zcount", "zx", "0", "10"],
    ["zlexcount", "zx", "-", "+"], ["xrange", "xs", "-", "+"],
    ["xrevrange", "xs", "+", "-"], ["substr", "k", "0", "2"],
    ["bitfield_ro", "bx", "get", "u8", "0"], ["geosearch", "g", "frommember", "p1", "byradius", "500", "km", "asc"],
    ["sort", "lx", "ALPHA"],
    # keyspace delta is deterministic even where the reply is random
    ["touch", "k"], ["touch", "k", "n", "no"], ["hrandfield", "hx"],
    ["srandmember", "sx"], ["zrandmember", "zx"], ["dump", "k"],
    ["georadius_ro", "g", "13.36", "38.11", "1000", "km"],
    ["georadiusbymember_ro", "g", "p1", "1000", "km"], ["zrank", "zx", "a", "withscore"],
    ["getex", "k"], ["lcs", "k", "e"], ["object", "refcount", "k"],
    ["object", "idletime", "k"], ["xinfo", "stream", "xs"],
]
# STORE / move variants: the source keys are reads (record hit/miss), the dest is
# a write (no counter). Placed here purely for the printed label — the gate asserts
# fr-delta == redis-delta empirically regardless of list.
STORE_AND_MOVE = [
    ["sinterstore", "d", "sx", "sy"], ["sunionstore", "d", "sx", "sy"],
    ["sdiffstore", "d", "sx", "sy"], ["zrangestore", "d", "zx", "0", "-1"],
    ["zdiffstore", "d", "2", "zx", "zy"], ["zinterstore", "d", "2", "zx", "zy"],
    ["zunionstore", "d", "2", "zx", "zy"], ["bitop", "and", "d", "bx", "k"],
    ["smove", "sx", "sy", "2"], ["lmove", "lx", "ld", "left", "right"],
    ["rpoplpush", "lx", "ld"], ["copy", "k", "kd"],
    # source-miss forms
    ["sinterstore", "d", "no", "sy"], ["zrangestore", "d", "no", "0", "-1"],
    ["bitop", "and", "d", "no", "k"], ["copy", "no", "kd"],
    ["geosearchstore", "gd", "g", "frommember", "p1", "byradius", "500", "km", "asc"],
    ["lmpop", "1", "lx", "left"], ["zmpop", "1", "zx", "min"],
    ["lpop", "lx", "1"], ["rpop", "lx", "1"], ["zpopmin", "zx"], ["zpopmax", "zx", "1"],
    ["sort", "lx", "by", "w_*", "get", "#", "get", "d_*"],
    ["lcs", "k", "e", "idx"],
    # write-family pop / miss forms
    ["lmpop", "1", "no", "left"], ["zmpop", "1", "no", "min"], ["zpopmin", "no"],
    # more reads (record hit/miss)
    ["hkeys", "hx"], ["hvals", "hx"], ["hkeys", "no"], ["hvals", "no"],
    ["sintercard", "1", "sx", "limit", "1"], ["lpos", "lx", "a", "count", "0"],
    # writes (must NOT bump counters) incl. read-modify and HLL merge
    ["bitfield", "bx", "get", "u8", "0"], ["bitfield", "bx", "incrby", "u8", "0", "1"],
    ["pfadd", "hll", "q"], ["pfmerge", "hd", "hll"], ["pfmerge", "hd", "hll", "no"],
    ["getex", "k", "ex", "100"], ["getex", "k", "persist"], ["getex", "no", "ex", "100"],
    ["zadd", "zx", "incr", "1", "a"], ["incrbyfloat", "n", "1.5"],
    ["hincrbyfloat", "hx", "f", "1.5"], ["hincrby", "hx", "cnt", "1"],
    ["setrange", "k", "0", "Z"], ["hsetnx", "hx", "nf", "1"],
    ["zadd", "zx", "gt", "ch", "9", "a"],
]
READS_MISS = [
    ["get", "no"], ["strlen", "no"], ["getrange", "no", "0", "1"], ["ttl", "no"],
    ["pttl", "no"], ["expiretime", "no"], ["type", "no"], ["getbit", "no", "0"],
    ["bitcount", "no"], ["bitpos", "no", "1"], ["llen", "no"], ["lpos", "no", "a"],
    ["scard", "no"], ["sismember", "no", "1"], ["smismember", "no", "1"],
    ["hlen", "no"], ["hget", "no", "f"], ["hexists", "no", "f"], ["hstrlen", "no", "f"],
    ["zcard", "no"], ["zscore", "no", "a"], ["zmscore", "no", "a"], ["xlen", "no"],
    ["object", "encoding", "no"], ["exists", "no"],
    ["hscan", "no", "0"], ["sscan", "no", "0"], ["zscan", "no", "0"],
]
WRITES = [
    ["set", "k", "v2"], ["append", "k", "x"], ["setrange", "k", "0", "Z"],
    ["incr", "n"], ["incrby", "n", "2"], ["decr", "n"], ["getset", "k", "z"],
    ["getdel", "k"], ["setnx", "k", "z"], ["setex", "k", "100", "z"],
    ["expire", "k", "100"], ["persist", "k"], ["pexpire", "k", "100"],
    ["sadd", "sx", "9"], ["srem", "sx", "1"], ["spop", "sx"], ["hset", "hx", "g", "1"],
    ["hdel", "hx", "f"], ["lpush", "lx", "z"], ["rpush", "lx", "z"], ["lpop", "lx"],
    ["rpop", "lx"], ["lset", "lx", "0", "q"], ["zadd", "zx", "5", "m"],
    ["zrem", "zx", "a"], ["zincrby", "zx", "1", "a"], ["setbit", "k", "3", "1"],
    ["del", "k"], ["rename", "k", "kr"], ["copy", "k", "kk"],
    # writes on a missing key still must not move the counters
    ["append", "no", "x"], ["incr", "no"], ["lpush", "no", "z"], ["sadd", "no", "9"],
    ["hset", "no", "g", "1"], ["zadd", "no", "1", "m"], ["expire", "no", "100"],
    ["setbit", "no", "3", "1"],
]


def reseed(c):
    c.cmd("flushall")
    for s in SEED:
        c.cmd(*s)


def delta(port, args):
    c = Conn(port)
    reseed(c)
    hb, mb = c.hm()
    c.cmd(*args)
    ha, ma = c.hm()
    c.s.close()
    return (ha - hb, ma - mb)


def main():
    fails = []
    for args in READS_HIT + READS_MISS + WRITES + STORE_AND_MOVE:
        rd = delta(OR, args)
        fr = delta(FRp, args)
        if rd != fr:
            fails.append((args, rd, fr))
    total = len(READS_HIT) + len(READS_MISS) + len(WRITES) + len(STORE_AND_MOVE)
    print("=" * 64)
    if fails:
        for a, rd, fr in fails:
            print(f"DIVERGE {' '.join(a)}  redis(h,m)={rd} fr={fr}")
        print(f"FAIL — {len(fails)}/{total} per-command keyspace-accounting divergence(s)")
        return 1
    print(f"PASS — per-command keyspace_hits/misses byte-exact vs redis 7.2.4 "
          f"({total} commands: {len(READS_HIT)} read-hit, {len(READS_MISS)} read-miss, "
          f"{len(WRITES)} write)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
