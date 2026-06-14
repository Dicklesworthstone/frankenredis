#!/usr/bin/env python3
"""blocking_fairness_gate.py — multi-blocked-client FIFO fairness differential
gate vs vendored redis 7.2.4.

The existing blocking_differ.py / blocking_edge_differ.py gates use a SINGLE
blocked waiter. This gate stresses the orthogonal dimension: when N clients
block on the SAME key, redis serves the LONGEST-waiting client first (FIFO by
block order), one push wakes EXACTLY one waiter, and a multi-element / MULTI-EXEC
push serves the front-of-queue waiters in order. Those are the wake-queue
ordering invariants that a single-waiter test cannot observe.

Method: connect N blockers and issue the blocking command on each in order with
small gaps to establish a deterministic FIFO block order; then feed pushes from a
separate connection; then read each blocker's reply (or __BLOCKED__ on timeout).
The per-blocker result vector must be byte-identical fr-vs-redis.

Covers BLPOP/BRPOP/BLMOVE/BZPOPMIN/BLMPOP, mixed BLPOP+BRPOP on one key,
N-blockers-vs-M-pushes partial service, and a blocked pop served by MULTI/EXEC.

Usage: blocking_fairness_gate.py <oracle_port> <fr_port>
"""
import socket, sys, time

OR = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FRp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def mk(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.settimeout(10)
    return s


def enc(*a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


def reader(s):
    buf = b""

    def rl():
        nonlocal buf
        while b"\r\n" not in buf:
            buf += s.recv(65536)
        l, buf = buf.split(b"\r\n", 1)
        return l

    def rd():
        nonlocal buf
        l = rl()
        t = l[:1]
        if t in (b'+', b':', b'-'):
            return l.decode()
        if t == b'$':
            n = int(l[1:])
            if n < 0:
                return None
            while len(buf) < n + 2:
                buf += s.recv(65536)
            d = buf[:n]
            buf = buf[n + 2:]
            return d.decode("latin1")
        if t == b'*':
            n = int(l[1:])
            return None if n < 0 else [rd() for _ in range(n)]
        return l.decode()

    return rd()


def run(port, blockers, feeder, settle=0.6):
    socks = []
    for args in blockers:
        s = mk(port)
        s.sendall(enc(*args))
        socks.append(s)
        time.sleep(0.08)
    time.sleep(0.2)
    fc = mk(port)
    for args in feeder:
        fc.sendall(enc(*args))
        reader(fc)
        time.sleep(0.05)
    time.sleep(settle)
    results = []
    for s in socks:
        s.settimeout(settle)
        try:
            results.append(reader(s))
        except socket.timeout:
            results.append("__BLOCKED__")
        except Exception as e:  # noqa: BLE001
            results.append("ERR:" + str(e))
    for s in socks:
        s.close()
    fc.close()
    return results


def flush():
    for p in (OR, FRp):
        c = mk(p)
        c.sendall(enc("flushall"))
        reader(c)
        c.close()


SCENARIOS = [
    ("3xBLPOP 1 push", [["blpop", "k", "0"]] * 3, [["lpush", "k", "v1"]]),
    ("3xBLPOP 2 push", [["blpop", "k", "0"]] * 3, [["lpush", "k", "a"], ["lpush", "k", "b"]]),
    ("3xBLPOP rpush 3", [["blpop", "k", "0"]] * 3, [["rpush", "k", "a", "b", "c"]]),
    ("3xBRPOP 2 push", [["brpop", "k", "0"]] * 3, [["rpush", "k", "a"], ["rpush", "k", "b"]]),
    ("BLPOP,BRPOP,BLPOP 1push",
     [["blpop", "k", "0"], ["brpop", "k", "0"], ["blpop", "k", "0"]], [["lpush", "k", "x"]]),
    ("2xBLMOVE 1push", [["blmove", "src", "dst", "left", "right", "0"]] * 2, [["lpush", "src", "m1"]]),
    ("3xBZPOPMIN 2add", [["bzpopmin", "z", "0"]] * 3, [["zadd", "z", "1", "a"], ["zadd", "z", "2", "b"]]),
    ("2xBLMPOP 1push", [["blmpop", "0", "1", "k", "left"]] * 2, [["rpush", "k", "v"]]),
    ("5xBLPOP 3push", [["blpop", "k", "0"]] * 5, [["rpush", "k", "a", "b", "c"]]),
    ("3xBLPOP via MULTI push",
     [["blpop", "k", "0"]] * 3, [["multi"], ["rpush", "k", "a", "b"], ["exec"]]),
]


def main():
    flush()
    div = 0
    for tag, blockers, feeder in SCENARIOS:
        o = run(OR, blockers, feeder)
        f = run(FRp, blockers, feeder)
        flush()
        if o != f:
            div += 1
            print(f"DIVERGE [{tag}]\n   blockers={blockers}\n   feeder={feeder}\n   O={o}\n   F={f}")
        else:
            print(f"ok   [{tag}] -> {o}")
    print("-" * 60)
    if div:
        print(f"FAIL — {div} blocking-fairness divergence(s)")
        return 1
    print("PASS — multi-blocked-client FIFO fairness byte-exact vs redis 7.2.4")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
