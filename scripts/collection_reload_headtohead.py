#!/usr/bin/env python3
"""collection_reload_headtohead.py — head-to-head TIMING of the RDB collection
codec (DEBUG RELOAD = save+load, and isolated RESTORE = decode) for fr vs the
vendored Redis 7.2.4, on a collection-heavy DB.

This fills the measurement gap the scorecard flags as "owed": the existing
reload_*_gate.py scripts assert FIDELITY (byte/digest parity) but never TIME the
codec head-to-head, so the collection encode/decode levers (presize cluster +
BlackThrush's decode_listpack num_elements presize 0ea29b6fe) had no vs-Redis
ratio. DEBUG RELOAD's load half exercises the decode path; RESTORE isolates it.

Both servers must be started with --enable-debug-command yes|local. Under host
contention absolute ms is noisy, so we INTERLEAVE fr/redis trials and report the
median ratio (the ratio is stable even when absolutes drift), plus CV.

Usage: collection_reload_headtohead.py <redis_port> <fr_port> [--trials N]
       [--hashes H] [--sets S] [--zsets Z] [--members M]
       [--set-kind str|int]
Exit 0 always (informational).
"""
import socket
import sys
import time
import statistics


def opt(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


RS = int(sys.argv[1]) if len(sys.argv) > 1 else 17812
FR = int(sys.argv[2]) if len(sys.argv) > 2 else 17811
TRIALS = int(opt("--trials", "9"))
HASHES = int(opt("--hashes", "2000"))
SETS = int(opt("--sets", "2000"))
ZSETS = int(opt("--zsets", "2000"))
MEMBERS = int(opt("--members", "40"))
SET_KIND = opt("--set-kind", "str")


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 5)
        self.s.settimeout(30.0)
        self.b = b""

    def _fill(self):
        d = self.s.recv(1 << 16)
        if not d:
            raise EOFError
        self.b += d

    def _line(self):
        while b"\r\n" not in self.b:
            self._fill()
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def read(self):
        l = self._line()
        t, rest = l[:1], l[1:]
        if t in (b"+", b"-", b":"):
            return rest
        if t == b"$":
            n = int(rest)
            if n < 0:
                return None
            while len(self.b) < n + 2:
                self._fill()
            data, self.b = self.b[:n], self.b[n + 2:]
            return data
        if t == b"*":
            n = int(rest)
            return [self.read() for _ in range(n)]
        return l

    def cmd(self, *args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            a = a if isinstance(a, (bytes, bytearray)) else str(a).encode()
            out.append(b"$%d\r\n%s\r\n" % (len(a), a))
        self.s.sendall(b"".join(out))
        return self.read()

    def pipe(self, cmds):
        buf = []
        for args in cmds:
            buf.append(b"*%d\r\n" % len(args))
            for a in args:
                a = a if isinstance(a, (bytes, bytearray)) else str(a).encode()
                buf.append(b"$%d\r\n%s\r\n" % (len(a), a))
        self.s.sendall(b"".join(buf))
        return [self.read() for _ in cmds]


def preload(c):
    c.cmd("FLUSHALL")
    batch = []
    for i in range(HASHES):
        args = ["HSET", f"h:{i}"]
        for j in range(MEMBERS):
            args += [f"f{j}", f"v{j}"]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    for i in range(SETS):
        args = ["SADD", f"s:{i}"] + [set_member(i, j) for j in range(MEMBERS)]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    for i in range(ZSETS):
        args = ["ZADD", f"z:{i}"]
        for j in range(MEMBERS):
            args += [j, f"m{j}"]
        batch.append(args)
        if len(batch) >= 200:
            c.pipe(batch); batch = []
    if batch:
        c.pipe(batch)


def set_member(i, j):
    if SET_KIND == "int":
        if i % 3 == 0:
            return j - (MEMBERS // 2)
        if i % 3 == 1:
            return (j * 257) - 12_345
        return (j * 1_048_573) - 2_147_483_000
    if SET_KIND != "str":
        raise ValueError(f"--set-kind must be str or int, got {SET_KIND!r}")
    return f"m{j}"


def time_reload(c):
    t0 = time.perf_counter()
    r = c.cmd("DEBUG", "RELOAD")
    dt = time.perf_counter() - t0
    if r != b"OK":
        raise RuntimeError(f"DEBUG RELOAD failed: {r!r}")
    return dt


def time_dump(c, keys):
    """Pipelined DUMP of every key (isolates the ENCODE half)."""
    t0 = time.perf_counter()
    for i in range(0, len(keys), 500):
        c.pipe([["DUMP", k] for k in keys[i:i + 500]])
    return time.perf_counter() - t0


def time_restore(c, payloads):
    """Pipelined RESTORE ... REPLACE of every payload (isolates the DECODE half)."""
    t0 = time.perf_counter()
    for i in range(0, len(payloads), 500):
        c.pipe([["RESTORE", b"r:" + k, 0, p, b"REPLACE"]
                for k, p in payloads[i:i + 500]])
    return time.perf_counter() - t0


def main():
    fr, rs = Conn(FR), Conn(RS)
    print(f"fr:{FR} redis:{RS}  hashes={HASHES} sets={SETS} zsets={ZSETS} members={MEMBERS} set_kind={SET_KIND}")
    print("preloading identical collection-heavy DB into both...")
    preload(fr); preload(rs)
    fk = fr.cmd("DBSIZE"); rk = rs.cmd("DBSIZE")
    print(f"DBSIZE fr={fk.decode()} redis={rk.decode()}")
    # warm one reload each
    time_reload(fr); time_reload(rs)
    fr_t, rs_t, ratios = [], [], []
    for _ in range(TRIALS):
        rt = time_reload(rs); ft = time_reload(fr)   # interleaved
        rs_t.append(rt); fr_t.append(ft); ratios.append(rt / ft)
    def cv(xs):
        return 100 * statistics.pstdev(xs) / statistics.mean(xs)
    print("\nDEBUG RELOAD (save+load round-trip):")
    print(f"  fr    median={statistics.median(fr_t)*1000:.1f}ms  cv={cv(fr_t):.1f}%")
    print(f"  redis median={statistics.median(rs_t)*1000:.1f}ms  cv={cv(rs_t):.1f}%")
    mr = statistics.median(ratios)

    def verdict(r):
        return "fr FASTER" if r > 1.05 else ("redis faster" if r < 0.95 else "parity")
    print(f"  median ratio (redis/fr) = {mr:.3f}x  [{verdict(mr)}]  trials={[round(r,2) for r in ratios]}")

    # Isolate the two halves so the gap can be attributed to encode (DUMP) vs
    # decode (RESTORE). DEBUG RELOAD's load half exercises the decode path
    # (fr-persist decode_listpack + fr-store object rebuild); DUMP exercises the
    # encode path (fr-store dump_key + fr-persist encode_compact_*).
    keys = [k for k in fr.cmd("KEYS", "*")]
    payloads = [(k, rs.cmd("DUMP", k)) for k in keys]
    df, dr = [], []
    for _ in range(max(5, TRIALS // 2)):
        dr.append(time_dump(rs, keys)); df.append(time_dump(fr, keys))   # encode
    rr2, rf2 = [], []
    for _ in range(max(5, TRIALS // 2)):
        rr2.append(time_restore(rs, payloads)); rf2.append(time_restore(fr, payloads))  # decode
    de = statistics.median(dr) / statistics.median(df)
    dd = statistics.median(rr2) / statistics.median(rf2)
    print("\nDUMP (encode half):")
    print(f"  fr median={statistics.median(df)*1000:.1f}ms cv={cv(df):.1f}%  "
          f"redis median={statistics.median(dr)*1000:.1f}ms cv={cv(dr):.1f}%  "
          f"ratio(redis/fr)={de:.3f}x  [{verdict(de)}]")
    print("RESTORE (decode half):")
    print(f"  fr median={statistics.median(rf2)*1000:.1f}ms cv={cv(rf2):.1f}%  "
          f"redis median={statistics.median(rr2)*1000:.1f}ms cv={cv(rr2):.1f}%  "
          f"ratio(redis/fr)={dd:.3f}x  [{verdict(dd)}]")


if __name__ == "__main__":
    main()
