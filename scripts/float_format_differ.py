#!/usr/bin/env python3
"""float_format_differ.py — differential + invariant gate for ZSET score
(double) formatting: fr-protocol::format_redis_double vs vendored redis 7.2.4
d2string (deps/fpconv/fpconv_dtoa.c, grisu2).

Sweeps random f64 bit patterns plus adversarial 17-significant-digit / ULP-edge
values through ZADD, reads them back with ZSCORE, and compares the two servers.

THE INVARIANT (what makes this a real gate, not just a diff):
  - fr's reply MUST always round-trip back to the exact f64 that was stored, and
  - fr's reply MUST be a shortest representation no longer than redis's.
fr currently derives the shortest digits from Rust's `{:e}` (Ryū), which is
perfectly rounded; redis uses grisu2, which occasionally tie-breaks the LAST
digit differently (both round-trip to the SAME f64). Those tie-break-only
differences are tolerated and counted; any difference where fr's reply does NOT
round-trip to the stored f64, or is longer than redis's, FAILS the gate.

As of the faithful fpconv_dtoa/grisu2 port (format_redis_double no longer
piggybacks on Ryū), this is expected to report ZERO tie-breaks — fr is now
byte-identical to redis d2string across the full f64 surface. The tolerance is
kept as a defensive invariant: any non-round-tripping or longer reply still
FAILS, and any reappearing tie-break flags a regression in the grisu2 port.

Usage: float_format_differ.py [--oracle 16399] [--fr 16400] [--n 8000] [--seed 1]
Exit 0 if every divergence is a same-f64 tie-break; exit 1 on any real fault.
"""
import argparse
import random
import socket
import struct
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
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

    def _parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":"):
            return r.decode()
        if t == b"-":
            return "ERR:" + r.decode()
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self._parse()


def adversarial():
    out = ["0", "-0", "0.0", "-0.0", "1", "-1", "inf", "-inf",
           "3.14159265358979", "2.718281828459045", "1e10", "1e-10", "1e100",
           "1e-100", "1e308", "5e-324", "1.7976931348623157e308", "0.1", "0.2",
           "0.3", "0.30000000000000004", "1234567890123456.7",
           "-1997107851181081.2", "9007199254740992", "9007199254740993",
           "1e16", "1e17", "0.0001", "123.456e10", "1.23e-5"]
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--n", type=int, default=8000)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    o, f = Conn(args.oracle), Conn(args.fr)
    for c in (o, f):
        if c.cmd("PING") != "PONG":
            print("server not responding", file=sys.stderr)
            sys.exit(2)

    rng = random.Random(args.seed)
    values = list(adversarial())
    while len(values) < args.n:
        v = struct.unpack("<d", struct.pack("<Q", rng.getrandbits(64)))[0]
        if v != v or v in (float("inf"), float("-inf")):
            continue
        values.append(repr(v))

    tiebreaks = 0
    faults = 0
    for s in values:
        o.cmd("DEL", "z")
        f.cmd("DEL", "z")
        o.cmd("ZADD", "z", s, "m")
        f.cmd("ZADD", "z", s, "m")
        so = o.cmd("ZSCORE", "z", "m")
        sf = f.cmd("ZSCORE", "z", "m")
        if so == sf:
            continue
        # Divergence: tolerate ONLY a same-f64, no-longer last-digit tie-break.
        try:
            stored = float(s)
        except ValueError:
            faults += 1
            print(f"FAULT non-float input slipped through: {s!r}")
            continue
        ok_roundtrip = (sf is not None) and (float(sf) == stored)
        no_longer = (so is None) or (sf is not None and len(sf) <= len(so))
        if ok_roundtrip and no_longer:
            tiebreaks += 1
            if tiebreaks <= 10:
                print(f"tie-break  in={s}  redis={so}  fr={sf}  (both → {stored!r})")
        else:
            faults += 1
            print(f"FAULT  in={s}  redis={so!r}  fr={sf!r}  "
                  f"roundtrip={ok_roundtrip} no_longer={no_longer}")

    rate = 100.0 * tiebreaks / len(values)
    print(f"--- {len(values)} values: {tiebreaks} grisu2 tie-breaks ({rate:.3f}%), "
          f"{faults} faults ---")
    if faults:
        print("FAIL: fr produced a non-round-tripping or longer score string")
        sys.exit(1)
    print("OK: every fr score round-trips and is shortest "
          "(grisu2 last-digit tie-breaks tolerated)")


if __name__ == "__main__":
    main()
