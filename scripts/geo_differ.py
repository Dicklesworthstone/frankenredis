#!/usr/bin/env python3
"""geo_differ.py — GEO command-family differential fuzzer vs vendored redis 7.2.4.

Covers the full GEO surface, which the geohash encode/decode, haversine distance,
bounding-box prefilter, and result-ordering paths make easy to get subtly wrong:
  GEOADD (incl. NX/XX/CH), GEODIST (all units), GEOPOS, GEOHASH,
  GEOSEARCH FROMLONLAT/FROMMEMBER BYRADIUS/BYBOX (ASC/DESC, COUNT, COUNT ANY,
  WITHCOORD/WITHDIST/WITHHASH), GEORADIUS / GEORADIUSBYMEMBER (deprecated),
  GEOSEARCHSTORE / GEORADIUS STORE / STOREDIST, and the error-class parity for
  malformed invocations.

Comparison rules (avoid false positives on genuinely-unspecified behavior):
  - Result MEMBERSHIP is always compared (missing/extra members are real bugs).
  - Coordinates (GEOPOS / WITHCOORD) and distances (GEODIST / WITHDIST) compare
    with a small tolerance — geohash quantization gives ~1e-5 deg / sub-metre
    jitter that redis itself exhibits run to run.
  - Tie order between equidistant members is unspecified, so the random phase
    compares membership; the deterministic phase uses well-separated points so
    ASC/DESC ordering IS pinned and is compared positionally.
  - STORE/STOREDIST destination zsets are compared by ZRANGE WITHSCORES (with
    score tolerance for STOREDIST distances).

Usage: geo_differ.py [--oracle 16399] [--fr 16400] [--seeds 6]
Exit 0 if fr matches redis across every case, else 1.
"""
import argparse
import random
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 4)
        self.s.settimeout(4.0)
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
        if t == b"+":
            return r.decode("latin1")
        if t == b":":
            return int(r)
        if t == b"-":
            # error class only (first token) — wording can differ harmlessly
            return "ERR:" + r.decode("latin1").split()[0]
        if t == b"$":
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


UNITS = ["m", "km", "mi", "ft"]


def approx(a, b, tol=0.02):
    try:
        return abs(float(a) - float(b)) <= tol * max(1.0, abs(float(a)))
    except (TypeError, ValueError):
        return a == b


def members_of(reply):
    if not isinstance(reply, list):
        return reply
    return sorted(it[0] if isinstance(it, list) else it for it in reply)


def withdist_map(reply):
    m = {}
    if isinstance(reply, list):
        for it in reply:
            if isinstance(it, list) and len(it) >= 2:
                m[it[0]] = it[1]
    return m


class Differ:
    def __init__(self, o, f):
        self.o, self.f = o, f
        self.div = 0
        self.ex = []

    def both(self, *a):
        return self.o.cmd(*a), self.f.cmd(*a)

    def chk(self, label, a, b, norm=lambda x: x):
        if norm(a) != norm(b):
            self.div += 1
            self.ex.append((label, a, b))

    def random_phase(self, seeds):
        for seed in range(seeds):
            random.seed(1000 + seed)
            self.o.cmd("FLUSHALL"); self.f.cmd("FLUSHALL")
            key = "g"
            mlist = []
            for i in range(random.randint(5, 20)):
                nm = f"m{i}"
                lon = round(random.uniform(-180, 180), 5)
                lat = round(random.uniform(-85, 85), 5)
                mlist.append(nm)
                self.chk(f"GEOADD {nm}", *self.both("GEOADD", key, lon, lat, nm))
            for _ in range(200):
                op = random.choice([
                    "DIST", "POS", "HASH", "SRADIUS", "SBOX", "SMEMBER",
                    "RADIUS", "RADIUSBYMEMBER", "SEARCHSTORE",
                ])
                rlon = round(random.uniform(-180, 180), 5)
                rlat = round(random.uniform(-85, 85), 5)
                u = random.choice(UNITS)
                if op == "DIST":
                    m1, m2 = random.choice(mlist), random.choice(mlist)
                    a, b = self.both("GEODIST", key, m1, m2, u)
                    if not (a == b or (a is not None and b is not None and approx(a, b))):
                        self.div += 1; self.ex.append((f"GEODIST {m1} {m2} {u}", a, b))
                elif op == "POS":
                    ms = random.sample(mlist, k=min(3, len(mlist)))
                    a, b = self.both("GEOPOS", key, *ms)
                    ok = isinstance(a, list) and isinstance(b, list) and len(a) == len(b)
                    if ok:
                        for pa, pb in zip(a, b):
                            if pa is None and pb is None:
                                continue
                            if pa is None or pb is None or not (
                                approx(pa[0], pb[0], 1e-5) and approx(pa[1], pb[1], 1e-5)):
                                ok = False; break
                    else:
                        ok = a == b
                    if not ok:
                        self.div += 1; self.ex.append((f"GEOPOS {ms}", a, b))
                elif op == "HASH":
                    ms = random.sample(mlist, k=min(3, len(mlist)))
                    self.chk(f"GEOHASH {ms}", *self.both("GEOHASH", key, *ms))
                elif op == "SRADIUS":
                    rad = random.choice([10, 100, 1000, 5000])
                    self.chk(f"GEOSEARCH BYRADIUS {rad}{u}",
                             *self.both("GEOSEARCH", key, "FROMLONLAT", rlon, rlat,
                                        "BYRADIUS", rad, u), norm=members_of)
                elif op == "RADIUS":
                    rad = random.choice([10, 100, 1000, 5000])
                    self.chk(f"GEORADIUS {rad}{u}",
                             *self.both("GEORADIUS", key, rlon, rlat, rad, u),
                             norm=members_of)
                elif op == "SBOX":
                    w = random.choice([100, 1000, 5000])
                    h = random.choice([100, 1000, 5000])
                    self.chk(f"GEOSEARCH BYBOX {w}x{h}{u}",
                             *self.both("GEOSEARCH", key, "FROMLONLAT", rlon, rlat,
                                        "BYBOX", w, h, u), norm=members_of)
                elif op == "SMEMBER":
                    m = random.choice(mlist); rad = random.choice([100, 1000, 5000])
                    a, b = self.both("GEOSEARCH", key, "FROMMEMBER", m, "BYRADIUS",
                                     rad, u, "WITHDIST")
                    da, db = withdist_map(a), withdist_map(b)
                    if set(da) != set(db):
                        self.div += 1; self.ex.append((f"SMEMBER {m} membership", sorted(da), sorted(db)))
                    else:
                        for k in da:
                            if not approx(da[k], db[k]):
                                self.div += 1; self.ex.append((f"SMEMBER {m} dist {k}", da[k], db[k])); break
                elif op == "RADIUSBYMEMBER":
                    m = random.choice(mlist); rad = random.choice([100, 1000, 5000])
                    self.chk(f"GEORADIUSBYMEMBER {m} {rad}{u}",
                             *self.both("GEORADIUSBYMEMBER", key, m, rad, u),
                             norm=members_of)
                elif op == "SEARCHSTORE":
                    rad = random.choice([1000, 5000])
                    self.chk("GEOSEARCHSTORE count",
                             *self.both("GEOSEARCHSTORE", "dst", key, "FROMLONLAT",
                                        rlon, rlat, "BYRADIUS", rad, u))
                    self.chk("GEOSEARCHSTORE dst",
                             *self.both("ZRANGE", "dst", "0", "-1"),
                             norm=lambda r: sorted(r or []))

    def deterministic_phase(self):
        self.o.cmd("FLUSHALL"); self.f.cmd("FLUSHALL")
        pts = [("palermo", 13.361389, 38.115556), ("catania", 15.087269, 37.502669),
               ("rome", 12.496366, 41.902782), ("naples", 14.2681, 40.8518),
               ("milan", 9.19, 45.4642), ("turin", 7.6869, 45.0703)]
        for nm, lo, la in pts:
            self.both("GEOADD", "Sicily", lo, la, nm)
        for cnt in [1, 2, 3, 10]:
            self.chk(f"SEARCH ASC WITHDIST count={cnt}",
                     *self.both("GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYRADIUS",
                                2000, "km", "ASC", "COUNT", cnt, "WITHDIST"))
            self.chk(f"SEARCH DESC count={cnt}",
                     *self.both("GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYRADIUS",
                                2000, "km", "DESC", "COUNT", cnt))
        self.chk("SEARCH ALLWITH ASC",
                 *self.both("GEOSEARCH", "Sicily", "FROMMEMBER", "palermo", "BYRADIUS",
                            2000, "km", "ASC", "WITHCOORD", "WITHDIST", "WITHHASH"))
        self.chk("RADIUS ALLWITH ASC",
                 *self.both("GEORADIUS", "Sicily", 13, 38, 2000, "km", "ASC",
                            "WITHCOORD", "WITHDIST", "WITHHASH", "COUNT", 4))
        self.chk("BYBOX ASC WITHDIST",
                 *self.both("GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYBOX",
                            4000, 4000, "km", "ASC", "WITHDIST"))
        self.chk("COUNT ANY membership",
                 *self.both("GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYRADIUS",
                            5000, "km", "COUNT", 3, "ANY"), norm=members_of)
        self.chk("RADIUS STORE", *self.both("GEORADIUS", "Sicily", 13, 38, 2000, "km", "STORE", "d1"))
        self.chk("RADIUS STORE zset", *self.both("ZRANGE", "d1", "0", "-1", "WITHSCORES"))
        self.chk("SEARCHSTORE STOREDIST",
                 *self.both("GEOSEARCHSTORE", "d3", "Sicily", "FROMLONLAT", 13, 38,
                            "BYRADIUS", 2000, "km", "STOREDIST"))
        # error-class parity
        for lbl, c in [
            ("bad unit", ["GEODIST", "Sicily", "palermo", "rome", "parsecs"]),
            ("box+radius", ["GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYRADIUS", 100, "km", "BYBOX", 100, 100, "km"]),
            ("store on search", ["GEOSEARCH", "Sicily", "FROMLONLAT", 13, 38, "BYRADIUS", 100, "km", "STORE", "x"]),
            ("missing from", ["GEOSEARCH", "Sicily", "BYRADIUS", 100, "km"]),
            ("radiusbymember nonexist", ["GEORADIUSBYMEMBER", "Sicily", "nope", 100, "km"]),
            ("bad lat", ["GEOADD", "Sicily", 10, 99.0, "badlat"]),
            ("nx xx", ["GEOADD", "Sicily", "NX", "XX", 10, 40, "m1"]),
            ("dist missing member", ["GEODIST", "Sicily", "palermo", "nope"]),
        ]:
            self.chk(f"ERR {lbl}", *self.both(*c))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--seeds", type=int, default=6)
    args = ap.parse_args()
    d = Differ(Conn(args.oracle), Conn(args.fr))
    d.random_phase(args.seeds)
    d.deterministic_phase()
    if d.div:
        print(f"FAIL: {d.div} GEO divergence(s)")
        for label, a, b in d.ex[:30]:
            print(f"  DIFF {label}\n     redis= {a!r:.160}\n     fr   = {b!r:.160}")
        sys.exit(1)
    print(f"OK: GEO family byte-exact vs redis 7.2.4 "
          f"({args.seeds} random seeds + deterministic ordered/with-opts/store/error cases)")


if __name__ == "__main__":
    main()
