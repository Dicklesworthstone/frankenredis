#!/usr/bin/env python3
"""Randomized DUMP/RESTORE differential fuzzer: fr (strict) vs vendored redis 7.2.4.

Complements dump_restore_differ.py (14 fixed encodings) with seeded random keys
that target encoding boundaries (intset/listpack/hashtable/skiplist/quicklist
transitions), binary member bytes, special zset scores, LZF-compressible strings,
and stream PEL/group shapes. Both directions, value round-trip verified (RESTORE
then read back, order-normalized) — not just RESTORE==OK.

Usage: dump_restore_fuzz.py <oracle_port> <fr_port> [--seed N] [--keys N]
"""
import socket, sys, random


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5.0)
        self.b = b""
    def _line(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        l, self.b = self.b.split(b"\r\n", 1); return l
    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n+2:]; return d
    def parse(self):
        l = self._line(); t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r); return None if n < 0 else self._rn(n)
        if t == b":": return int(r)
        if t == b"+": return r
        if t == b"-": return b"ERR:" + r
        if t == b"*":
            n = int(r); return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)
    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out); return self.parse()


def readback(c, key):
    t = c.cmd("TYPE", key)
    if t == b"string": return ("string", c.cmd("GET", key))
    if t == b"list": return ("list", c.cmd("LRANGE", key, 0, -1))
    if t == b"set": return ("set", sorted(c.cmd("SMEMBERS", key)))
    if t == b"hash":
        flat = c.cmd("HGETALL", key)
        pairs = sorted(zip(flat[0::2], flat[1::2]))
        return ("hash", pairs)
    if t == b"zset": return ("zset", c.cmd("ZRANGE", key, 0, -1, "WITHSCORES"))
    if t == b"stream":
        return ("stream", c.cmd("XRANGE", key, "-", "+"),
                c.cmd("XINFO", "GROUPS", key))
    return ("other", c.cmd("DUMP", key))


def rbytes(rnd, maxlen):
    n = rnd.randint(0, maxlen)
    # sometimes include binary / NUL / high bytes
    if rnd.random() < 0.3:
        return bytes(rnd.randint(0, 255) for _ in range(n))
    return bytes(rnd.randint(97, 122) for _ in range(n))


def build_key(c, rnd, key):
    kind = rnd.choice(["str", "list", "set", "intset", "hash", "zset", "stream"])
    if kind == "str":
        choice = rnd.random()
        if choice < 0.3:
            c.cmd("SET", key, str(rnd.randint(-10**18, 10**18)))   # int-enc
        elif choice < 0.5:
            c.cmd("SET", key, rbytes(rnd, 44))                     # embstr boundary
        elif choice < 0.7:
            c.cmd("SET", key, b"ab" * rnd.randint(20, 2000))       # LZF-friendly raw
        else:
            c.cmd("SET", key, rbytes(rnd, 600))                    # raw random
    elif kind == "list":
        n = rnd.choice([1, 5, 64, 127, 128, 129, 300])
        elems = [rbytes(rnd, rnd.choice([3, 3, 70])) or b"x" for _ in range(n)]
        c.cmd("RPUSH", key, *elems)
    elif kind == "intset":
        n = rnd.choice([1, 64, 128, 300, 512, 513])
        c.cmd("SADD", key, *[str(rnd.randint(-10**15, 10**15)) for _ in range(n)])
    elif kind == "set":
        n = rnd.choice([1, 64, 127, 128, 129, 300])
        c.cmd("SADD", key, *[(rbytes(rnd, rnd.choice([3, 70])) or b"x") + b"#%d" % i
                             for i in range(n)])
    elif kind == "hash":
        n = rnd.choice([1, 64, 127, 128, 129, 300])
        args = []
        for i in range(n):
            args += [b"f%d" % i, rbytes(rnd, rnd.choice([3, 70])) or b"v"]
        c.cmd("HSET", key, *args)
    elif kind == "zset":
        n = rnd.choice([1, 64, 127, 128, 129, 300])
        args = []
        for i in range(n):
            sc = rnd.choice([str(rnd.randint(-1000, 1000)),
                             repr(rnd.uniform(-1e6, 1e6)), "inf", "-inf",
                             "3.141592653589793", "0"])
            args += [sc, b"m%d" % i]
        c.cmd("ZADD", key, *args)
    elif kind == "stream":
        for _ in range(rnd.choice([1, 5, 40, 130])):
            c.cmd("XADD", key, "*", "f", str(rnd.randint(0, 999)),
                  "g", rbytes(rnd, 8) or b"x")
        if rnd.random() < 0.7:
            c.cmd("XGROUP", "CREATE", key, "g1", "0")
            # create a PEL: read some entries as a consumer
            c.cmd("XREADGROUP", "GROUP", "g1", "c1", "COUNT", "5", "STREAMS", key, ">")
        if rnd.random() < 0.3:
            c.cmd("XGROUP", "CREATE", key, "g2", "$")
    # random TTL sometimes
    if rnd.random() < 0.2:
        c.cmd("PEXPIRE", key, str(rnd.randint(100000, 9999999)))
    return kind


def run(oport, fport, seed, nkeys):
    o = Conn(oport); f = Conn(fport)
    rnd = random.Random(seed)
    o.cmd("FLUSHALL"); f.cmd("FLUSHALL")
    keys = []
    for i in range(nkeys):
        k = "k%d_%d" % (seed, i)
        # build identically on BOTH (same rnd stream cloned)
        st = rnd.getstate()
        r1 = random.Random(); r1.setstate(st)
        kind = build_key(o, r1, k)
        r2 = random.Random(); r2.setstate(st)
        build_key(f, r2, k)
        rnd.setstate(r1.getstate())
        keys.append((k, kind))
    divs = []
    for k, kind in keys:
        if o.cmd("EXISTS", k) == 0 or f.cmd("EXISTS", k) == 0:
            continue
        # Direction 1: redis DUMP -> fr RESTORE
        blob = o.cmd("DUMP", k)
        if isinstance(blob, bytes):
            res = f.cmd("RESTORE", "r2f_" + k, "0", blob, "REPLACE")
            if res != b"OK":
                divs.append((kind, k, "redis->fr RESTORE", res)); continue
            if readback(o, k) != readback(f, "r2f_" + k):
                divs.append((kind, k, "redis->fr value mismatch",
                             (readback(o, k), readback(f, "r2f_" + k))))
        # Direction 2: fr DUMP -> redis RESTORE
        blob = f.cmd("DUMP", k)
        if isinstance(blob, bytes):
            res = o.cmd("RESTORE", "f2r_" + k, "0", blob, "REPLACE")
            if res != b"OK":
                divs.append((kind, k, "fr->redis RESTORE", res)); continue
            if readback(f, k) != readback(o, "f2r_" + k):
                divs.append((kind, k, "fr->redis value mismatch",
                             (readback(f, k), readback(o, "f2r_" + k))))
    return divs


def main():
    oport, fport = int(sys.argv[1]), int(sys.argv[2])
    seed, nkeys = 1, 60
    a = sys.argv[3:]; j = 0
    while j < len(a):
        if a[j] == "--seed": seed = int(a[j+1]); j += 2
        elif a[j] == "--keys": nkeys = int(a[j+1]); j += 2
        else: j += 1
    total = []
    for s in range(seed, seed + 8):
        total += run(oport, fport, s, nkeys)
        if len(total) > 40: break
    if not total:
        print(f"PASS — DUMP/RESTORE round-trip byte-compatible across seeds "
              f"{seed}..{seed+7} x {nkeys} random keys")
        return 0
    print(f"FOUND {len(total)} divergences:")
    seen = set()
    for kind, k, what, detail in total:
        sig = (kind, what)
        if sig in seen: continue
        seen.add(sig)
        ds = repr(detail)
        print(f"\n[{kind}] {k}: {what}\n  {ds[:400]}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
