#!/usr/bin/env python3
"""sort_semantics_gate.py — SORT/SORT_RO semantics parity vs redis 7.2.4.

Guards the rich SORT surface that the thin sort_differ doesn't pin down:
BY weight_* / hash_*->field / nosort, GET # / data_* / hash_*->field / missing,
LIMIT (incl. negative count and non-integer), ASC/DESC, ALPHA, numeric-parse
errors, STORE, missing key, and SORT_RO.

The redis oracle is launched with LC_ALL=C ON PURPOSE: redis SORT ... ALPHA
without STORE compares via strcoll/collateStringObjects under server.locale_collate
(default = the LC_COLLATE environment), while fr (and redis under C/POSIX, and any
SORT ... STORE) compares by byte order. Running the oracle in the C locale isolates
the deterministic SORT *algorithm* (which fr matches byte-for-byte) from the
locale-collation behavior, which is tracked separately as frankenredis-jaezc.

Every probed case uses DISTINCT sort keys so the total order is unambiguous
(redis SORT is not stable for equal weights). Self-launches a clean fr + redis
pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, tempfile, time


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout); self.b = b""
    def _l(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        line, self.b = self.b.split(b"\r\n", 1); return line
    def rd(self):
        line = self._l(); t, r = line[:1], line[1:]
        if t == b"+": return r.decode("latin1")
        if t == b"-": return "ERR:" + r.decode("latin1").split()[0]   # error CLASS
        if t == b":": return int(r)
        if t == b"$":
            n = int(r)
            if n < 0: return None
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return d.decode("latin1")
        if t == b"*":
            n = int(r)
            return None if n < 0 else [self.rd() for _ in range(n)]
        return line.decode("latin1")
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.rd()


def seed(c):
    for k in ("L", "NL", "Z", "D"):
        c.cmd("DEL", k)
    c.cmd("RPUSH", "L", "banana", "Apple", "cherry", "-2", "9x", "3.5", "0")
    c.cmd("RPUSH", "NL", "3", "-1", "10", "2", "-5", "100", "7")
    c.cmd("ZADD", "Z", "1", "30", "2", "10", "3", "20", "4", "5")
    weights = {"banana": 50, "Apple": 10, "cherry": 30, "-2": 40,
               "9x": 20, "3.5": 60, "0": 70}
    for m, w in weights.items():
        c.cmd("SET", f"w_{m}", str(w))
        c.cmd("SET", f"d_{m}", f"data-{m}")
        c.cmd("HSET", f"h_{m}", "fld", str(w + 1), "name", f"n-{m}")


CASES = [
    ["SORT", "NL"], ["SORT", "NL", "DESC"],
    ["SORT", "NL", "LIMIT", "2", "3"], ["SORT", "NL", "LIMIT", "1", "-1"],
    ["SORT", "L", "ALPHA"], ["SORT", "L", "ALPHA", "DESC"],
    ["SORT", "L", "ALPHA", "LIMIT", "0", "3"],
    ["SORT", "L"],                                  # numeric on non-numeric -> error
    ["SORT", "L", "BY", "w_*"], ["SORT", "L", "BY", "w_*", "DESC"],
    ["SORT", "L", "BY", "h_*->fld"],
    ["SORT", "L", "BY", "h_*->fld", "GET", "#", "GET", "d_*"],
    ["SORT", "L", "BY", "nosort"], ["SORT", "L", "BY", "nosort", "GET", "#"],
    ["SORT", "L", "BY", "w_*", "GET", "#", "GET", "h_*->name", "GET", "nope_*"],
    ["SORT", "Z", "BY", "w_*", "GET", "d_*"],
    ["SORT", "NL", "BY", "w_nope_*"],
    ["SORT", "NL", "LIMIT", "0", "100", "GET", "#"],
    ["SORT", "L", "ALPHA", "STORE", "D"], ["LRANGE", "D", "0", "-1"],
    ["SORT", "NL", "STORE", "D"], ["LRANGE", "D", "0", "-1"],
    ["SORT", "NL", "BY", "w_*", "STORE", "D"], ["LRANGE", "D", "0", "-1"],
    ["SORT", "MISSING"], ["SORT", "MISSING", "ALPHA"],
    ["SORT", "L", "LIMIT", "abc", "2"],
    ["SORT_RO", "NL"], ["SORT_RO", "L", "ALPHA"],
]


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == "PONG": return True
        except Exception: time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    args = ap.parse_args()
    fr = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr):
        print(f"SKIP: fr binary not found at {fr}"); return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_sortgate_")
    fp, rp = free_port(), free_port()
    # redis oracle in the C locale so SORT ALPHA collation == byte order (jaezc).
    cenv = dict(os.environ, LC_ALL="C", LC_COLLATE="C")
    procs = [
        subprocess.Popen([fr, "--port", str(fp), "--enable-debug-command", "yes"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no"], env=cenv,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        f, r = Conn(fp), Conn(rp)
        seed(f); seed(r)
        diffs = []
        for case in CASES:
            a = f.cmd(*case); b = r.cmd(*case)
            if a != b:
                diffs.append((case, a, b))
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    for case, a, b in diffs:
        print(f"  [DIFF] {' '.join(case)}\n    fr={a}\n    rd={b}")
    if diffs:
        print(f"FAIL — {len(diffs)} SORT divergence(s) vs redis 7.2.4 (C locale)")
        return 1
    print(f"PASS — SORT/SORT_RO semantics parity vs redis 7.2.4 "
          f"({len(CASES)} cases, C-locale oracle; collation tracked in jaezc)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
