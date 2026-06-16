#!/usr/bin/env python3
"""flag_error_edge_gate.py — deterministic flag-conflict / error-ordering /
zset-range / encoding-boundary parity gate vs vendored redis 7.2.4.

Most differ scripts here fuzz random command streams; this one pins a CURATED
battery of the high-value DETERMINISTIC edges that random fuzzers under-sample:
mutually-exclusive option flags (EXPIRE NX/XX/GT/LT, SET EX/PX/NX/XX/KEEPTTL,
GETEX EX/PERSIST, ZADD NX/XX/GT/LT/INCR), argument-validation / type-check
ORDER and error wording, ZSET range commands (BYSCORE/BYLEX/REV/LIMIT, ±inf,
exclusive bounds, hex-float), and OBJECT ENCODING transitions probed EXACTLY at
the compiled-in thresholds (hash 512 entries / 64-byte value, zset 128 / 64,
set intset 512 / listpack 128, list 8KB). Every reply is compared byte-for-byte.

ORACLE-POLLUTION GUARD (the reason this file is self-defending):
A redis-server left running by a prior probe may carry CONFIG SETs that silently
change the encoding thresholds — e.g. a peer setting hash-max-listpack-entries to
128 makes a correct fr (compiled default 512) look broken. Before running ANY
case, this gate asserts the oracle still reports the true redis 7.2.4 compiled
defaults; if not, it ABORTS with a clear "polluted oracle" message instead of
emitting false divergences. Launch the oracle config-less:
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes

Usage: flag_error_edge_gate.py <oracle_port> <fr_port>
Exit 0 if every case matches, 2 if the oracle is polluted, 1 on a real diff.
"""
import socket
import sys
import time

# True redis 7.2.4 COMPILED-IN defaults (config.c create*Config), i.e. what a
# config-less server reports. Used as the pollution preflight.
TRUE_DEFAULTS = {
    "hash-max-listpack-entries": b"512",
    "hash-max-listpack-value": b"64",
    "list-max-listpack-size": b"-2",
    "set-max-intset-entries": b"512",
    "set-max-listpack-entries": b"128",
    "set-max-listpack-value": b"64",
    "zset-max-listpack-entries": b"128",
    "zset-max-listpack-value": b"64",
}


def cli(p):
    return socket.create_connection(("127.0.0.1", p), timeout=3)


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        if isinstance(x, str):
            x = x.encode()
        elif isinstance(x, int):
            x = str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.012)
    return s.recv(262144)


def config_value(s, name):
    r = cmd(s, "CONFIG", "GET", name)
    # *2\r\n$<>\r\nNAME\r\n$<>\r\nVALUE\r\n -> last bulk is the value
    parts = r.split(b"\r\n")
    return parts[-2] if len(parts) >= 2 else b""


def assert_oracle_clean(oport):
    s = cli(oport)
    bad = []
    for name, want in TRUE_DEFAULTS.items():
        got = config_value(s, name)
        if got != want:
            bad.append((name, want, got))
    s.close()
    if bad:
        print("ABORT: oracle on port %d is POLLUTED (not config-less redis 7.2.4 defaults):" % oport)
        for name, want, got in bad:
            print("   %-30s want=%s got=%s" % (name, want.decode(), got.decode()))
        print("Start a FRESH config-less oracle and retry. (a stray CONFIG SET from")
        print("another probe changed these encoding thresholds; comparisons would be bogus)")
        sys.exit(2)


CASES = []


def add(label, setup, probe):
    CASES.append((label, setup, probe))


def field_pairs(n):
    return sum([[f"f{i}", f"v{i}"] for i in range(n)], [])


def zset_pairs(n):
    return sum([[str(i), f"m{i}"] for i in range(n)], [])


# ── flag conflicts / error ordering ──────────────────────────────────────────
add("expire NX no-ttl", [("SET", "k", "v")], ("EXPIRE", "k", "100", "NX"))
add("expire XX no-ttl", [("SET", "k", "v")], ("EXPIRE", "k", "100", "XX"))
add("expire GT no-ttl", [("SET", "k", "v")], ("EXPIRE", "k", "100", "GT"))
add("expire LT no-ttl", [("SET", "k", "v")], ("EXPIRE", "k", "100", "LT"))
add("expire NX XX conflict", [("SET", "k", "v")], ("EXPIRE", "k", "100", "NX", "XX"))
add("expire GT LT conflict", [("SET", "k", "v")], ("EXPIRE", "k", "100", "GT", "LT"))
add("expire NX GT conflict", [("SET", "k", "v")], ("EXPIRE", "k", "100", "NX", "GT"))
add("expire bad flag", [("SET", "k", "v")], ("EXPIRE", "k", "100", "ZZ"))
add("expire overflow", [("SET", "k", "v")], ("EXPIRE", "k", "9999999999999999"))
add("pexpire overflow", [("SET", "k", "v")], ("PEXPIRE", "k", "99999999999999999999"))
add("expire GT bigger", [("SET", "k", "v"), ("EXPIRE", "k", "50")], ("EXPIRE", "k", "100", "GT"))
add("expire LT bigger noop", [("SET", "k", "v"), ("EXPIRE", "k", "50")], ("EXPIRE", "k", "100", "LT"))
add("getex EX PERSIST conflict", [("SET", "k", "v")], ("GETEX", "k", "EX", "100", "PERSIST"))
add("getex EX PX conflict", [("SET", "k", "v")], ("GETEX", "k", "EX", "100", "PX", "100"))
add("getex bad opt", [("SET", "k", "v")], ("GETEX", "k", "FOO"))
add("getex EX 0", [("SET", "k", "v")], ("GETEX", "k", "EX", "0"))
add("getex EX negative", [("SET", "k", "v")], ("GETEX", "k", "EX", "-5"))
add("set EX PX conflict", [], ("SET", "k", "v", "EX", "10", "PX", "100"))
add("set NX XX conflict", [], ("SET", "k", "v", "NX", "XX"))
add("set KEEPTTL EX conflict", [], ("SET", "k", "v", "KEEPTTL", "EX", "10"))
add("set EX 0", [], ("SET", "k", "v", "EX", "0"))
add("set EX notanum", [], ("SET", "k", "v", "EX", "abc"))
add("set GET wrongtype", [("RPUSH", "k", "x")], ("SET", "k", "v", "GET"))
add("set EXAT PXAT conflict", [], ("SET", "k", "v", "EXAT", "100", "PXAT", "100"))
add("zadd GT LT conflict", [], ("ZADD", "z", "GT", "LT", "1", "a"))
add("zadd NX XX conflict", [], ("ZADD", "z", "NX", "XX", "1", "a"))
add("zadd NX GT conflict", [], ("ZADD", "z", "NX", "GT", "1", "a"))
add("zadd INCR multi", [], ("ZADD", "z", "INCR", "1", "a", "2", "b"))
add("zadd nan", [], ("ZADD", "z", "nan", "a"))
add("zadd bad score", [], ("ZADD", "z", "notanum", "a"))
add("incr non-int", [("SET", "k", "abc")], ("INCR", "k"))
add("incrby overflow", [("SET", "k", "9223372036854775807")], ("INCRBY", "k", "1"))
add("incrbyfloat nan", [("SET", "k", "1")], ("INCRBYFLOAT", "k", "nan"))
add("incrbyfloat inf", [("SET", "k", "1")], ("INCRBYFLOAT", "k", "inf"))
add("lpos rank0", [("RPUSH", "l", "a", "b", "a")], ("LPOS", "l", "a", "RANK", "0"))
add("lpos count neg", [("RPUSH", "l", "a")], ("LPOS", "l", "a", "COUNT", "-1"))
add("linsert bad where", [("RPUSH", "l", "a")], ("LINSERT", "l", "SIDEWAYS", "a", "x"))
add("sintercard limit neg", [("SADD", "s", "a", "b")], ("SINTERCARD", "1", "s", "LIMIT", "-1"))
add("getrange start>end", [("SET", "k", "Hello")], ("GETRANGE", "k", "3", "1"))
add("setrange neg offset", [("SET", "k", "Hi")], ("SETRANGE", "k", "-1", "X"))
add("bitcount bit-range", [("SET", "k", "foobar")], ("BITCOUNT", "k", "5", "30", "BIT"))
add("bitpos bit unit", [("SET", "k", "\xff\xf0\x00")], ("BITPOS", "k", "0", "2", "-1", "BIT"))
add("bitfield overflow fail", [], ("BITFIELD", "bf", "OVERFLOW", "FAIL", "SET", "u8", "0", "200", "INCRBY", "u8", "0", "100"))
add("bitfield bad type", [], ("BITFIELD", "bf", "SET", "x8", "0", "1"))
add("bitfield_ro rejects set", [], ("BITFIELD_RO", "bf", "SET", "u8", "0", "1"))
add("lcs len idx conflict", [("SET", "k1", "a"), ("SET", "k2", "b")], ("LCS", "k1", "k2", "LEN", "IDX"))

# ── zset range surface ───────────────────────────────────────────────────────
ZADD5 = [("ZADD", "z", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e")]
add("zrbs exclusive", ZADD5, ("ZRANGEBYSCORE", "z", "(1", "(4"))
add("zrbs inf withscores", ZADD5, ("ZRANGEBYSCORE", "z", "-inf", "+inf", "WITHSCORES"))
add("zrbs limit", ZADD5, ("ZRANGEBYSCORE", "z", "-inf", "+inf", "LIMIT", "1", "2"))
add("zrevrangebyscore swapped", ZADD5, ("ZREVRANGEBYSCORE", "z", "(4", "(1"))
add("zrbs hexfloat", ZADD5, ("ZRANGEBYSCORE", "z", "0x1p0", "0x1.8p1"))
add("zrange byscore rev", ZADD5, ("ZRANGE", "z", "3", "1", "BYSCORE", "REV"))
add("zrange limit no byscore err", ZADD5, ("ZRANGE", "z", "0", "-1", "LIMIT", "0", "2"))
add("zrangestore empty deletes", ZADD5, ("ZRANGESTORE", "dst", "z", "5", "2"))
add("zadd inf score", [("ZADD", "z", "inf", "a")], ("ZSCORE", "z", "a"))
add("zincrby to nan", [("ZADD", "z", "inf", "a")], ("ZINCRBY", "z", "-inf", "a"))

# ── encoding boundaries at compiled thresholds ───────────────────────────────
add("hash 512 listpack", [("HSET", "h", *field_pairs(512))], ("OBJECT", "ENCODING", "h"))
add("hash 513 hashtable", [("HSET", "h", *field_pairs(513))], ("OBJECT", "ENCODING", "h"))
add("hash value 64 listpack", [("HSET", "h", "f", "x" * 64)], ("OBJECT", "ENCODING", "h"))
add("hash value 65 hashtable", [("HSET", "h", "f", "x" * 65)], ("OBJECT", "ENCODING", "h"))
add("hash no downgrade", [("HSET", "h", *field_pairs(513)), ("HDEL", "h", *[f"f{i}" for i in range(500)])], ("OBJECT", "ENCODING", "h"))
add("zset 128 listpack", [("ZADD", "z", *zset_pairs(128))], ("OBJECT", "ENCODING", "z"))
add("zset 129 skiplist", [("ZADD", "z", *zset_pairs(129))], ("OBJECT", "ENCODING", "z"))
add("zset value 65 skiplist", [("ZADD", "z", "1", "x" * 65)], ("OBJECT", "ENCODING", "z"))
add("set intset 512", [("SADD", "s", *[str(i) for i in range(512)])], ("OBJECT", "ENCODING", "s"))
add("set intset 513 hashtable", [("SADD", "s", *[str(i) for i in range(513)])], ("OBJECT", "ENCODING", "s"))
add("set listpack 128 strs", [("SADD", "s", *[f"m{i}" for i in range(128)])], ("OBJECT", "ENCODING", "s"))
add("set listpack 129 strs hashtable", [("SADD", "s", *[f"m{i}" for i in range(129)])], ("OBJECT", "ENCODING", "s"))
add("set str value 65 hashtable", [("SADD", "s", "x" * 65)], ("OBJECT", "ENCODING", "s"))
add("list big value quicklist", [("RPUSH", "l", "x" * 9000)], ("OBJECT", "ENCODING", "l"))
add("list over 8KB quicklist", [("RPUSH", "l", *["x" * 100 for _ in range(100)])], ("OBJECT", "ENCODING", "l"))
add("list small listpack", [("RPUSH", "l", *[f"i{i}" for i in range(128)])], ("OBJECT", "ENCODING", "l"))
add("str int encoding", [("SET", "k", "12345")], ("OBJECT", "ENCODING", "k"))
add("str embstr", [("SET", "k", "short")], ("OBJECT", "ENCODING", "k"))
add("str raw long", [("SET", "k", "x" * 100)], ("OBJECT", "ENCODING", "k"))
add("append to int makes raw", [("SET", "k", "100"), ("APPEND", "k", "x")], ("OBJECT", "ENCODING", "k"))


def run(port):
    s = cli(port)
    out = []
    for label, setup, probe in CASES:
        cmd(s, "FLUSHALL")
        for c in setup:
            cmd(s, *c)
        out.append((label, cmd(s, *probe)))
    s.close()
    return out


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    oport, fport = int(sys.argv[1]), int(sys.argv[2])
    assert_oracle_clean(oport)
    ro = run(oport)
    rf = run(fport)
    fails = 0
    for (lbl, a), (_, b) in zip(ro, rf):
        if a != b:
            fails += 1
            print("FAIL | %s" % lbl)
            print("     oracle=%r" % a)
            print("     fr    =%r" % b)
    n = len(CASES)
    if fails:
        print("\n%d/%d match  <-- %d DIVERGENCE(S)" % (n - fails, n, fails))
        sys.exit(1)
    print("OK: %d/%d flag/error/zset-range/encoding-boundary cases byte-exact vs redis 7.2.4" % (n, n))
    sys.exit(0)


if __name__ == "__main__":
    main()
