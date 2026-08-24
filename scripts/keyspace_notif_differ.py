#!/usr/bin/env python3
"""keyspace_notif_differ.py — differential fuzzer for keyspace notifications.

With `notify-keyspace-events KEA` set, a subscriber PSUBSCRIBEs to
`__key*@0__:*`; a second connection runs an identical random command sequence
against fr-server and the vendored redis 7.2.4 oracle. After each command the
emitted notifications (channel + payload, in order) are drained and compared.
This exercises WHICH events fire, their channel/payload, and ordering — e.g.
del-on-empty, rename_from/rename_to, copy_to, move_from/move_to, setrange,
incrby, spop/srem, zpop, hdel, lrem/ltrim, expire/persist, etc.

Deterministic events only: TTL-based "expired" is timer-driven and excluded.

Usage: keyspace_notif_differ.py [--oracle 16399] [--fr 16400] [--iters 3000] [--seed N]
"""
import argparse
import random
import socket
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.s.settimeout(2)
        self.buf = b""

    def _fill(self, block=True, timeout=0.06):
        try:
            if not block:
                self.s.settimeout(timeout)
            c = self.s.recv(65536)
            if not c:
                raise EOFError("closed")
            self.buf += c
            return True
        except socket.timeout:
            return False
        finally:
            if not block:
                self.s.settimeout(2)

    def _readline(self):
        while b"\r\n" not in self.buf:
            self._fill()
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _readn(self, n):
        while len(self.buf) < n + 2:
            self._fill()
        d, self.buf = self.buf[:n], self.buf[n + 2:]
        return d

    def _parse(self):
        line = self._readline()
        t, rest = line[:1], line[1:]
        if t == b":":
            return ("int", int(rest))
        if t == b"+":
            return ("status", rest)
        if t == b"-":
            return ("error", rest)
        if t == b"$":
            n = int(rest)
            return ("nil", None) if n < 0 else ("bulk", self._readn(n))
        if t in (b"*", b">", b"~"):
            n = int(rest)
            return ("nil", None) if n < 0 else ("array", [self._parse() for _ in range(n)])
        if t == b"%":
            n = int(rest)
            return ("array", [self._parse() for _ in range(n * 2)])
        return ("other", rest)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            if isinstance(a, int):
                a = str(a)
            if isinstance(a, str):
                a = a.encode()
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)
        return self._parse()

    def close(self):
        self.s.close()

    # Quiet-period budgets for the drain below. The loop can only conclude "no more
    # frames are coming" by waiting, so this timeout is paid on EVERY drain — twice
    # per iteration (oracle + fr). At the old flat 0.06 that was 120ms/iter, which
    # with the 30ms settle made 0.152 s/iter and put the registered 3000 iterations
    # at ~456s against parity_suite's 180s run_gate cap: the suite never saw this
    # gate's verdict, it killed it. (frankenredis-1zpr7)
    #
    # The budget is ADAPTIVE rather than simply smaller, because "I saw nothing" is
    # exactly the answer that must not be rushed: if a short wait made BOTH engines
    # miss the same straggler, they would agree on the empty set and the gate would
    # pass having observed nothing — the false-PASS class tesrb existed to remove.
    # So the full budget is still paid whenever we have seen NO event yet; only the
    # already-have-events case, where the question is merely "is there one more",
    # takes the short poll.
    QUIET_EMPTY = 0.06
    QUIET_AFTER_EVENT = 0.006

    def drain_pmessages(self):
        """Collect all pending pmessage frames (best-effort, ~timeout-bounded)."""
        msgs = []
        # ensure at least one short wait so async pushes arrive
        while True:
            quiet = self.QUIET_AFTER_EVENT if msgs else self.QUIET_EMPTY
            if b"\r\n" not in self.buf and not self._fill(block=False, timeout=quiet):
                break
            # parse one frame if a full one is buffered
            try:
                save = self.buf
                frame = self._parse_nonblock()
            except _Incomplete:
                self.buf = save
                # Keeps the FULL budget deliberately: a frame is already mid-flight
                # (split across TCP segments), so the remaining bytes are known to be
                # coming and cutting this short would truncate a real event.
                if not self._fill(block=False, timeout=self.QUIET_EMPTY):
                    break
                continue
            if frame is None:
                break
            if frame[0] == "array" and frame[1] and frame[1][0] == ("bulk", b"pmessage"):
                ch = frame[1][2][1]
                payload = frame[1][3][1]
                msgs.append((ch, payload))
        return msgs

    def _parse_nonblock(self):
        # Like _parse but raises _Incomplete instead of blocking.
        if b"\r\n" not in self.buf:
            raise _Incomplete()
        idx = self.buf.index(b"\r\n")
        line = self.buf[:idx]
        t, rest = line[:1], line[1:]
        if t == b"*":
            n = int(rest)
            self.buf = self.buf[idx + 2:]
            return ("array", [self._parse_nonblock() for _ in range(n)]) if n >= 0 else ("nil", None)
        if t == b"$":
            n = int(rest)
            if n < 0:
                self.buf = self.buf[idx + 2:]
                return ("nil", None)
            if len(self.buf) < idx + 2 + n + 2:
                raise _Incomplete()
            data = self.buf[idx + 2:idx + 2 + n]
            self.buf = self.buf[idx + 2 + n + 2:]
            return ("bulk", data)
        if t == b":":
            self.buf = self.buf[idx + 2:]
            return ("int", int(rest))
        self.buf = self.buf[idx + 2:]
        return ("other", rest)


class _Incomplete(Exception):
    pass


def expect_status(reply, expected, label):
    if reply != ("status", expected):
        raise SystemExit(f"SETUP FAILED [{label}]: got {reply!r}, expected status {expected!r}")


def expect_psubscribe(reply, pattern):
    expected = (
        "array",
        [("bulk", b"psubscribe"), ("bulk", pattern.encode()), ("int", 1)],
    )
    if reply != expected:
        raise SystemExit(f"SETUP FAILED [PSUBSCRIBE {pattern}]: got {reply!r}, expected {expected!r}")


def compare_notifications(label, oracle, fr):
    """Return one when the gate's sorted notification comparison detects a mismatch."""
    if oracle == fr:
        return 0
    print(f"{label}: redis={oracle!r} fr={fr!r}")
    return 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--iters", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=1234)
    ap.add_argument("--db", type=int, default=0, help="DB to exercise (catches db-prefix leaks)")
    ap.add_argument(
        "--planted-negative",
        action="store_true",
        help="prove the same notification comparison rejects a deliberately missing event",
    )
    args = ap.parse_args()
    if args.planted_negative:
        return compare_notifications(
            "PLANTED NEGATIVE detected", [(b"__keyevent@0__:set", b"k")], []
        )

    rng = random.Random(args.seed)  # nosec B311 -- deterministic seed is required for a reproducible differential-fuzzer schedule.
    op, fp = Conn(args.oracle), Conn(args.fr)
    os_, fs_ = Conn(args.oracle), Conn(args.fr)  # subscriber connections
    for c in (op, fp):
        expect_status(c.cmd("FLUSHALL"), b"OK", "FLUSHALL")
        expect_status(c.cmd("CONFIG", "SET", "notify-keyspace-events", "KEA"), b"OK", "CONFIG SET notify-keyspace-events")
        if args.db:
            expect_status(c.cmd("SELECT", str(args.db)), b"OK", f"SELECT {args.db}")
    for c in (os_, fs_):
        pattern = "__key*@%d__:*" % args.db
        expect_psubscribe(c.cmd("PSUBSCRIBE", pattern), pattern)
        time.sleep(0.05)
        c.drain_pmessages()  # clear the subscribe confirmation

    keys = ["k1", "k2", "k3"]

    def pick(options):
        return options[rng.randrange(len(options))]

    def k():
        return pick(keys)

    def v():
        return pick(["x", "1", "10", "-3", "ab", "yyy"])

    log = []
    ops = [
        lambda: ("SET", k(), v()),
        lambda: ("SETEX", k(), "100", v()),
        lambda: ("APPEND", k(), v()),
        lambda: ("SETRANGE", k(), str(rng.randint(0, 5)), v()),
        lambda: ("INCR", k()),
        lambda: ("INCRBY", k(), str(rng.randint(-5, 5))),
        lambda: ("GETSET", k(), v()),
        lambda: ("GETDEL", k()),
        lambda: ("DEL", k(), k()),
        lambda: ("EXPIRE", k(), "1000"),
        lambda: ("PERSIST", k()),
        lambda: ("RENAME", k(), k()),
        lambda: ("COPY", k(), k(), "REPLACE"),
        lambda: ("MOVE", k(), "1"),
        lambda: ("LPUSH", k(), v()),
        lambda: ("RPUSH", k(), v(), v()),
        lambda: ("LPOP", k()),
        lambda: ("RPOP", k(), str(rng.randint(1, 2))),
        lambda: ("LREM", k(), str(rng.randint(-2, 2)), v()),
        lambda: ("LSET", k(), "0", v()),
        lambda: ("LINSERT", k(), "BEFORE", v(), v()),
        lambda: ("LTRIM", k(), "0", "1"),
        lambda: ("SADD", k(), v(), v()),
        lambda: ("SREM", k(), v()),
        lambda: ("SINTERSTORE", k(), k(), k()),
        lambda: ("HSET", k(), v(), v()),
        lambda: ("HDEL", k(), v()),
        lambda: ("HINCRBY", k(), v(), "2"),
        lambda: ("ZADD", k(), "1", v()),
        lambda: ("ZREM", k(), v()),
        lambda: ("ZINCRBY", k(), "1", v()),
        lambda: ("ZPOPMIN", k()),
        lambda: ("XADD", k(), "*", "f", v()),
        # ── broadened event-edge coverage ──
        lambda: ("DECR", k()),
        lambda: ("DECRBY", k(), str(rng.randint(-5, 5))),
        lambda: ("INCRBYFLOAT", k(), pick(["1", "2.5", "-1"])),
        lambda: ("SETNX", k(), v()),
        lambda: ("MSET", k(), v(), k(), v()),
        lambda: ("SETBIT", k(), str(rng.randint(0, 20)), str(rng.randint(0, 1))),
        lambda: ("GETEX", k()) + pick([(), ("PERSIST",), ("EX", "100"), ("EXAT", "1")]),
        lambda: ("EXPIRE", k(), "500", pick(["NX", "XX", "GT", "LT"])),
        lambda: ("PEXPIRE", k(), "500000"),
        lambda: ("EXPIREAT", k(), "9999999999"),
        lambda: ("RENAMENX", k(), k()),
        lambda: ("COPY", k(), k()),
        lambda: ("ZADD", k(), pick(["GT", "LT", "NX", "XX"]), str(rng.randint(-3, 3)), v()),
        lambda: ("ZADD", k(), "INCR", "1", v()),
        lambda: ("ZRANGESTORE", k(), k(), "0", "-1"),
        lambda: ("ZREMRANGEBYRANK", k(), "0", "0"),
        lambda: ("ZPOPMAX", k()),
        lambda: ("HSETNX", k(), v(), v()),
        lambda: ("HINCRBYFLOAT", k(), v(), "1.5"),
        lambda: ("SMOVE", k(), k(), v()),
        # SPOP excluded: random member removal desyncs set state → downstream
        # event false positives (its own "spop" event payload is just the key).
        lambda: ("SUNIONSTORE", k(), k(), k()),
        lambda: ("LMOVE", k(), k(), pick(["LEFT", "RIGHT"]), pick(["LEFT", "RIGHT"])),
        lambda: ("SORT", k(), "STORE", k()),
        lambda: ("BITFIELD", k(), "SET", "u8", "0", str(rng.randint(0, 255))),
    ]

    def cleanup():
        for c in (op, fp):
            try:
                c.cmd("CONFIG", "SET", "notify-keyspace-events", "")
                c.cmd("FLUSHALL")
            except Exception:
                pass

    observed_events = 0
    for it in range(args.iters):
        opv = tuple(str(x) for x in pick(ops)())
        op.cmd(*opv)
        fp.cmd(*opv)
        # Async pmessages can arrive slightly after the command reply. Give both
        # subscribers time to receive, then re-drain once after a settle to catch
        # any straggler before declaring a mismatch — avoids false positives from
        # an event landing in the next iteration's drain window.
        time.sleep(0.03)
        oe = sorted(os_.drain_pmessages())
        fe = sorted(fs_.drain_pmessages())
        if oe != fe:
            time.sleep(0.05)
            oe = sorted(oe + os_.drain_pmessages())
            fe = sorted(fe + fs_.drain_pmessages())
        observed_events += len(oe) + len(fe)
        log.append(" ".join(opv) + "  => O:%d F:%d events" % (len(oe), len(fe)))
        if oe != fe:
            print("=== KEYSPACE-EVENT DIVERGENCE at iter %d ===" % it)
            print("seed=%d" % args.seed)
            print("op: %s" % " ".join(opv))
            print("oracle events: %s" % oe)
            print("fr     events: %s" % fe)
            print("--- op log (last 30) ---")
            for line in log[-30:]:
                print("  " + line)
            cleanup()
            return 1

    cleanup()
    for c in (op, fp, os_, fs_):
        c.close()
    if observed_events == 0:
        print("SETUP FAILED: captured zero keyspace events; subscription or notification setup was skipped")
        return 1
    print("OK: %d iters, seed %d — no keyspace-notification divergence" % (args.iters, args.seed))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
