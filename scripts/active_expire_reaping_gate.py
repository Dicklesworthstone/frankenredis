#!/usr/bin/env python3
"""active_expire_reaping_gate.py — active expiration reaps volatile keys without
access, matching redis 7.2.4. Guards frankenredis-bk7pi (092bd0453).

bk7pi added an O(1) fast-exit to run_active_expire_cycle when
store.count_expiring_keys() == 0, to skip the per-command cycle (and its clock
reads) on no-TTL workloads. The correctness-critical risk is a desync of that
counter: if it ever read 0 while volatile keys still need reaping, the fast-exit
would skip them and expired keys would LEAK (survive past their TTL without being
actively reaped). This gate seeds a mix of volatile (short-TTL) and persistent
keys, waits WITHOUT touching them (so only the active-expire cycle can remove
them — never lazy-on-access), and asserts fr reaps exactly the volatile keys, the
same as redis. It also checks the no-volatile-keys steady state.

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
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
        if t in (b"+", b"-"): return r.decode("latin1")
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


def scenario(c):
    """Returns (dbsize_after_reaping, persistent_keys_intact). Never reads the
    volatile keys, so only the active-expire cycle can remove them."""
    c.cmd("FLUSHALL")
    # 200 volatile keys @ 120ms, 50 persistent keys.
    for i in range(200):
        c.cmd("SET", f"vol:{i}", "x", "PX", "120")
    for i in range(50):
        c.cmd("SET", f"perm:{i}", "x")
    before = c.cmd("DBSIZE")
    # Drive the event loop with PINGs on a SEPARATE concern (no access to vol/perm
    # keys) while the active-expire cycle runs. Wait well past the TTL.
    deadline = time.time() + 2.0
    while time.time() < deadline:
        c.cmd("PING")
        time.sleep(0.02)
    after = c.cmd("DBSIZE")
    # Confirm persistent keys are all still present (without expiring them).
    perm_present = sum(1 for i in range(50) if c.cmd("EXISTS", f"perm:{i}") == 1)
    return before, after, perm_present


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

    rdir = tempfile.mkdtemp(prefix="fr_aereap_")
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
        f_before, f_after, f_perm = scenario(Conn(fp))
        r_before, r_after, r_perm = scenario(Conn(rp))
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    fails = []
    # Both must have started with 250 keys.
    if f_before != 250 or r_before != 250:
        fails.append(f"seed mismatch: fr_before={f_before} rd_before={r_before}")
    # After active reaping, only the 50 persistent keys remain — on BOTH.
    if f_after != r_after:
        fails.append(f"post-reap DBSIZE diverges: fr={f_after} rd={r_after}")
    if f_after != 50:
        fails.append(f"fr did not actively reap to 50 (got {f_after}) — possible "
                     f"count_expiring_keys desync / fast-exit leak (bk7pi)")
    if f_perm != 50 or r_perm != 50:
        fails.append(f"persistent keys disappeared: fr={f_perm} rd={r_perm}")

    for m in fails:
        print(f"  [FAIL] {m}")
    if fails:
        print(f"FAIL — active-expire reaping diverges from redis 7.2.4")
        return 1
    print("PASS — active expiration reaps volatile keys without access "
          f"(fr {f_before}->{f_after}, 50 persistent intact), matching redis 7.2.4")
    return 0


if __name__ == "__main__":
    sys.exit(main())
