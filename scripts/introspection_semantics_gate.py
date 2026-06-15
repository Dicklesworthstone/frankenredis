#!/usr/bin/env python3
"""Introspection + semantics differential gate: fr vs redis 7.2.4 oracle.

Consolidates byte-exact surfaces verified 2026-06-15 (BlackThrush) after the
DEBUG DIGEST type / DEBUG OBJECT quicklist / ZRANGEBYSCORE hex-float fixes:
  - error-message wording (arity / syntax / range / type / flag errors)
  - OBJECT ENCODING transitions across the listpack<->quicklist/hashtable/
    skiplist/intset boundaries
  - DEBUG subcommands (SET-ACTIVE-EXPIRE / STRINGMATCH-LEN / JMAP /
    QUICKLIST-PACKED-THRESHOLD / LISTPACK-ENTRIES / RELOAD NOSAVE / ...)
  - DEBUG OBJECT quicklist ql_* fields
  - stateful multi-connection blocking + WATCH/MULTI/EXEC semantics

Both servers must run with `--enable-debug-command yes`. Launch the redis
oracle from a clean cwd (it loads dump.rdb / appendonly files from cwd).

Usage: introspection_semantics_gate.py <oracle_port> <fr_port>
"""
import socket, sys, time, re

ORACLE = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FR = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def enc(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a if isinstance(a, bytes) else str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def conn(port):
    s = socket.create_connection(("127.0.0.1", port))
    s.settimeout(2)
    return s


def cmd(s, args):
    s.sendall(enc(args))
    time.sleep(0.012)
    try:
        d = b""
        while True:
            c = s.recv(16000)
            if not c:
                break
            d += c
            if len(c) < 16000:
                break
        return d
    except socket.timeout:
        return b"<TIMEOUT>"


def q(port, args):
    s = conn(port)
    r = cmd(s, args)
    s.close()
    return r


def norm_debugobj(b):
    s = b.decode("latin1")
    s = re.sub(r"at:0x[0-9a-f]+", "at:0xP", s)
    s = re.sub(r"lru:\d+", "lru:N", s)
    s = re.sub(r"lru_seconds_idle:\d+", "lru_seconds_idle:N", s)
    return s.strip()


ERROR_CASES = [
    ["GET"], ["GET", "a", "b"], ["SET", "k"], ["MSET", "k"], ["MSET", "k", "v", "k2"],
    ["EXPIRE", "s", "notanint"], ["EXPIRE", "s", "1", "BADFLAG"], ["SETEX", "k", "0", "v"],
    ["SETRANGE", "s", "-1", "v"], ["SETBIT", "s", "-1", "0"], ["SETBIT", "s", "1", "2"],
    ["INCR", "s"], ["INCRBYFLOAT", "s", "x"], ["LSET", "nokey", "0", "x"],
    ["LINSERT", "l", "BAD", "a", "b"], ["LPOP", "l", "x"],
    ["ZADD", "z", "NX", "XX", "1", "m"], ["ZADD", "z", "GT", "NX", "1", "m"],
    ["ZRANGEBYSCORE", "z", "x", "y"], ["SINTERCARD", "0", "s"], ["SINTERCARD", "x", "s"],
    ["LMPOP", "2", "l", "BADDIR"], ["GETEX", "s", "PERSIST", "EX", "5"],
    ["COPY", "s", "d", "DB", "x"], ["OBJECT", "BADSUB", "k"], ["OBJECT", "ENCODING"],
    ["BITCOUNT", "s", "0", "0", "BADUNIT"], ["BITFIELD", "s", "GET", "u99", "0"],
    ["SET", "k", "v", "EX", "0"], ["SET", "k", "v", "BADOPT"], ["LPOS", "l", "a", "RANK", "0"],
]

ENCODING_SEQS = [
    [("SET", "i", "12345"), ("APPEND", "i", "x")],
    [("SET", "i2", "12345"), ("SETRANGE", "i2", "0", "9")],
    [("SET", "big", "x" * 44)], [("SET", "big2", "x" * 45)],
    [("RPUSH", "ll", *["a"] * 128)], [("RPUSH", "ll3", *["a"] * 129)],
    [("SADD", "si", *[str(i) for i in range(513)])],
    [("SADD", "smix", "1", "2", "notint")], [("SADD", "sbig", "1", "x" * 65)],
    [("HSET", "hl2", *sum([[f"f{i}", str(i)] for i in range(129)], []))],
    [("ZADD", "zl2", *sum([[str(i), f"m{i}"] for i in range(129)], []))],
]


def check_errors(failures):
    for p in (ORACLE, FR):
        q(p, ["FLUSHALL"]); q(p, ["SET", "s", "v"]); q(p, ["RPUSH", "l", "a"]); q(p, ["ZADD", "z", "1", "m"])
    for a in ERROR_CASES:
        r, f = q(ORACLE, a), q(FR, a)
        if r != f:
            failures.append(("error", a, r, f))


def check_encodings(failures):
    for seq in ENCODING_SEQS:
        k = seq[-1][1]
        for p in (ORACLE, FR):
            q(p, ["DEL", k])
            for op in seq:
                q(p, list(op))
        r, f = q(ORACLE, ["OBJECT", "ENCODING", k]), q(FR, ["OBJECT", "ENCODING", k])
        if r != f:
            failures.append(("encoding", k, r, f))


def check_debug(failures):
    for p in (ORACLE, FR):
        q(p, ["FLUSHALL"]); q(p, ["RPUSH", "ql", *["x" * 100] * 200])
    subs = [
        ["DEBUG", "SET-ACTIVE-EXPIRE", "0"], ["DEBUG", "SET-ACTIVE-EXPIRE", "1"],
        ["DEBUG", "STRINGMATCH-LEN", "a*", "aaa"], ["DEBUG", "JMAP"],
        ["DEBUG", "QUICKLIST-PACKED-THRESHOLD", "1K"], ["DEBUG", "QUICKLIST-PACKED-THRESHOLD", "0"],
        ["DEBUG", "LISTPACK-ENTRIES"], ["DEBUG", "OBJECT", "nokey"], ["DEBUG", "SLEEP", "0"],
    ]
    for a in subs:
        r, f = q(ORACLE, a), q(FR, a)
        if r != f:
            failures.append(("debug", a, r, f))
    r = norm_debugobj(q(ORACLE, ["DEBUG", "OBJECT", "ql"]))
    f = norm_debugobj(q(FR, ["DEBUG", "OBJECT", "ql"]))
    rq = " ".join(x for x in r.split() if x.startswith("ql_") or x.startswith("encoding"))
    fq = " ".join(x for x in f.split() if x.startswith("ql_") or x.startswith("encoding"))
    if rq != fq:
        failures.append(("debug_object_ql", "ql", rq.encode(), fq.encode()))


def check_stateful(failures):
    def blpop_wake(p):
        c1, c2 = conn(p), conn(p)
        cmd(c1, ["FLUSHALL"])
        c1.sendall(enc(["BLPOP", "bk", "2"])); time.sleep(0.1)
        r2 = cmd(c2, ["RPUSH", "bk", "hello"]); time.sleep(0.1)
        try:
            r1 = c1.recv(8000)
        except socket.timeout:
            r1 = b"<TIMEOUT>"
        c1.close(); c2.close()
        return (r1, r2)

    def watch_abort(p):
        c1, c2 = conn(p), conn(p)
        cmd(c1, ["FLUSHALL"]); cmd(c1, ["SET", "wk", "1"])
        cmd(c1, ["WATCH", "wk"]); cmd(c1, ["MULTI"]); cmd(c1, ["INCR", "wk"])
        cmd(c2, ["SET", "wk", "99"])
        r = cmd(c1, ["EXEC"])
        c1.close(); c2.close()
        return (r,)

    def blpop_wrongtype(p):
        c1, c2 = conn(p), conn(p)
        cmd(c1, ["FLUSHALL"])
        c1.sendall(enc(["BLPOP", "wtk", "1"])); time.sleep(0.1)
        cmd(c2, ["SET", "wtk", "notalist"]); time.sleep(0.1)
        try:
            r1 = c1.recv(8000)
        except socket.timeout:
            r1 = b"<TIMEOUT>"
        c1.close(); c2.close()
        return (r1,)

    for name, sc in [("blpop_wake", blpop_wake), ("watch_abort", watch_abort),
                     ("blpop_wrongtype", blpop_wrongtype)]:
        r, f = sc(ORACLE), sc(FR)
        if r != f:
            failures.append(("stateful_" + name, name, str(r).encode(), str(f).encode()))


def main():
    failures = []
    check_errors(failures)
    check_encodings(failures)
    check_debug(failures)
    check_stateful(failures)
    print("=" * 60)
    if failures:
        print(f"FAIL — {len(failures)} divergence(s) vs redis 7.2.4:")
        for kind, key, r, f in failures[:40]:
            print(f"  [{kind}] {key}\n    redis={r[:100]!r}\n    fr   ={f[:100]!r}")
        sys.exit(1)
    print("PASS — introspection + semantics byte-exact vs redis 7.2.4")
    print("  (errors, encoding transitions, DEBUG subcommands + OBJECT ql_*,"
          " stateful blocking/WATCH)")


main()
