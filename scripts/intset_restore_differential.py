#!/usr/bin/env python3
"""Live same-invocation differential for the typed-intset RESTORE/RDB paths (frankenredis-y6zqo).

y6zqo was marked complete on a SOURCE READ with no build and no execution. The claim is that two
separate paths -- the RDB/DEBUG RELOAD path via `RdbValue::IntSet` and the RESTORE command path via
`decode_intset_ints` -> `SetValue::Int` -- both install integers natively instead of round-tripping
each value through decimal bytes. A source read can confirm the code exists; it cannot confirm the
bytes on the wire still match the incumbent, and a typed fast path that drops or reorders a value is
exactly the failure a read does not catch.

So this asks the question the refactor is answerable to: for the same members, do fr and redis 7.2.4
produce the SAME DUMP payload, accept each other's shape, and survive a reload with the same
membership and the same encoding.

DUMP PAYLOAD EQUALITY IS THE STRONG ROW, AND ONLY FOR THE INTSET ENCODING. An intset is stored in
canonical strictly-increasing order, so the payload pins width, ordering, length header, RDB version
and CRC -- a typed path that silently widened an i16 intset to i32, or lost the order the decoder
relies on, shows up here even though every membership query would still agree.

Past set-max-intset-entries the container becomes a hashtable and the payload follows hash iteration
order, which is NOT a parity property: MEASURED, two separate redis 7.2.4 processes produce DIFFERENT
DUMP bytes for the same 513 members, while one process rebuilding the same set twice is stable. So
comparing those bytes across engines fails against redis ITSELF. This script compares payload bytes
only where the encoding is intset, and for every case asserts the property that does hold everywhere
-- each engine RESTOREs the other's payload to the same cardinality and membership.

The sizes straddle the three intset widths (i16/i32/i64) and the set-max-intset-entries boundary,
because the encoding is chosen per width and the container converts to a hashtable past the limit.
"""
import os
import socket
import subprocess
import sys
import tempfile
import time

ROOT = "/data/projects/frankenredis"
FR = os.path.join(ROOT, "target/release/frankenredis")
REDIS = os.path.join(ROOT, "legacy_redis_code/redis/src/redis-server")
FR_PORT, REDIS_PORT = 7811, 7812

# (name, members) -- chosen to straddle INTSET_ENC_INT16/INT32/INT64 and the entries limit.
CASES = [
    ("i16_small",     [1, 2, 3]),
    ("i16_negative",  [-32768, -1, 0, 32767]),
    ("i32_boundary",  [-32769, 32768, 100000]),
    ("i64_boundary",  [-2147483649, 2147483648, 9223372036854775807]),
    ("i64_min",       [-9223372036854775808, 0]),
    ("mixed_widths",  [-1, 300, 70000, 5000000000]),
    ("dense_128",     list(range(128))),
    ("at_limit_512",  list(range(512))),
    ("over_limit_513", list(range(513))),
    ("unsorted_input", [9, 3, 7, 1, 5]),
    ("single",        [42]),
]


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


def _cpu_snapshot():
    with open("/proc/stat") as f:
        parts = f.readline().split()
    fields = [int(x) for x in parts[1:]]
    return sum(fields), fields[4]  # (total jiffies, iowait jiffies)


def iowait_and_mhz(window_s: float = 1.0):
    """Windowed iowait, plus mean CPU MHz.

    CORRECTED: this read `iowait / total` from a SINGLE /proc/stat sample, which is the
    average SINCE BOOT and not the measurement's conditions. On a host three days up, one
    sampling window is ~0.0009% of the accumulated jiffies, so the value cannot move: every
    row this helper produced recorded 0.93-0.95% no matter what the machine was doing. It
    read 0.95% cumulative at an instant when the true windowed figure was 0.35% and when the
    fleet reported the host disk-bound at 32%.

    A rate has to be differenced. Two samples `window_s` apart give the iowait share of the
    jiffies that actually elapsed during the window, which is the number a measurement row is
    claiming when it prints one.
    """
    total_a, io_a = _cpu_snapshot()
    time.sleep(window_s)
    total_b, io_b = _cpu_snapshot()
    d_total = total_b - total_a
    iowait = (io_b - io_a) / d_total * 100 if d_total else 0.0
    try:
        with open("/proc/cpuinfo") as f:
            v = [float(l.split(":")[1]) for l in f if l.startswith("cpu MHz")]
        mhz = "%.0f" % (sum(v) / len(v)) if v else "?"
    except OSError:
        mhz = "?"
    return "%.2f%%" % iowait, mhz


def start(binary, port, tag, env_extra=None):
    d = tempfile.mkdtemp(prefix=f"intset_{tag}_")
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    p = subprocess.Popen([binary, "--port", str(port), "--dir", d, "--save", ""],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    for _ in range(150):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return p
        except OSError:
            time.sleep(0.1)
    raise SystemExit(f"{tag} did not start on {port}")


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 20)
        self.s.settimeout(20)
        self.buf = b""

    def _line(self):
        while b"\r\n" not in self.buf:
            c = self.s.recv(65536)
            if not c:
                raise IOError("closed")
            self.buf += c
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        t, rest = line[:1], line[1:]
        if t in (b"+", b":"):
            return rest
        if t == b"-":
            return b"ERR:" + rest
        if t == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                c = self.s.recv(65536)
                if not c:
                    raise IOError("closed")
                self.buf += c
            out, self.buf = self.buf[:n], self.buf[n + 2:]
            return out
        if t == b"*":
            n = int(rest)
            return None if n == -1 else [self._read() for _ in range(n)]
        raise IOError("bad type %r" % t)

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            b = a if isinstance(a, bytes) else str(a).encode()
            out += b"$%d\r\n%s\r\n" % (len(b), b)
        self.s.sendall(out)
        return self._read()

    def close(self):
        self.s.close()


def probe(c, name, members):
    """Return the full observable shape of one case as comparable plain data."""
    key, restored = ("k:" + name).encode(), ("r:" + name).encode()
    c.cmd("DEL", key, restored)
    c.cmd("SADD", key, *members)
    row = {
        "card": c.cmd("SCARD", key),
        "encoding": c.cmd("OBJECT", "ENCODING", key),
        "members": sorted(c.cmd("SMEMBERS", key) or [], key=lambda b: int(b)),
    }
    payload = c.cmd("DUMP", key)
    # Only canonically-ordered encodings can be compared byte for byte across engines; see the
    # module docstring for the redis-vs-redis measurement that establishes this.
    canonical = row["encoding"] == b"intset"
    row["dump"] = payload.hex().encode() if canonical else b"<hash order, not comparable>"
    row["payload_len"] = len(payload) if canonical else None
    # Round-trip through the RESTORE command path.
    row["restore_ok"] = c.cmd("RESTORE", restored, 0, payload)
    row["restore_encoding"] = c.cmd("OBJECT", "ENCODING", restored)
    row["restore_members"] = sorted(c.cmd("SMEMBERS", restored) or [], key=lambda b: int(b))
    # And through the RDB path.
    c.cmd("DEBUG", "RELOAD")
    row["reload_encoding"] = c.cmd("OBJECT", "ENCODING", key)
    row["reload_members"] = sorted(c.cmd("SMEMBERS", key) or [], key=lambda b: int(b))
    row["reload_dump"] = ((c.cmd("DUMP", key) or b"").hex().encode()
                          if row["reload_encoding"] == b"intset"
                          else b"<hash order, not comparable>")
    return row, payload


def cross_restore(c, name, payload, expect_members):
    """Does THIS engine accept the OTHER engine's payload? Holds for every encoding."""
    key = ("x:" + name).encode()
    c.cmd("DEL", key)
    ok = c.cmd("RESTORE", key, 0, payload)
    members = sorted(c.cmd("SMEMBERS", key) or [], key=lambda b: int(b))
    return ok == b"OK" and members == expect_members


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else ""
    for b in (FR, REDIS):
        if not os.path.exists(b):
            raise SystemExit("missing binary: " + b)
    fr = start(FR, FR_PORT, "fr", {"FR_SHARED_NOTHING_PARTITIONS": "4"})
    rd = start(REDIS, REDIS_PORT, "redis")
    a, b = Conn(FR_PORT), Conn(REDIS_PORT)
    bad = 0
    try:
        io, mhz = iowait_and_mhz()
        print("intset RESTORE/RDB differential -- fr vs redis 7.2.4, one invocation")
        print("loadavg %s, iowait %s, CPU %s MHz" % (loadavg(), io, mhz))
        print()
        print("%-16s %-9s %s" % ("case", "verdict", "first differing field"))
        print("-" * 78)
        for name, members in CASES:
            if only and only not in name:
                continue
            (ra, pa), (rb, pb) = probe(a, name, members), probe(b, name, members)
            diff = [k for k in ra if ra[k] != rb[k]]
            # The always-valid property: each engine accepts the other's bytes.
            if not cross_restore(a, name, pb, rb["members"]):
                diff.append("fr_rejects_redis_payload")
            if not cross_restore(b, name, pa, ra["members"]):
                diff.append("redis_rejects_fr_payload")
            if diff:
                bad += 1
                print("%-16s %-9s %s" % (name, "DIVERGE", ", ".join(diff)))
                for k in diff:
                    print("      %-18s fr    %s" % (k, str(ra[k])[:90]))
                    print("      %-18s redis %s" % (k, str(rb[k])[:90]))
            else:
                print("%-16s %-9s (%s members, %s)"
                      % (name, "match", ra["card"].decode(), ra["encoding"].decode()))
        io2, _ = iowait_and_mhz()
        print()
        print("%d diverging cases, end loadavg %s, iowait %s" % (bad, loadavg(), io2))
    finally:
        for c in (a, b):
            try:
                c.close()
            except OSError:
                pass
        for p in (fr, rd):
            p.terminate()
        for p in (fr, rd):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
