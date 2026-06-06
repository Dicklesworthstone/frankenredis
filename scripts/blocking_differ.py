#!/usr/bin/env python3
"""blocking_differ.py — stateful multi-connection differential gate for the
BLOCKING command surface + WATCH/MULTI/EXEC CAS vs vendored redis 7.2.4.

Covers: BLPOP/BRPOP (single + multi-key, immediate + blocked-then-pushed +
timeout), BLMOVE/BRPOPLPUSH, BLMPOP, BZPOPMIN/BZPOPMAX, BZMPOP, XREAD BLOCK,
and WATCH-based optimistic-lock aborts driven by a second connection.

A separate "pusher" connection feeds the blocked "waiter" after a short delay;
the waiter's reply (and FIFO ordering across two waiters) is compared to redis.
Timeouts use fractional seconds so the suite stays fast.

Usage: blocking_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if byte-exact, else 1.
"""
import argparse
import socket
import sys
import threading
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5.0)
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

    def send(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)

    def cmd(self, *a):
        self.send(*a)
        return self.parse()


def blocked_then_feed(port, waiter_cmd, feeder_cmds, feed_delay=0.15):
    """Run waiter_cmd on conn A (blocks); after feed_delay, run feeder_cmds on
    conn B; return the waiter's reply."""
    w = Conn(port)
    result = {}

    def wait():
        try:
            result["r"] = w.cmd(*waiter_cmd)
        except Exception as e:
            result["r"] = ("EXC", str(e))

    th = threading.Thread(target=wait)
    th.start()
    time.sleep(feed_delay)
    feeder = Conn(port)
    for fc in feeder_cmds:
        feeder.cmd(*fc)
    th.join(6)
    return result.get("r", ("TIMEOUT",))


def two_waiters_fifo(port, key, waiter_cmd_factory, feeder_cmds, feed_delay=0.25):
    """Start two blocking waiters in order, then feed; return [first, second]
    replies to check FIFO wakeup order."""
    results = [None, None]
    conns = [Conn(port), Conn(port)]

    def wait(i):
        try:
            results[i] = conns[i].cmd(*waiter_cmd_factory())
        except Exception as e:
            results[i] = ("EXC", str(e))

    t0 = threading.Thread(target=wait, args=(0,))
    t0.start()
    time.sleep(0.08)  # ensure waiter 0 enqueues first
    t1 = threading.Thread(target=wait, args=(1,))
    t1.start()
    time.sleep(feed_delay)
    feeder = Conn(port)
    for fc in feeder_cmds:
        feeder.cmd(*fc)
    t0.join(6)
    t1.join(6)
    return results


def run(port):
    r = {}
    c = Conn(port)
    c.cmd("FLUSHALL")

    # --- immediate (non-blocking) cases ---
    c.cmd("RPUSH", "l", "a", "b", "c")
    r["blpop_immediate"] = c.cmd("BLPOP", "l", "0.5")
    r["brpop_immediate"] = c.cmd("BRPOP", "l", "0.5")
    c.cmd("DEL", "l")
    # --- timeout (no data) ---
    r["blpop_timeout"] = c.cmd("BLPOP", "nope", "0.2")
    r["brpop_timeout"] = c.cmd("BRPOP", "nope", "0.2")
    r["blmove_timeout"] = c.cmd("BLMOVE", "nope", "dst", "LEFT", "RIGHT", "0.2")
    r["blmpop_timeout"] = c.cmd("BLMPOP", "0.2", "2", "n1", "n2", "LEFT")
    r["bzpopmin_timeout"] = c.cmd("BZPOPMIN", "nope", "0.2")
    r["bzmpop_timeout"] = c.cmd("BZMPOP", "0.2", "2", "n1", "n2", "MIN")
    # --- blocked then fed (single key) ---
    r["blpop_fed"] = blocked_then_feed(port, ("BLPOP", "bk", "2"), [("RPUSH", "bk", "X")])
    r["brpop_fed"] = blocked_then_feed(port, ("BRPOP", "bk2", "2"), [("RPUSH", "bk2", "Y", "Z")])
    # --- blocked then fed (multi key — first available key wins) ---
    r["blpop_multi"] = blocked_then_feed(
        port, ("BLPOP", "m1", "m2", "2"), [("RPUSH", "m2", "second")])
    # --- BLMOVE / BRPOPLPUSH blocked then fed ---
    r["blmove_fed"] = blocked_then_feed(
        port, ("BLMOVE", "src", "dst", "LEFT", "RIGHT", "2"), [("RPUSH", "src", "v1")])
    r["blmove_dst"] = c.cmd("LRANGE", "dst", "0", "-1")
    r["brpoplpush_fed"] = blocked_then_feed(
        port, ("BRPOPLPUSH", "src2", "dst2", "2"), [("RPUSH", "src2", "w1")])
    # --- BLMPOP blocked then fed ---
    r["blmpop_fed"] = blocked_then_feed(
        port, ("BLMPOP", "2", "2", "p1", "p2", "LEFT", "COUNT", "2"),
        [("RPUSH", "p2", "a", "b", "c")])
    # --- BZPOPMIN / BZPOPMAX blocked then fed ---
    r["bzpopmin_fed"] = blocked_then_feed(
        port, ("BZPOPMIN", "zk", "2"), [("ZADD", "zk", "5", "lo", "9", "hi")])
    r["bzpopmax_fed"] = blocked_then_feed(
        port, ("BZPOPMAX", "zk2", "2"), [("ZADD", "zk2", "5", "lo", "9", "hi")])
    # --- BZMPOP blocked then fed ---
    r["bzmpop_fed"] = blocked_then_feed(
        port, ("BZMPOP", "2", "2", "zp1", "zp2", "MIN", "COUNT", "2"),
        [("ZADD", "zp2", "1", "a", "2", "b", "3", "c")])
    # --- FIFO: two BLPOP waiters, one push wakes the FIRST only ---
    fifo = two_waiters_fifo(port, "fk", lambda: ("BLPOP", "fk", "2"),
                            [("RPUSH", "fk", "one")])
    r["fifo_first"] = fifo[0]
    r["fifo_second_timeout"] = fifo[1]  # second should still time out (or get nothing)
    # --- XREAD BLOCK then fed ---
    c.cmd("XADD", "strm", "1-1", "f", "v0")
    r["xread_block_fed"] = blocked_then_feed(
        port, ("XREAD", "BLOCK", "2000", "STREAMS", "strm", "$"),
        [("XADD", "strm", "2-2", "f", "v1")])
    r["xread_block_timeout"] = c.cmd("XREAD", "BLOCK", "150", "STREAMS", "strm", "$")
    # --- WATCH/MULTI/EXEC CAS: second conn modifies key -> EXEC aborts (nil) ---
    w = Conn(port)
    w.cmd("SET", "cas", "1")
    w.cmd("WATCH", "cas")
    other = Conn(port)
    other.cmd("SET", "cas", "2")
    w.cmd("MULTI")
    w.cmd("SET", "cas", "3")
    r["cas_aborted"] = w.cmd("EXEC")           # nil (watched key changed)
    r["cas_value_after"] = c.cmd("GET", "cas")  # "2"
    # --- WATCH then no modification -> EXEC succeeds ---
    w2 = Conn(port)
    w2.cmd("SET", "cas2", "1")
    w2.cmd("WATCH", "cas2")
    w2.cmd("MULTI")
    w2.cmd("SET", "cas2", "9")
    r["cas_ok"] = w2.cmd("EXEC")               # [OK]
    r["cas_ok_value"] = c.cmd("GET", "cas2")    # "9"
    # --- WATCH key that expires -> EXEC aborts ---
    w3 = Conn(port)
    w3.cmd("SET", "casx", "1", "PX", "50")
    w3.cmd("WATCH", "casx")
    time.sleep(0.12)  # let it expire
    w3.cmd("MULTI")
    w3.cmd("GET", "casx")
    r["cas_expired"] = w3.cmd("EXEC")          # redis: nil (expiry counts as touch)
    # --- UNWATCH clears the watch ---
    w4 = Conn(port)
    w4.cmd("SET", "casu", "1")
    w4.cmd("WATCH", "casu")
    w4.cmd("UNWATCH")
    other2 = Conn(port)
    other2.cmd("SET", "casu", "2")
    w4.cmd("MULTI")
    w4.cmd("SET", "casu", "3")
    r["cas_unwatch_ok"] = w4.cmd("EXEC")       # [OK] (unwatched)
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()

    o = run(args.oracle)
    f = run(args.fr)
    diffs = 0
    for k in o:
        if o[k] != f.get(k):
            diffs += 1
            print(f"DIFF [{k}]")
            print(f"   oracle: {o[k]!r}")
            print(f"   fr    : {f.get(k)!r}")
    if diffs:
        print(f"\nFAIL: {diffs} blocking/CAS divergences")
        sys.exit(1)
    print(f"OK: {len(o)} blocking + WATCH/CAS cases byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
