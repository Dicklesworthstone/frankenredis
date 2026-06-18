#!/usr/bin/env python3
"""Differential gate for the borrowed byte-prefix fast-path packets (frankenredis-u7r1s).

fr-server short-circuits the exact canonical RESP byte shapes of many hot commands
(`*N\\r\\n$len\\r\\nCMD\\r\\n...`) with dedicated parse_borrowed_plain_*_packet fast
paths that bypass the generic multibulk parse, then execute via the borrowed
runtime path. This gate sends each such command as canonical RESP (so the
byte-prefix packet fires) and compares the reply byte-for-byte vs vendored redis
7.2.4, regression-locking the whole fast-path surface added across the EXISTS
(2-8) / MGET / MSET / GET / SET / INCR / DECR / STRLEN / LLEN / GETDEL / SCARD /
HLEN / ZCARD / TTL / PTTL / TYPE / EXPIRETIME / PEXPIRETIME / XLEN / OBJECT
ENCODING|REFCOUNT / HGET / SISMEMBER / ZSCORE / HEXISTS / APPEND / GETBIT / LINDEX
families.

Deterministic only: expiring keys use a fixed far-future EXAT and are probed with
the ABSOLUTE EXPIRETIME/PEXPIRETIME (not timing-relative TTL/PTTL); TTL/PTTL are
only probed on no-expiry (-1) and missing (-2) keys.

Usage: packet_fastpath_differ.py <oracle_port> <fr_port>
       Exit 0 = every fast-path reply byte-exact, 1 = divergence.
"""
import socket
import sys
import time

EXAT = "4102444800"  # 2100-01-01, stable absolute expire seconds


def conn(p):
    return socket.create_connection(("127.0.0.1", p), timeout=5)


def enc(args):
    o = b"*%d\r\n" % len(args)
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        o += b"$%d\r\n%s\r\n" % (len(a), a)
    return o


def call(s, *args):
    s.sendall(enc(args))
    time.sleep(0.02)
    return s.recv(1 << 20)


def setup(s):
    call(s, "FLUSHALL")
    call(s, "SET", "sk", "12345")           # int encoding
    call(s, "SET", "str", "hello world")
    call(s, "RPUSH", "lst", "a", "b", "c")
    call(s, "HSET", "h", "f1", "v1", "f2", "v2")
    call(s, "SADD", "s", "a", "b", "c")
    call(s, "SADD", "si", "1", "2", "3")
    call(s, "ZADD", "z", "1.5", "x", "2.5", "y")
    call(s, "SET", "ctr", "10")
    call(s, "SET", "exk", "v", "EXAT", EXAT)
    call(s, "SET", "gd", "bye")
    call(s, "SET", "bk", "\xff")


# Each entry is a command sent as canonical RESP (fires the byte-prefix packet).
# Order matters (mutations); both servers get the identical sequence.
CASES = [
    ["EXISTS", "sk", "str"],
    ["EXISTS", "sk", "nope", "str"],
    ["EXISTS", "sk", "str", "lst", "h"],
    ["EXISTS", "sk", "str", "lst", "h", "z"],
    ["EXISTS", "sk", "str", "lst", "h", "z", "s"],
    ["EXISTS", "sk", "str", "lst", "h", "z", "s", "si"],
    ["EXISTS", "sk", "str", "lst", "h", "z", "s", "si", "ctr"],
    ["MGET", "sk", "str", "nope"],
    ["INCR", "ctr"],
    ["DECR", "ctr"],
    ["INCRBY", "ctr", "7"],
    ["DECRBY", "ctr", "3"],
    ["INCRBY", "ctr", "-100"],
    ["INCRBY", "str", "1"],
    ["STRLEN", "str"],
    ["STRLEN", "nope"],
    ["LLEN", "lst"],
    ["GETDEL", "gd"],
    ["GET", "gd"],
    ["SCARD", "s"],
    ["HLEN", "h"],
    ["ZCARD", "z"],
    ["TTL", "str"],
    ["TTL", "nope"],
    ["PTTL", "str"],
    ["PTTL", "nope"],
    ["TYPE", "lst"],
    ["TYPE", "h"],
    ["TYPE", "z"],
    ["TYPE", "nope"],
    ["EXPIRETIME", "exk"],
    ["EXPIRETIME", "str"],
    ["PEXPIRETIME", "exk"],
    ["XLEN", "nope"],
    ["OBJECT", "ENCODING", "sk"],
    ["OBJECT", "ENCODING", "str"],
    ["OBJECT", "ENCODING", "lst"],
    ["OBJECT", "ENCODING", "si"],
    ["OBJECT", "REFCOUNT", "str"],
    ["HGET", "h", "f1"],
    ["HMGET", "h", "f1", "f2"],
    ["HMGET", "h", "f1", "nope"],
    ["HMGET", "h", "f1", "f2", "nope"],
    ["HMGET", "nope", "f1", "f2"],
    ["HMGET", "str", "f1", "f2"],
    ["HGET", "h", "nope"],
    ["SISMEMBER", "s", "a"],
    ["SISMEMBER", "s", "zzz"],
    ["ZSCORE", "z", "x"],
    ["HSTRLEN", "h", "f1"],
    ["HSTRLEN", "h", "nope"],
    ["HSTRLEN", "str", "f"],
    ["ZRANK", "z", "x"],
    ["ZRANK", "z", "nope"],
    ["ZREVRANK", "z", "x"],
    ["SMISMEMBER", "s", "a", "zz"],
    ["SMISMEMBER", "s", "a", "b", "zz"],
    ["ZMSCORE", "z", "x", "nope"],
    ["ZMSCORE", "z", "x", "y", "nope"],
    ["ZSCORE", "z", "nope"],
    ["HEXISTS", "h", "f1"],
    ["APPEND", "ap", "hi"],
    ["APPEND", "ap", "there"],
    ["GETBIT", "bk", "0"],
    ["GETBIT", "bk", "100"],
    ["LINDEX", "lst", "0"],
    ["LINDEX", "lst", "-1"],
    ["LINDEX", "lst", "99"],
    ["GETRANGE", "str", "0", "-1"],
    ["GETRANGE", "str", "0", "4"],
    ["GETRANGE", "str", "-5", "-1"],
    ["GETRANGE", "str", "100", "200"],
    ["GETRANGE", "nope", "0", "-1"],
    ["GETRANGE", "lst", "0", "-1"],
    ["LRANGE", "lst", "0", "-1"],
    ["LRANGE", "lst", "1", "2"],
    ["LRANGE", "lst", "-2", "-1"],
    ["LRANGE", "lst", "5", "1"],
    ["LRANGE", "nope", "0", "-1"],
    ["LRANGE", "str", "0", "-1"],
    # HGETALL/SMEMBERS: small listpack/intset collections preserve insertion/
    # sorted order, so byte-exact vs redis (large hashtable/set order is a
    # separate dict-order WONTFIX, not exercised here).
    ["HGETALL", "h"],
    ["HKEYS", "h"],
    ["HKEYS", "nope"],
    ["HKEYS", "str"],
    ["HVALS", "h"],
    ["HVALS", "nope"],
    ["HGETALL", "nope"],
    ["HGETALL", "str"],
    ["SMEMBERS", "s"],
    ["SMEMBERS", "si"],
    ["SMEMBERS", "nope"],
    ["SMEMBERS", "str"],
    # wrong-type / error shapes through the fast path
    ["STRLEN", "lst"],
    ["HGET", "str", "f"],
    ["GETBIT", "lst", "0"],
    ["LINDEX", "str", "0"],
    ["INCR", "str"],
    # base/cod packets: PING / SET / HSET / MSET (fresh keys, appended so they
    # don't disturb the read cases above) — regression-locks those fast paths too.
    ["PING"],
    ["PING", "hello there"],
    ["SET", "nk", "nv"],
    ["GET", "nk"],
    ["HSET", "nh", "ff", "vv"],
    ["HGET", "nh", "ff"],
    ["MSET", "ma", "1", "mb", "2"],
    ["MGET", "ma", "mb", "nope"],
    ["DBSIZE"],
    ["COMMAND", "COUNT"],
    ["ECHO", "hello"],
    ["ECHO", ""],
    # writes (fresh keys, appended): SETRANGE/ZINCRBY/EXPIRE reuse verified write
    # executes — propagation/events handled there.
    ["SETRANGE", "srk", "2", "XYZ"],
    ["GET", "srk"],
    ["ZADD", "zk", "5", "m"],
    ["ZINCRBY", "zk", "2.5", "m"],
    ["ZINCRBY", "zk", "1", "newm"],
    ["EXPIRE", "srk", "1000"],
    ["EXPIRE", "nope", "100"],
    # single-key pops (appended last so they don't disturb lst/z reads above)
    ["LPOP", "lst"],
    ["RPOP", "lst"],
    ["LPOP", "nope"],
    ["LPOP", "str"],
    ["ZPOPMIN", "z"],
    ["ZPOPMAX", "z"],
    ["ZPOPMIN", "nope"],
    # single-value writes (fresh keys, appended): LPUSH/RPUSH/SADD reuse verified
    # keyed-values write execute.
    ["LPUSH", "plk", "a"],
    ["RPUSH", "plk", "b"],
    ["LRANGE", "plk", "0", "-1"],
    ["SADD", "psk", "x"],
    ["SADD", "psk", "x"],
    ["SCARD", "psk"],
]


def run_pass(od, fr, proto):
    """Run every CASE once after a fresh deterministic setup; returns failures.
    proto=3 issues HELLO 3 first so RESP3-specific reply types (e.g. ZSCORE
    Double, nil) are exercised through the byte-prefix fast paths."""
    if proto == 3:
        call(od, "HELLO", "3")
        call(fr, "HELLO", "3")
    setup(od)
    setup(fr)
    fails = []
    for args in CASES:
        ro, rf = call(od, *args), call(fr, *args)
        if ro != rf:
            fails.append(f"[RESP{proto}] {' '.join(args)}: redis={ro!r} fr={rf!r}")
    return fails


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = run_pass(od, fr, 2) + run_pass(od, fr, 3)
    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} fast-path packet divergence(s) vs redis 7.2.4:")
        for x in fails:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — all {len(CASES)} byte-prefix fast-path packets byte-exact vs "
        "redis 7.2.4 under both RESP2 and RESP3"
    )


if __name__ == "__main__":
    main()
