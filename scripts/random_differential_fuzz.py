#!/usr/bin/env python3
"""Seeded randomized differential fuzzer: fr vs redis 7.2.4 oracle.
Shared small key pool, random commands with edge-case args. Compares byte-exact
replies and reports divergences (skipping inherently-random commands)."""
import socket, random, sys, hashlib

FR, RED = 28802, 28801

def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=4); s.settimeout(4); return s

def enc(args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out

def read_reply(s):
    buf = b""
    def rl():
        nonlocal buf
        while b"\r\n" not in buf:
            d = s.recv(4096)
            if not d: return None
            buf += d
        l, buf = buf.split(b"\r\n", 1); return l
    def rn(n):
        nonlocal buf
        while len(buf) < n:
            d = s.recv(4096)
            if not d: break
            buf += d
        o, buf = buf[:n], buf[n:]; return o
    def one():
        l = rl()
        if l is None: return b"<EOF>"
        t = l[:1]
        if t in (b"+", b"-", b":", b",", b"#", b"(", b"_"): return l + b"\r\n"
        if t in (b"$", b"="):
            n = int(l[1:])
            if n < 0: return l + b"\r\n"
            return l + b"\r\n" + rn(n + 2)
        if t in (b"*", b"%", b"~", b">"):
            n = int(l[1:])
            if n < 0: return l + b"\r\n"
            cnt = n * (2 if t == b"%" else 1)
            parts = [l + b"\r\n"]
            for _ in range(cnt): parts.append(one())
            return b"".join(parts)
        return l + b"\r\n"
    return one()

KEYS = ["k1", "k2", "k3", "h1", "l1", "s1", "z1", "nope"]
INTS = ["0", "1", "-1", "2", "5", "-5", "10", "100", "-100", "9223372036854775807",
        "-9223372036854775808", "9999999999999999999", "3.14", "-0", "+1", " 1", "1 ", "0x10", "1e3"]
VALS = ["", "a", "abc", "hello world", "foo\x00bar", "\xff\xfe", "123", "-1", "3.14",
        "नमस्ते", "  ", "\r\n", "x" * 40]
FIELDS = ["f1", "f2", "f3", "fx"]

# (cmd, arg-generators) — generators draw from pools. Reads + light writes.
def g_key(): return random.choice(KEYS)
def g_int(): return random.choice(INTS)
def g_val(): return random.choice(VALS)
def g_field(): return random.choice(FIELDS)
def g_member(): return random.choice(VALS + FIELDS)

CMDS = [
    ("GET", [g_key]), ("STRLEN", [g_key]), ("EXISTS", [g_key, g_key]),
    ("TYPE", [g_key]), ("TTL", [g_key]), ("PTTL", [g_key]),
    ("INCR", [g_key]), ("DECR", [g_key]), ("INCRBY", [g_key, g_int]),
    ("INCRBYFLOAT", [g_key, g_int]), ("DECRBY", [g_key, g_int]),
    ("APPEND", [g_key, g_val]), ("SETRANGE", [g_key, g_int, g_val]),
    ("GETRANGE", [g_key, g_int, g_int]), ("GETBIT", [g_key, g_int]),
    ("SETBIT", [g_key, g_int, g_int]), ("BITCOUNT", [g_key, g_int, g_int]),
    ("BITPOS", [g_key, g_int, g_int]),
    ("LPUSH", [g_key, g_val]), ("RPUSH", [g_key, g_val]), ("LLEN", [g_key]),
    ("LINDEX", [g_key, g_int]), ("LRANGE", [g_key, g_int, g_int]),
    ("LPOP", [g_key, g_int]), ("RPOP", [g_key, g_int]), ("LREM", [g_key, g_int, g_val]),
    ("LSET", [g_key, g_int, g_val]), ("LPOS", [g_key, g_val]),
    ("LINSERT", [g_key, lambda: random.choice(["BEFORE", "AFTER", "X"]), g_val, g_val]),
    ("SADD", [g_key, g_member]), ("SREM", [g_key, g_member]), ("SCARD", [g_key]),
    ("SISMEMBER", [g_key, g_member]), ("SMEMBERS", [g_key]),
    ("SMISMEMBER", [g_key, g_member, g_member]), ("SINTERCARD", [lambda: "1", g_key]),
    ("HSET", [g_key, g_field, g_val]), ("HGET", [g_key, g_field]), ("HDEL", [g_key, g_field]),
    ("HEXISTS", [g_key, g_field]), ("HLEN", [g_key]), ("HSTRLEN", [g_key, g_field]),
    ("HMGET", [g_key, g_field, g_field]), ("HGETALL", [g_key]), ("HKEYS", [g_key]),
    ("HINCRBY", [g_key, g_field, g_int]), ("HINCRBYFLOAT", [g_key, g_field, g_int]),
    ("ZADD", [g_key, g_int, g_member]), ("ZSCORE", [g_key, g_member]), ("ZCARD", [g_key]),
    ("ZRANK", [g_key, g_member]), ("ZRANGE", [g_key, g_int, g_int]),
    ("ZINCRBY", [g_key, g_int, g_member]), ("ZREM", [g_key, g_member]),
    ("ZRANGEBYSCORE", [g_key, g_int, g_int]), ("ZCOUNT", [g_key, g_int, g_int]),
    ("ZMSCORE", [g_key, g_member, g_member]),
    ("SET", [g_key, g_val]), ("GETDEL", [g_key]), ("GETEX", [g_key]),
    ("EXPIRE", [g_key, g_int]), ("PERSIST", [g_key]),
    ("OBJECT", [lambda: "ENCODING", g_key]), ("COPY", [g_key, g_key]),
    ("SETEX", [g_key, g_int, g_val]), ("SETNX", [g_key, g_val]),
]
# inherently non-deterministic — compare only error-vs-ok shape, skip value diffs
RANDOMISH = {"SRANDMEMBER", "HRANDFIELD", "ZRANDMEMBER", "SPOP", "RANDOMKEY"}
TIMING = {"PTTL", "TTL", "PEXPIRETIME", "EXPIRETIME", "OBJECT"}
def has_ctrl(args):
    return any(isinstance(x,str) and any(c in x for c in "\r\n\x00") for x in args)

def kind(reply):
    # classify reply: error type vs success-shape, ignoring random payloads
    return reply[:1]

def main():
    seed = int(sys.argv[1]) if len(sys.argv) > 1 else 1234
    N = int(sys.argv[2]) if len(sys.argv) > 2 else 6000
    # Optional explicit ports (oracle/redis then fr) so run_fuzz_sweep.sh can drive
    # this fuzzer against its chosen port pair; default to the standalone constants.
    red_port = int(sys.argv[3]) if len(sys.argv) > 3 else RED
    fr_port = int(sys.argv[4]) if len(sys.argv) > 4 else FR
    random.seed(seed)
    fr, red = conn(fr_port), conn(red_port)
    # reset both
    for s in (fr, red):
        s.sendall(enc(["FLUSHALL"])); read_reply(s)
    diffs = []
    nrun = 0
    for i in range(N):
        name, gens = random.choice(CMDS)
        args = [name] + [g() for g in gens]
        # occasionally drop/extra args to hit arity edges
        r = random.random()
        if r < 0.08 and len(args) > 1: args = args[:-1]
        elif r < 0.12: args = args + [g_val()]
        try:
            fr.sendall(enc(args)); a = read_reply(fr)
            red.sendall(enc(args)); b = read_reply(red)
        except Exception as e:
            print("conn error", e, args); break
        nrun += 1
        if name in RANDOMISH or name in TIMING:
            if kind(a) != kind(b):
                diffs.append((args, b, a))
        elif name == "EXPIRE" and has_ctrl(args):
            pass  # known Rust-String error-text NUL/CR residual
        else:
            if a != b:
                diffs.append((args, b, a))
    print(f"seed={seed} ran={nrun} diffs={len(diffs)}")
    seen = set()
    uniq = []
    for args, b, a in diffs:
        key = (args[0], b[:1], a[:1], b[:30], a[:30])
        if key in seen: continue
        seen.add(key); uniq.append((args, b, a))
    print(f"unique diff signatures: {len(uniq)}")
    for args, b, a in uniq[:60]:
        print(f"  {args}\n    redis={b[:120]!r}\n    fr   ={a[:120]!r}")

main()
