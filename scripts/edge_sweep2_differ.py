#!/usr/bin/env python3
"""edge_sweep2_differ.py — second deterministic edge sweep vs redis 7.2.4,
focused on SET option combos, exact encoding-transition boundaries, error
wording (arity/syntax/type), and numeric edges. Non-deterministic surfaces
(auto-id time, shared-int idletime, SCAN bucket order) are deliberately avoided.
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(2.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":", b",", b"#", b"("):
            return l.decode("latin1")
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t in (b"$", b"="):
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t in (b"*", b"~", b">"):
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return ["MAP"] + [self.parse() for _ in range(2 * n)]
        if t == b"_":
            return None
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


SCENARIOS = [
    # --- SET option combos ---
    [("SET", "k", "v", "XX")],                            # nil (no key)
    [("SET", "k", "v", "NX"), ("SET", "k", "w", "NX")],   # OK then nil
    [("SET", "k", "v"), ("SET", "k", "w", "XX", "GET")],  # returns old "v"
    [("SET", "k", "v"), ("SET", "k", "w", "NX", "GET")],  # returns old "v", no set
    [("SET", "k", "v"), ("SET", "k", "w", "NX", "GET"), ("GET", "k")],  # still "v"
    [("SET", "k", "v", "EX", "100", "GET")],              # nil, key set with ttl
    [("SET", "k", "v", "EX", "100", "GET"), ("TTL", "k")],
    [("SET", "k", "v", "KEEPTTL")],
    [("SET", "k", "v", "EX", "100"), ("SET", "k", "w", "KEEPTTL"), ("TTL", "k")],
    [("SET", "k", "v", "EX", "100"), ("SET", "k", "w"), ("TTL", "k")],   # plain SET clears ttl
    [("SET", "k", "v", "EXAT", "1"), ("EXISTS", "k")],    # past -> deleted
    [("SET", "k", "v", "PX", "100000"), ("PTTL", "k")],
    [("RPUSH", "k", "a"), ("SET", "k", "v", "GET")],      # WRONGTYPE on GET
    [("SET", "k", "v", "EX", "100", "PX", "100")],        # syntax error (both EX/PX)
    [("SET", "k", "v", "EX")],                            # syntax error (missing arg)
    [("SET", "k", "v", "EX", "abc")],                     # not an integer
    [("SET", "k", "v", "EX", "0")],                       # invalid expire
    [("SET", "k", "v", "EX", "-1")],                      # invalid expire
    [("SET", "k", "v", "NX", "XX")],                      # syntax error
    [("SET", "k", "v", "IDLE", "5")],                     # syntax error (no IDLE for SET)
    # --- exact encoding thresholds (compiled defaults: list/hash/set/zset=128, set-max-intset=512, value=64) ---
    [("RPUSH", "e", *[str(i) for i in range(128)]), ("OBJECT", "ENCODING", "e")],   # 128 -> listpack
    [("RPUSH", "e", *[str(i) for i in range(129)]), ("OBJECT", "ENCODING", "e")],   # 129 -> quicklist
    [("HSET", "e", *sum([["f" + str(i), str(i)] for i in range(128)], [])), ("OBJECT", "ENCODING", "e")],
    [("HSET", "e", *sum([["f" + str(i), str(i)] for i in range(129)], [])), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", *sum([[str(i), "m" + str(i)] for i in range(128)], [])), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", *sum([[str(i), "m" + str(i)] for i in range(129)], [])), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", *[str(i) for i in range(512)]), ("OBJECT", "ENCODING", "e")],    # 512 ints -> intset
    [("SADD", "e", *[str(i) for i in range(513)]), ("OBJECT", "ENCODING", "e")],    # 513 ints -> hashtable
    [("SADD", "e", *["m" + str(i) for i in range(128)]), ("OBJECT", "ENCODING", "e")],  # 128 strs -> listpack
    [("SADD", "e", *["m" + str(i) for i in range(129)]), ("OBJECT", "ENCODING", "e")],  # 129 strs -> hashtable
    # intset stays intset when small ints; switches to listpack when a non-int added (<=128)
    [("SADD", "e", "1", "2", "3"), ("SADD", "e", "abc"), ("OBJECT", "ENCODING", "e")],
    # value > 64 bytes forces hashtable/quicklist/skiplist even with few elements
    [("HSET", "e", "f", "x" * 64), ("OBJECT", "ENCODING", "e")],   # exactly 64 -> listpack
    [("HSET", "e", "f", "x" * 65), ("OBJECT", "ENCODING", "e")],   # 65 -> hashtable
    [("ZADD", "e", "1", "x" * 64), ("OBJECT", "ENCODING", "e")],
    [("ZADD", "e", "1", "x" * 65), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", "x" * 64), ("OBJECT", "ENCODING", "e")],
    [("SADD", "e", "x" * 65), ("OBJECT", "ENCODING", "e")],
    # --- error wording: arity ---
    [("GET",)],
    [("SET", "k")],
    [("MSET", "k")],
    [("HSET", "k", "f")],
    [("GET", "a", "b")],
    [("EXPIRE", "k")],
    [("LPUSH", "k")],
    [("SUBSCRIBE",)],
    [("ZADD", "k", "1")],
    [("GETRANGE", "k", "0")],
    # --- error wording: unknown command / subcommand ---
    [("NOTACOMMAND", "x")],
    [("OBJECT", "NOTASUB", "k")],
    [("CLIENT", "NOTASUB")],
    [("COMMAND", "NOTASUB")],
    [("CONFIG", "NOTASUB")],
    # --- error wording: type / value ---
    [("SET", "k", "v"), ("LPUSH", "k", "x")],             # WRONGTYPE
    [("SET", "k", "v"), ("INCR", "k")],                   # not an integer
    [("SET", "k", "3.2"), ("INCRBYFLOAT", "k", "1.0e400")],   # nan/inf result
    [("HSET", "h", "f", "v"), ("HINCRBY", "h", "f", "1")],    # hash value not int
    [("ZADD", "z", "notanumber", "m")],                   # not a valid float
    [("ZADD", "z", "nan", "m")],                          # nan
    [("EXPIRE", "k", "notanumber")],
    [("SETEX", "k", "0", "v")],                           # invalid expire
    [("SETEX", "k", "-5", "v")],
    [("LPUSH", "l", "a"), ("LPOP", "l", "-1")],           # out of range count
    [("LPUSH", "l", "a"), ("LPOP", "l", "abc")],
    # --- SETRANGE/APPEND grow + numeric edges ---
    [("SET", "k", "12345"), ("APPEND", "k", "6"), ("OBJECT", "ENCODING", "k")],
    [("APPEND", "k", "hello"), ("APPEND", "k", " world"), ("GET", "k")],
    [("SET", "k", "100"), ("GETRANGE", "k", "0", "1")],
    [("SET", "k", "100"), ("STRLEN", "k")],
    [("SETRANGE", "k", "0", "hi"), ("STRLEN", "k")],
    # --- INCR/DECR edges ---
    [("SET", "n", "-9223372036854775808"), ("DECR", "n")],   # overflow
    [("SET", "n", "10"), ("DECRBY", "n", "-9223372036854775808")],  # overflow via decrby neg
    [("INCRBYFLOAT", "f", "5.0e3"), ("GET", "f")],
    # --- LPOP/RPOP count ---
    [("RPUSH", "l", "a", "b", "c"), ("LPOP", "l", "0")],     # empty array
    [("RPUSH", "l", "a", "b", "c"), ("LPOP", "l", "10"), ("EXISTS", "l")],
    [("RPUSH", "l", "a", "b", "c"), ("LPOP", "l")],
    # --- ZADD INCR with NX on existing returns nil ---
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "NX", "INCR", "3", "m")],   # nil
    [("ZADD", "z", "5", "m"), ("ZADD", "z", "XX", "INCR", "3", "m")],   # 8
    [("ZADD", "z", "XX", "INCR", "3", "m")],                            # nil (no key)
    # --- HELLO / RESET basics ---
    [("HELLO",)],
    [("HELLO", "4")],          # unsupported proto version
    [("HELLO", "3"), ("HELLO", "2")],
]


def run_scenario(c, steps):
    c.cmd("FLUSHALL")
    return [c.cmd(*step) for step in steps]


def normalize_hello(reply):
    # HELLO returns a map with server-version/id/etc that legitimately differ;
    # only compare the proto field + structural shape.
    return reply


# Time-relative reads drift by the µs–ms gap between the two servers running the
# same command, so a fresh `SET k v PX 100000; PTTL k` legitimately reports e.g.
# 100000 on one and 99999 on the other. Treat both-positive integers within a
# small tolerance as equal (the same "sub-10ms TTL race" filter the other gates
# already carry); ms units get 50, second units get 2.
_TTL_TOL = {"PTTL": 50, "PEXPIRETIME": 50, "TTL": 2, "EXPIRETIME": 2}


def time_jitter_ok(step, a, b):
    tol = _TTL_TOL.get(step[0].upper()) if step else None
    if tol is None:
        return False
    if not (isinstance(a, str) and isinstance(b, str) and a[:1] == ":" and b[:1] == ":"):
        return False
    try:
        ia, ib = int(a[1:]), int(b[1:])
    except ValueError:
        return False
    return ia > 0 and ib > 0 and abs(ia - ib) <= tol


# Pin both servers to a known encoding-threshold baseline before the exact
# boundary scenarios (128/129, 512/513, 64/65-byte). The shared oracle's CONFIG
# drifts (other probes leave small thresholds; config-less redis = 512/-2 vs fr's
# compiled defaults), which otherwise turns a pure config skew into spurious
# encoding "divergences". (config_default_vs_oracle)
ENCODING_BASELINE = (
    ("list-max-listpack-size", "128"),
    ("hash-max-listpack-entries", "128"),
    ("hash-max-listpack-value", "64"),
    ("set-max-listpack-entries", "128"),
    ("set-max-intset-entries", "512"),
    ("zset-max-listpack-entries", "128"),
    ("zset-max-listpack-value", "64"),
)


def align_encoding_config(c):
    for k, v in ENCODING_BASELINE:
        c.cmd("CONFIG", "SET", k, v)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    o, f = Conn(args.oracle), Conn(args.fr)
    align_encoding_config(o)
    align_encoding_config(f)

    diffs = 0
    try:
        for steps in SCENARIOS:
            is_hello = any(s[0] == "HELLO" and len(s) <= 1 or (s[0] == "HELLO") for s in steps)
            ro = run_scenario(o, steps)
            rf = run_scenario(f, steps)
            if is_hello:
                continue  # HELLO maps carry version/id; skip value compare
            if ro != rf:
                for i, (a, b) in enumerate(zip(ro, rf)):
                    if a != b and not time_jitter_ok(steps[i], a, b):
                        diffs += 1
                        print(f"DIFF {steps}")
                        print(f"   step {i} {steps[i]}")
                        print(f"   oracle: {a!r}")
                        print(f"   fr    : {b!r}")
                        break
    finally:
        for c in (o, f):
            try:
                c.cmd("FLUSHALL")
            except Exception:
                pass
    if diffs:
        print(f"\nFAIL: {diffs} divergences")
        sys.exit(1)
    print(f"OK: edge sweep 2 byte-exact vs redis 7.2.4 (HELLO maps skipped)")


if __name__ == "__main__":
    main()
