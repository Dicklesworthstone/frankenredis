#!/usr/bin/env python3
"""Seeded RESTORE corruption fuzz: fr vs redis 7.2.4 (frankenredis-nhw3z).

RESTORE deserializes a client-supplied DUMP payload, so it must be robust against
arbitrary corruption — erroring (never panicking / hanging / disconnecting) exactly
where redis does. This fuzz DUMPs valid string/list/hash/zset/set/stream values,
then applies random mutations (single bit-flip, truncation, byte insertion, random
byte, multi bit-flip) and RESTOREs the corrupted payload on BOTH servers, asserting:
(a) fr never throws a socket exception (a proxy for panic/hang/disconnect), and
(b) the reply is byte-identical to redis. Seeded for reproducibility.

Usage: restore_corruption_fuzz.py <oracle_port> <fr_port> [iters] [seed]
       Exit 0 = identical + no fr exception, 1 = divergence / fr panic.
"""
import random
import socket
import sys
import time


def conn(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=5)
    s.settimeout(3)
    return s


def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o)
    time.sleep(0.004)
    try:
        return s.recv(1 << 20)
    except Exception as e:
        return ("EXC", str(e))


def dump_payload(s, key):
    r = cmd(s, "DUMP", key)
    nl = r.index(b"\r\n")
    return r[nl + 2 : nl + 2 + int(r[1:nl])]


def corrupt(rnd, p):
    if not p:
        return p
    p = bytearray(p)
    mode = rnd.randint(0, 4)
    if mode == 0:
        i = rnd.randrange(len(p)); p[i] ^= (1 << rnd.randint(0, 7))
    elif mode == 1:
        return bytes(p[: rnd.randrange(len(p) + 1)])
    elif mode == 2:
        i = rnd.randrange(len(p) + 1); p[i:i] = bytes([rnd.randint(0, 255)])
    elif mode == 3:
        i = rnd.randrange(len(p)); p[i] = rnd.randint(0, 255)
    else:
        for _ in range(rnd.randint(1, 4)):
            i = rnd.randrange(len(p)); p[i] ^= (1 << rnd.randint(0, 7))
    return bytes(p)


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    iters = int(sys.argv[3]) if len(sys.argv) > 3 else 4000
    seed = int(sys.argv[4]) if len(sys.argv) > 4 else 20260618
    od, fr = conn(op), conn(fp)
    for s in (od, fr):
        cmd(s, "FLUSHALL")
        cmd(s, "SET", "ss", "hello world this is a longer string value")
        cmd(s, "RPUSH", "ls", *[f"item{i}" for i in range(40)])
        cmd(s, "HSET", "hs", *sum([[f"f{i}", f"v{i}"] for i in range(30)], []))
        cmd(s, "ZADD", "zs", *sum([[str(i), f"m{i}"] for i in range(30)], []))
        cmd(s, "SADD", "es", *[str(i) for i in range(40)])
        cmd(s, "XADD", "xs", "1-1", "f", "v")
        cmd(s, "XADD", "xs", "2-1", "g", "w")
    bases = {k: dump_payload(od, k) for k in ("ss", "ls", "hs", "zs", "es", "xs")}
    rnd = random.Random(seed)
    diffs = 0
    panics = 0
    fails = []
    for it in range(iters):
        k = rnd.choice(list(bases))
        payload = corrupt(rnd, bases[k])
        dk = f"r{it}"
        ro = cmd(od, "RESTORE", dk, "0", payload)
        rf = cmd(fr, "RESTORE", dk, "0", payload)
        if isinstance(rf, tuple):
            panics += 1
            fails.append(f"fr-EXCEPTION it={it} key={k} payload={payload[:48]!r}: {rf}")
            fr = conn(fp)  # reconnect and keep going (up to a cap)
            if panics > 8:
                break
            continue
        if isinstance(ro, bytes) and ro.startswith(b"+OK"):
            cmd(od, "DEL", dk)
            cmd(fr, "DEL", dk)
        if ro != rf:
            diffs += 1
            if len(fails) < 10:
                fails.append(f"DIFF it={it} key={k}: redis={ro!r} fr={rf!r} payload={payload[:48]!r}")
    print("=" * 60)
    if diffs or panics:
        print(f"FAIL — RESTORE corruption fuzz: {diffs} diff(s), {panics} fr-exception(s) over {iters} iters:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        f"PASS — RESTORE corruption fuzz: fr errors identically to redis 7.2.4 with no "
        f"panic/hang ({iters} mutated payloads, seed {seed}, 6 types x bit-flip/truncate/insert/multi)"
    )


if __name__ == "__main__":
    main()
