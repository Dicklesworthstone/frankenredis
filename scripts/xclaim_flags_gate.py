#!/usr/bin/env python3
"""xclaim_flags_gate.py — XCLAIM / XAUTOCLAIM flag-semantics parity vs vendored
redis 7.2.4.

The existing stream fuzzers (stream_command_fuzz_gate, rich_option_fuzz) exercise
XCLAIM/XAUTOCLAIM only in their bare `key group consumer min-idle id` form (plus
XAUTOCLAIM COUNT/JUSTID). XCLAIM's MUTATING flags — FORCE (create a PEL entry for
an id not currently pending), RETRYCOUNT (override the delivery counter), IDLE /
TIME (override the delivery time), LASTID, and JUSTID — change the PEL contents
and the reply shape with subtle interactions and were UNGATED. This locks them.

Each case runs against a freshly reseeded stream (5 entries, one group, all
delivered to consumer c1) and compares the XCLAIM reply AND the resulting PEL
state (via XPENDING) byte-exact vs redis. Volatile millisecond fields
(delivery-time / idle in XPENDING and the implicit delivery time XCLAIM stamps)
are normalized to <T> since they track wall-clock; RETRYCOUNT, ids, consumer
ownership, entry payloads and reply structure are compared exactly.

Usage: xclaim_flags_gate.py <oracle_port> <fr_port>
"""
import socket, sys, re, time

OR = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
FRp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400


def enc(*a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x.encode() if isinstance(x, str) else x
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


class Conn:
    def __init__(s, p):
        s.s = socket.create_connection(("127.0.0.1", p), timeout=10)
        s.s.settimeout(10)
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
            return l + b"\r\n"
        if t == b"$":
            n = int(l[1:])
            if n < 0:
                return b"$-1\r\n"
            while len(s.buf) < n + 2:
                s.buf += s.s.recv(65536)
            d = s.buf[:n]
            s.buf = s.buf[n + 2:]
            return b"$%d\r\n%s\r\n" % (n, d)
        if t in (b"*", b"~", b"%"):
            n = int(l[1:])
            if n < 0:
                return l + b"\r\n"
            out = l + b"\r\n"
            for _ in range(n * (2 if t == b"%" else 1)):
                out += s.read()
            return out
        return l + b"\r\n"

    def cmd(s, *a):
        s.s.sendall(enc(*a))
        return s.read()


SEED = (
    [["xadd", "s", f"{i}-0", "f", f"v{i}"] for i in range(1, 6)]
    + [["xgroup", "create", "s", "g", "0"],
       ["xreadgroup", "group", "g", "c1", "count", "10", "streams", "s", ">"]]
)

CASES = [
    ["xclaim", "s", "g", "c2", "0", "1-0"],
    ["xclaim", "s", "g", "c2", "0", "2-0", "justid"],
    ["xclaim", "s", "g", "c2", "0", "3-0", "retrycount", "7"],
    ["xclaim", "s", "g", "c2", "0", "4-0", "force"],
    ["xclaim", "s", "g", "c2", "0", "99-0", "force"],        # force on absent id
    ["xclaim", "s", "g", "c2", "0", "99-0"],                 # absent id, no force
    ["xclaim", "s", "g", "c2", "0", "1-0", "idle", "5000"],
    ["xclaim", "s", "g", "c2", "0", "2-0", "time", "111111111111"],
    ["xclaim", "s", "g", "c2", "0", "1-0", "retrycount", "3", "force", "justid"],
    ["xclaim", "s", "g", "c2", "0", "1-0", "lastid", "5-0"],
    ["xclaim", "s", "g", "c2", "999999999", "1-0"],          # min-idle too high
    ["xclaim", "s", "g", "c2", "0", "1-0", "2-0", "3-0", "justid"],  # multi-id
    ["xautoclaim", "s", "g", "c3", "0", "0"],
    ["xautoclaim", "s", "g", "c3", "0", "0", "count", "2"],
    ["xautoclaim", "s", "g", "c3", "0", "0", "justid"],
    ["xautoclaim", "s", "g", "c3", "999999999", "0"],
]

INT = re.compile(rb":(-?\d+)\r\n")


def norm_pend(b):
    # XPENDING extended entries are `*4 [id, consumer, idle_ms, delivery_count]`.
    # idle_ms = now - last_delivery is wall-clock-volatile (it differs by sub-ms
    # scheduling jitter even between two healthy servers), so normalize it; keep
    # delivery_count exact — that is what FORCE/RETRYCOUNT/JUSTID actually mutate.
    # Per entry the two `:int`s appear in order idle, count, so every even-indexed
    # integer match (0-based) is an idle and gets blanked.
    n = [0]

    def repl(m):
        i = n[0]
        n[0] += 1
        return b":<T>\r\n" if i % 2 == 0 else m.group(0)

    return INT.sub(repl, b)


def run(port, action):
    c = Conn(port)
    c.cmd("flushall")
    for s in SEED:
        c.cmd(*s)
    reply = c.cmd(*action)
    pend = c.cmd("xpending", "s", "g", "-", "+", "20")
    c.s.close()
    # XCLAIM/XAUTOCLAIM replies carry entry id+fields (no idle), so they are
    # deterministic; only the XPENDING idle column needs normalizing.
    return reply, norm_pend(pend)


def main():
    fails = []
    for action in CASES:
        ro = run(OR, action)
        fr = run(FRp, action)
        if ro != fr:
            fails.append((action, ro, fr))
    print("=" * 64)
    if fails:
        for a, ro, fr in fails:
            print(f"DIVERGE {' '.join(a)}\n  redis reply={ro[0]!r} pend={ro[1]!r}"
                  f"\n  fr    reply={fr[0]!r} pend={fr[1]!r}")
        print(f"FAIL — {len(fails)}/{len(CASES)} XCLAIM/XAUTOCLAIM flag divergence(s)")
        return 1
    print(f"PASS — XCLAIM/XAUTOCLAIM flag semantics byte-exact vs redis 7.2.4 "
          f"({len(CASES)} cases: reply + resulting PEL state, ms-normalized)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
