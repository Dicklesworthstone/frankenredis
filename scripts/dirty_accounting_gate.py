#!/usr/bin/env python3
"""dirty_accounting_gate.py — per-command server `dirty` (rdb_changes_since_last_save)
accounting differential vs vendored redis 7.2.4.

The `dirty` counter (INFO persistence `rdb_changes_since_last_save`) counts the
number of logical changes a write makes. It drives the RDB auto-save cadence
(save N M) AND replication/AOF feed sizing, so a per-command drift from upstream
changes when fr snapshots / how much it propagates. This is the dirty-counter
sibling of keyspace_accounting_gate.py: same per-command delta methodology, a
different invariant.

Bug class this guards (already seen): a command implemented as a composite of
store helpers where EACH helper does `dirty += 1`, so the command over-counts
(e.g. MOVE = copy + del double-bumped before frankenredis-movedirty normalized it
to one). Also under-count: a write that forgets to bump dirty at all.

Method: for each command, FLUSHALL + reseed, snapshot dirty, run the command,
snapshot again, and assert fr-delta == redis-delta. The absolute base differs
between servers (accumulated history) so only the per-command delta is compared;
the comparison is empirical (we don't hardcode redis's rule), so list placement
is just documentation.

Usage: dirty_accounting_gate.py <oracle_port> <fr_port>
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
            return None if n < 0 else [s.read() for _ in range(n * (2 if t == b"%" else 1))]
        return l

    def cmd(s, *a):
        s.s.sendall(enc(*a))
        return s.read()

    def dirty(s):
        s.s.sendall(enc("INFO", "persistence"))
        raw = s.read()
        txt = raw.decode("latin1") if isinstance(raw, (bytes, bytearray)) else ""
        s.buf = b""
        m = re.search(r"rdb_changes_since_last_save:(\d+)", txt)
        return int(m.group(1)) if m else None


SEED = [
    ["set", "k", "hello"], ["set", "n", "10"], ["set", "k2", "world"],
    ["sadd", "sx", "1", "2", "3"], ["sadd", "sy", "2", "3", "4"],
    ["hset", "hx", "f1", "v1", "f2", "v2"], ["rpush", "lx", "a", "b", "c"],
    ["zadd", "zx", "1", "a", "2", "b", "3", "c"], ["zadd", "zy", "2", "b", "3", "c"],
    ["setbit", "bx", "20", "1"], ["xadd", "xs", "1-1", "f", "v"],
    ["pfadd", "hll", "x", "y", "z"], ["pfadd", "hll2", "y", "z", "w"],
    ["geoadd", "g", "13.36", "38.11", "p1"], ["geoadd", "g", "15.08", "37.5", "p2"],
    ["set", "e", "v"], ["expire", "e", "10000"],
]

# Each entry is just a command argv; the gate compares fr-delta == redis-delta.
WRITES = [
    # string
    ["set", "k", "v2"], ["set", "new", "v"], ["setnx", "k", "z"], ["setnx", "new2", "z"],
    ["append", "k", "x"], ["append", "newk", "x"], ["setrange", "k", "0", "Z"],
    ["incr", "n"], ["incrby", "n", "5"], ["decr", "n"], ["decrby", "n", "2"],
    ["incrbyfloat", "n", "1.5"], ["getset", "k", "z"], ["getdel", "k"], ["getdel", "no"],
    ["setex", "k", "100", "z"], ["psetex", "k", "100000", "z"], ["setbit", "k", "3", "1"],
    ["mset", "a", "1", "b", "2"], ["msetnx", "c", "1", "d", "2"], ["msetnx", "k", "1", "z", "2"],
    ["getex", "k", "ex", "100"], ["getex", "k", "persist"], ["getex", "k"],
    # expire family
    ["expire", "k", "100"], ["pexpire", "k", "100000"], ["expireat", "k", "99999999999"],
    ["persist", "e"], ["persist", "k"], ["expire", "no", "100"],
    # del / unlink / rename / copy / move
    ["del", "k"], ["del", "k", "k2"], ["del", "no"], ["unlink", "sx"],
    ["rename", "k", "kr"], ["renamenx", "k2", "k2r"], ["copy", "k", "kc"], ["copy", "no", "kc"],
    ["move", "k", "1"], ["move", "no", "1"],
    # list
    ["lpush", "lx", "z"], ["rpush", "lx", "y", "w"], ["lpop", "lx"], ["rpop", "lx", "2"],
    ["lset", "lx", "0", "Q"], ["linsert", "lx", "before", "b", "X"], ["lrem", "lx", "0", "a"],
    ["ltrim", "lx", "0", "1"], ["rpoplpush", "lx", "ld"], ["lmove", "lx", "ld", "left", "right"],
    ["lpush", "lx", "1", "2", "3"], ["lpop", "lx", "2"],
    # set
    ["sadd", "sx", "9"], ["sadd", "sx", "1"], ["sadd", "sx", "7", "8"], ["srem", "sx", "1"],
    ["spop", "sx"], ["spop", "sy", "2"], ["smove", "sx", "sy", "2"], ["smove", "sx", "sy", "999"],
    ["sinterstore", "sd", "sx", "sy"], ["sunionstore", "sd2", "sx", "sy"],
    ["sdiffstore", "sd3", "sx", "sy"],
    # hash
    ["hset", "hx", "f3", "v3"], ["hset", "hx", "f1", "nv"], ["hset", "hx", "g1", "1", "g2", "2"],
    ["hsetnx", "hx", "f1", "x"], ["hsetnx", "hx", "fn", "x"], ["hdel", "hx", "f1"],
    ["hincrby", "hx", "cnt", "3"], ["hincrbyfloat", "hx", "cf", "1.5"],
    # zset
    ["zadd", "zx", "5", "m"], ["zadd", "zx", "9", "a"], ["zadd", "zx", "gt", "1", "a"],
    ["zadd", "zx", "ch", "9", "a"], ["zincrby", "zx", "1", "a"], ["zrem", "zx", "a"],
    ["zpopmin", "zx"], ["zpopmax", "zx", "2"], ["zremrangebyrank", "zy", "0", "0"],
    ["zremrangebyscore", "zy", "0", "2"], ["zrangestore", "zd", "zx", "0", "-1"],
    ["zdiffstore", "zd2", "2", "zx", "zy"], ["zinterstore", "zd3", "2", "zx", "zy"],
    ["zunionstore", "zd4", "2", "zx", "zy"], ["zmpop", "1", "zx", "min"],
    # bit
    ["setbit", "bx", "25", "1"], ["bitop", "and", "bd", "bx", "k2"],
    ["bitfield", "bx", "set", "u8", "0", "5"], ["bitfield", "bx", "incrby", "u8", "0", "1"],
    ["bitfield", "bx", "get", "u8", "0"],
    # stream
    ["xadd", "xs", "*", "f", "v"], ["xdel", "xs", "1-1"], ["xtrim", "xs", "maxlen", "0"],
    ["xsetid", "xs", "50-0"], ["xadd", "xs", "nomkstream", "5-5", "f", "v"],
    # hll / geo
    ["pfadd", "hll", "new1"], ["pfadd", "hll", "x"], ["pfmerge", "hd", "hll", "hll2"],
    ["geoadd", "g", "10.0", "20.0", "p3"], ["geoadd", "g", "13.36", "38.11", "p1"],
    # flush
    ["flushdb"],
]

# Reads + no-op writes: dirty delta must be 0.
ZERO = [
    ["get", "k"], ["strlen", "k"], ["mget", "k", "n"], ["exists", "k"], ["ttl", "k"],
    ["type", "k"], ["llen", "lx"], ["lrange", "lx", "0", "-1"], ["scard", "sx"],
    ["smembers", "sx"], ["sismember", "sx", "1"], ["hget", "hx", "f1"], ["hgetall", "hx"],
    ["zscore", "zx", "a"], ["zrange", "zx", "0", "-1"], ["zcard", "zx"], ["getbit", "bx", "20"],
    ["bitcount", "bx"], ["object", "encoding", "k"], ["object", "refcount", "k"],
    ["xlen", "xs"], ["xrange", "xs", "-", "+"], ["pfcount", "hll"], ["geopos", "g", "p1"],
    ["randomkey"], ["dbsize"], ["keys", "*"], ["dump", "k"], ["memory", "usage", "k"],
    ["sintercard", "2", "sx", "sy"], ["touch", "k"], ["scan", "0"], ["hscan", "hx", "0"],
]


def reseed(c):
    c.cmd("select", "0")
    c.cmd("flushall")
    for s in SEED:
        c.cmd(*s)


def delta(port, args):
    c = Conn(port)
    reseed(c)
    b = c.dirty()
    c.cmd(*args)
    a = c.dirty()
    c.s.close()
    return a - b


def main():
    fails = []
    for args in WRITES + ZERO:
        rd = delta(OR, args)
        fr = delta(FRp, args)
        if rd != fr:
            fails.append((args, rd, fr))
    total = len(WRITES) + len(ZERO)
    print("=" * 64)
    if fails:
        for a, rd, fr in fails:
            print(f"DIVERGE {' '.join(a):42s} redis_dirty_delta={rd} fr={fr}")
        print(f"FAIL — {len(fails)}/{total} per-command dirty-accounting divergence(s)")
        return 1
    print(f"PASS — per-command rdb_changes_since_last_save delta byte-exact vs "
          f"redis 7.2.4 ({total} commands: {len(WRITES)} write, {len(ZERO)} read/no-op)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
