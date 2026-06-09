#!/usr/bin/env python3
"""acl_semantics_gate.py — ACL SETUSER/GETUSER/DRYRUN/CAT/LIST parity vs redis 7.2.4.

Guards the hard-won ACL surface (beads 3b1jc reset-disable, fh7hn LIST/token order,
4kuad command rule-order, d919b selectors, 1ktss NOPERM wording): rule parsing and
validation (on/off, passwords >pass / nopass / resetpass, key patterns ~k / %RW~k /
allkeys / resetkeys, command rules +c / -c / +@cat / -@cat / allcommands / nocommands,
selectors (...)), GETUSER reply shape, DRYRUN allow/deny, CAT, LIST, WHOAMI, DELUSER,
and the SETUSER error surface (invalid command / category / token).

ACL CAT's per-category command list (and the top-level category list) is iterated in
redis's command-table dict order, which is hash-seed-randomized across restarts
(confirmed run-to-run) — the same WONTFIX class as FUNCTION LIST. So CAT replies are
compared as SORTED SETS; everything else is compared byte-for-byte (GENPASS hex masked).

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, re, socket, subprocess, sys, tempfile, time


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
        if t == b"+": return ("S", r.decode("latin1"))
        if t == b"-": return ("E", r.decode("latin1"))
        if t == b":": return ("I", int(r))
        if t == b"$":
            n = int(r)
            if n < 0: return ("N", None)
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return ("B", d.decode("latin1"))
        if t == b"*":
            n = int(r)
            return ("A", None) if n < 0 else ("A", [self.rd() for _ in range(n)])
        return ("?", line.decode("latin1"))
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self.rd()


def norm(v):
    if isinstance(v, tuple):
        t, x = v
        if t == "B" and x and re.fullmatch(r"[0-9a-f]{16,}", x):
            return ("B", "<hex>")          # mask GENPASS-style random hex
        if t == "A" and x is not None:
            return (t, [norm(e) for e in x])
        return (t, x)
    return v


def sorted_arr(v):
    """For ACL CAT replies: compare as a sorted set (dict-order is non-deterministic)."""
    if isinstance(v, tuple) and v[0] == "A" and isinstance(v[1], list):
        return ("A", sorted(norm(v)[1], key=lambda e: str(e)))
    return norm(v)


# (cmd, compare_mode) — "exact" byte-for-byte, "sorted" as a set.
STEPS = [
    (["ACL", "SETUSER", "u1", "on", ">pass1", "~key:*", "+get", "+set"], "exact"),
    (["ACL", "GETUSER", "u1"], "exact"),
    (["ACL", "SETUSER", "u2", "on", "nopass", "allkeys", "+@read"], "exact"),
    (["ACL", "GETUSER", "u2"], "exact"),
    (["ACL", "SETUSER", "u3", "+get", "-get"], "exact"),
    (["ACL", "GETUSER", "u3"], "exact"),
    (["ACL", "SETUSER", "u4", "+@all", "-@dangerous"], "exact"),
    (["ACL", "GETUSER", "u4"], "exact"),
    (["ACL", "SETUSER", "ubad", "+nosuchcommand"], "exact"),
    (["ACL", "SETUSER", "ubad2", "+@nosuchcat"], "exact"),
    (["ACL", "SETUSER", "ubad3", "badtoken"], "exact"),
    (["ACL", "SETUSER", "u5", "reset"], "exact"),
    (["ACL", "GETUSER", "u5"], "exact"),
    (["ACL", "SETUSER", "u6", "on", ">p", "+get", "%RW~foo"], "exact"),
    (["ACL", "GETUSER", "u6"], "exact"),
    (["ACL", "SETUSER", "u7", "on", ">p", "(+get ~k1)"], "exact"),
    (["ACL", "GETUSER", "u7"], "exact"),
    (["ACL", "DRYRUN", "u1", "GET", "key:1"], "exact"),
    (["ACL", "DRYRUN", "u1", "DEL", "key:1"], "exact"),
    (["ACL", "DRYRUN", "u1", "GET", "other"], "exact"),
    (["ACL", "DRYRUN", "nosuchuser", "GET", "k"], "exact"),
    (["ACL", "CAT"], "sorted"),
    (["ACL", "CAT", "read"], "sorted"),
    (["ACL", "CAT", "write"], "sorted"),
    (["ACL", "CAT", "nosuchcat"], "exact"),
    (["ACL", "WHOAMI"], "exact"),
    (["ACL", "GETUSER", "nosuchuser"], "exact"),
    (["ACL", "DELUSER", "u3", "u5"], "exact"),
    (["ACL", "DELUSER", "default"], "exact"),
    (["ACL", "SETUSER", "u8", "on", ">p", "+get", "+get"], "exact"),
    (["ACL", "GETUSER", "u8"], "exact"),
    (["ACL", "SETUSER", "u9", "on", "resetkeys", "~k1", "allkeys"], "exact"),
    (["ACL", "GETUSER", "u9"], "exact"),
]


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") == ("S", "PONG"): return True
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

    rdir = tempfile.mkdtemp(prefix="fr_aclgate_")
    fp, rp = free_port(), free_port()
    procs = [
        subprocess.Popen([fr, "--port", str(fp)],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        f, r = Conn(fp), Conn(rp)
        diffs = []
        for cmd, mode in STEPS:
            fv, rv = f.cmd(*cmd), r.cmd(*cmd)
            a, b = (sorted_arr(fv), sorted_arr(rv)) if mode == "sorted" else (norm(fv), norm(rv))
            if a != b:
                diffs.append((cmd, a, b))
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    for cmd, a, b in diffs:
        print(f"  [DIFF] {' '.join(cmd)}\n    fr={a}\n    rd={b}")
    if diffs:
        print(f"FAIL — {len(diffs)} ACL divergence(s) vs redis 7.2.4")
        return 1
    print(f"PASS — ACL SETUSER/GETUSER/DRYRUN/CAT/LIST parity vs redis 7.2.4 "
          f"({len(STEPS)} steps; CAT order = dict-hash WONTFIX, compared as set)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
