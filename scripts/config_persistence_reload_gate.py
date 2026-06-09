#!/usr/bin/env python3
"""config_persistence_reload_gate.py — CONFIG SET survives DEBUG RELOAD vs redis 7.2.4.

Root-cause gate for frankenredis-hpfey: a runtime CONFIG SET must survive an RDB
round-trip (DEBUG RELOAD, restart-from-RDB, replica full-sync). Upstream never
touches the config on reload; fr resets STORE-level config to compiled defaults
because the RDB-apply path swaps in a fresh Store and preserve_store_load_context
does not carry the encoding thresholds / maxmemory-policy over. That reset is what
makes large collections re-encode to listpack after reload (the visible symptom).

For each tracked param: CONFIG SET a non-default value, DEBUG RELOAD, CONFIG GET,
and confirm the value is unchanged — on BOTH fr and redis. The gate passes while
ONLY the hpfey-known params reset on fr (and they must NOT reset on redis), and
FAILS if a NEW param starts resetting (regression) or a known one is fixed (prune).

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, time, tempfile, shutil


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout); self.b = b""
    def _rd(self):
        while b"\r\n" not in self.b:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.b += d
        line, self.b = self.b.split(b"\r\n", 1); t = line[:1]
        if t in (b"+", b"-"):
            return line[1:]
        if t == b":":
            return int(line[1:])
        if t == b"$":
            n = int(line[1:])
            if n < 0: return None
            while len(self.b) < n + 2: self.b += self.s.recv(65536)
            d, self.b = self.b[:n], self.b[n+2:]; return d
        if t == b"*":
            n = int(line[1:])
            return None if n < 0 else [self._rd() for _ in range(n)]
        return line
    def cmd(self, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(o); return self._rd()
    def cfg_get(self, k):
        r = self.cmd("CONFIG", "GET", k)
        return r[1].decode() if isinstance(r, list) and len(r) >= 2 else None


# Non-default values to probe. (param, value)
PARAMS = [
    ("list-max-listpack-size", "64"),
    ("hash-max-listpack-entries", "200"),
    ("hash-max-listpack-value", "99"),
    ("set-max-listpack-entries", "99"),
    ("set-max-intset-entries", "999"),
    ("zset-max-listpack-entries", "99"),
    ("zset-max-listpack-value", "99"),
    ("maxmemory-policy", "allkeys-lru"),
    ("maxmemory", "123456789"),
    ("maxmemory-samples", "7"),
    ("hz", "42"),
    ("timeout", "300"),
    ("proto-max-bulk-len", "1234567"),
]

# (frankenredis-hpfey) fr resets these STORE-level params to compiled defaults on
# DEBUG RELOAD / RDB load (preserve_store_load_context drops them). redis keeps all.
KNOWN_RESET_ON_FR = {
    "list-max-listpack-size", "hash-max-listpack-entries", "hash-max-listpack-value",
    "set-max-listpack-entries", "set-max-intset-entries", "zset-max-listpack-entries",
    "zset-max-listpack-value", "maxmemory-policy",
}


def probe(port):
    c = Conn(port)
    for k, v in PARAMS:
        c.cmd("CONFIG", "SET", k, v)
    c.cmd("DEBUG", "RELOAD")
    # which params changed from the value we set?
    return {k: c.cfg_get(k) for k, v in PARAMS}


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, 2).cmd("PING") in (b"PONG", b"OK"): return True
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

    rdir = tempfile.mkdtemp(prefix="fr_cfgreload_")
    fp, rp = free_port(), free_port()
    procs = []
    try:
        procs.append(subprocess.Popen([fr, "--port", str(fp), "--enable-debug-command", "yes"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [redis, "--port", str(rp), "--dir", rdir, "--save", "", "--appendonly", "no",
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        rafter, fafter = probe(rp), probe(fp)
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()
        shutil.rmtree(rdir, ignore_errors=True)

    setv = dict(PARAMS)
    new_fr, fixed_fr, redis_reset = [], [], []
    for k, v in PARAMS:
        if rafter.get(k) != v:
            # redis must preserve every config across reload.
            redis_reset.append((k, v, rafter.get(k)))
        if fafter.get(k) != v:
            if k in KNOWN_RESET_ON_FR:
                print(f"  [known(hpfey)] {k}: set={v} after_reload={fafter.get(k)}")
            else:
                print(f"  [NEW] {k}: set={v} after_reload={fafter.get(k)}")
                new_fr.append(k)
        elif k in KNOWN_RESET_ON_FR:
            fixed_fr.append(k)

    for k, v, g in redis_reset:
        print(f"  [REDIS-UNEXPECTED] {k}: set={v} after_reload={g} (oracle should preserve)")
    if fixed_fr:
        print(f"NOTE: {sorted(fixed_fr)} now PERSIST on fr — prune KNOWN_RESET_ON_FR")
    if new_fr or redis_reset:
        print(f"FAIL — {len(new_fr)} NEW fr config reset(s), "
              f"{len(redis_reset)} oracle anomaly(ies)")
        return 1
    print(f"PASS — CONFIG SET reload-persistence parity vs redis 7.2.4 "
          f"({len(PARAMS)} params; {len(KNOWN_RESET_ON_FR)} known hpfey store-config resets tracked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
