#!/usr/bin/env python3
"""replication_convergence_gate.py — cross-implementation replication convergence
vs vendored redis 7.2.4.

A parity dimension the single-server differs do not cover: that the ENTIRE
replication stack (PSYNC handshake, RDB full-sync transfer, and the incremental
command-propagation stream) makes a follower converge byte-exact to its leader —
in BOTH directions:

  Topology A:  fr master   -> redis replica   (exercises fr's master side:
               propagation rewrites, RDB generation, backlog/+CONTINUE)
  Topology B:  redis master -> fr replica     (exercises fr's replica side:
               redis-RDB load, applying redis's deterministic effect stream)

Three workloads run on each topology:
  * convergence — diverse data-type matrix (strings/lists/sets/hashes/zsets/
    streams, all encodings, expiries, COPY/RENAME/SPOP/GETDEL/LMPOP rewrites)
  * hard cases  — EVAL effect propagation, blocking-pop served->non-blocking,
    INCRBYFLOAT/HINCRBYFLOAT determinism, MULTI/EXEC, expiry timing (replica
    must not expire early), and live partial-resync after a REPLICAOF NO ONE bounce
  * determinism fuzz — long seeded stream of NON-DETERMINISTIC writes (SPOP,
    SRANDMEMBER/HRANDFIELD/ZRANDMEMBER count, ZPOPMIN, random TTLs, EVAL) that
    must propagate as deterministic effects

Self-launches both servers on free ports. Hard gate: exits non-zero on any
convergence divergence.

Usage: replication_convergence_gate.py [--bin FR] [--redis-bin REDIS] [--seeds N]
"""
import argparse, os, socket, subprocess, sys, time, random, shutil, tempfile


class Conn:
    def __init__(self, port, timeout=8):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.s.settimeout(timeout); self.buf = b""
    def _l(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.buf += d
        l, self.buf = self.buf.split(b"\r\n", 1); return l
    def _n(self, n):
        while len(self.buf) < n + 2:
            d = self.s.recv(65536)
            if not d: raise OSError("closed")
            self.buf += d
        d, self.buf = self.buf[:n], self.buf[n+2:]; return d
    def parse(self):
        l = self._l(); t, r = l[:1], l[1:]
        if t == b"$":
            n = int(r); return None if n < 0 else self._n(n)
        if t == b":": return int(r)
        if t in (b"+", b"-"): return r
        if t == b"*":
            n = int(r); return None if n < 0 else [self.parse() for _ in range(n)]
        return l
    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        try: return self.parse()
        except Exception: return None


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            if Conn(port, timeout=2).cmd("PING") in (b"PONG", b"OK"):
                return True
        except Exception:
            time.sleep(0.2)
    return False


def wait_link(r, mport, tries=50):
    r.cmd("REPLICAOF", "127.0.0.1", str(mport))
    for _ in range(tries):
        if b"master_link_status:up" in (r.cmd("INFO", "replication") or b""):
            return True
        time.sleep(0.25)
    return False


def snapshot(c):
    keys = sorted(x.decode("latin1") for x in (c.cmd("KEYS", "*") or []))
    snap = {}
    for k in keys:
        kb = k.encode("latin1")
        t = c.cmd("TYPE", kb); t = t.decode() if isinstance(t, bytes) else str(t)
        if t == "string": v = c.cmd("GET", kb)
        elif t == "list": v = c.cmd("LRANGE", kb, "0", "-1")
        elif t == "set": v = sorted(c.cmd("SMEMBERS", kb) or [])
        elif t == "hash":
            raw = c.cmd("HGETALL", kb) or []
            v = sorted((raw[i], raw[i+1]) for i in range(0, len(raw), 2))
        elif t == "zset": v = c.cmd("ZRANGE", kb, "0", "-1", "WITHSCORES")
        elif t == "stream": v = (c.cmd("XLEN", kb), c.cmd("XRANGE", kb, "-", "+"))
        else: v = c.cmd("DUMP", kb)
        snap[k] = (t, repr(v))
    return snap


def diff_snaps(ms, rs):
    out = []
    for k in sorted(set(ms) | set(rs)):
        if ms.get(k) != rs.get(k):
            out.append((k, ms.get(k), rs.get(k)))
    return out


def wl_convergence(m):
    m.cmd("FLUSHALL")
    m.cmd("SET", "s:int", "12345"); m.cmd("SET", "s:raw", "x" * 100)
    m.cmd("APPEND", "s:app", "abc"); m.cmd("APPEND", "s:app", "def")
    m.cmd("INCR", "ctr"); m.cmd("INCRBYFLOAT", "f", "3.14")
    m.cmd("SETBIT", "bm", "100", "1")
    m.cmd("RPUSH", "l:small", "a", "b", "c")
    for i in range(200): m.cmd("RPUSH", "l:big", f"item{i}")
    m.cmd("SADD", "set:int", "1", "2", "3", "100", "99999")
    for i in range(200): m.cmd("SADD", "set:big", f"m{i}")
    m.cmd("HSET", "h:small", "f1", "v1", "f2", "v2")
    for i in range(200): m.cmd("HSET", "h:big", f"field{i}", f"val{i}")
    m.cmd("ZADD", "z:small", "1", "a", "2.5", "b", "3", "c")
    for i in range(200): m.cmd("ZADD", "z:big", str(i * 1.5), f"e{i}")
    m.cmd("XADD", "stream", "1-1", "f", "1"); m.cmd("XADD", "stream", "2-1", "f", "2")
    m.cmd("XGROUP", "CREATE", "stream", "g1", "0"); m.cmd("XADD", "stream", "3-1", "f", "3")
    m.cmd("SET", "exp:s", "v", "EX", "5000")
    m.cmd("SADD", "sp", "a", "b", "c", "d", "e"); m.cmd("SPOP", "sp", "2")
    m.cmd("SET", "gd", "x"); m.cmd("GETDEL", "gd")
    m.cmd("RPUSH", "mp", "1", "2", "3"); m.cmd("LMPOP", "2", "nope", "mp", "LEFT")
    m.cmd("COPY", "s:int", "s:int:copy"); m.cmd("RENAME", "s:raw", "s:raw:renamed")


def wl_hard(m, r, mport):
    fails = []
    m.cmd("EVAL", "redis.call('set',KEYS[1],ARGV[1]); redis.call('rpush',KEYS[2],'a','b'); return 1",
          "2", "sc:str", "sc:list", "scripted")
    m.cmd("EVAL", "for i=1,50 do redis.call('sadd',KEYS[1],i) end return 1", "1", "sc:set")
    m.cmd("RPUSH", "bl:list", "x", "y", "z"); m.cmd("BLPOP", "bl:list", "0")
    m.cmd("ZADD", "bl:zset", "1", "a", "2", "b"); m.cmd("BZPOPMIN", "bl:zset", "0")
    m.cmd("SET", "fl", "10"); m.cmd("INCRBYFLOAT", "fl", "0.1")
    m.cmd("HSET", "hfl", "f", "10"); m.cmd("HINCRBYFLOAT", "hfl", "f", "0.1")
    m.cmd("MULTI"); m.cmd("SET", "tx:a", "1"); m.cmd("INCR", "tx:a"); m.cmd("RPUSH", "tx:b", "q"); m.cmd("EXEC")
    time.sleep(1.5)
    try: m.cmd("WAIT", "1", "3000")
    except Exception: pass
    time.sleep(0.4)
    keys = ["sc:str", "sc:list", "sc:set", "bl:list", "bl:zset", "fl", "hfl", "tx:a", "tx:b"]
    ms = {k: snapshot(m).get(k) for k in keys}; rs = {k: snapshot(r).get(k) for k in keys}
    for k in keys:
        if ms[k] != rs[k]: fails.append((f"hard/{k}", ms[k], rs[k]))
    # expiry timing
    m.cmd("SET", "exp:k", "v", "PX", "700"); time.sleep(0.2)
    if r.cmd("GET", "exp:k") != b"v": fails.append(("hard/expiry-early", b"v", r.cmd("GET", "exp:k")))
    time.sleep(1.6)
    if (m.cmd("GET", "exp:k"), r.cmd("GET", "exp:k")) != (None, None):
        fails.append(("hard/expiry-converge", m.cmd("GET", "exp:k"), r.cmd("GET", "exp:k")))
    # live partial-resync
    r.cmd("REPLICAOF", "NO", "ONE")
    for i in range(20): m.cmd("SET", f"pr:{i}", i)
    if not wait_link(r, mport): fails.append(("hard/reattach-link", "up", "down"))
    else:
        time.sleep(1.2)
        try: m.cmd("WAIT", "1", "3000")
        except Exception: pass
        time.sleep(0.3)
        miss = [f"pr:{i}" for i in range(20) if r.cmd("GET", f"pr:{i}") != str(i).encode()]
        if miss: fails.append((f"hard/reattach-delta ({len(miss)})", "all", miss[:5]))
    return fails


SK = [f"k{i}" for i in range(12)]

def rnd_cmd(rng):
    k = rng.choice(SK); k2 = rng.choice(SK)
    members = [str(rng.randint(0, 30)) for _ in range(rng.randint(1, 6))]
    table = [
        ("SET", k, str(rng.randint(0, 1000))),
        ("SET", k, str(rng.randint(0, 1000)), "PX", str(rng.randint(200, 100000))),
        ("INCRBYFLOAT", k, f"{rng.uniform(-5,5):.3f}"),
        ("SADD", k, *members),
        ("SPOP", k, str(rng.randint(1, 4))),
        ("SMOVE", k, k2, members[0]),
        ("HSET", k, *sum(([f"f{m}", str(rng.randint(0,99))] for m in members[:3]), [])),
        ("ZADD", k, *sum(([str(rng.randint(0,50)), f"m{m}"] for m in members[:3]), [])),
        ("ZADD", k, "GT", "CH", str(rng.randint(0,50)), f"m{members[0]}"),
        ("ZPOPMIN", k, str(rng.randint(1, 3))),
        ("RPUSH", k, *members),
        ("LPOP", k, str(rng.randint(1, 3))),
        ("LMPOP", "2", k, k2, rng.choice(["LEFT", "RIGHT"])),
        ("GETEX", k, "PX", str(rng.randint(200, 100000))),
        ("GETDEL", k),
        ("COPY", k, k2, "REPLACE"),
        ("EXPIRE", k, str(rng.randint(1, 100000))),
        ("PERSIST", k),
        ("DEL", k),
        ("EVAL", f"redis.call('set', KEYS[1], '{rng.randint(0,99)}'); return 1", "1", k),
    ]
    return rng.choice(table)


def wl_fuzz(m, seed, iters):
    rng = random.Random(seed)
    m.cmd("FLUSHALL")
    for _ in range(iters): m.cmd(*rnd_cmd(rng))


def converge_check(m, r, label, fails):
    time.sleep(1.5)
    try: m.cmd("WAIT", "1", "4000")
    except Exception: pass
    time.sleep(0.5)
    d = diff_snaps(snapshot(m), snapshot(r))
    for k, a, b in d[:20]:
        print(f"    [{label}] {k}\n        master={a}\n        replica={b}")
    if d: fails.append((label, f"{len(d)} key divergence(s)", ""))


def run_topology(name, master_cmd, replica_cmd, mport, rport, seeds):
    print(f"== Topology {name} ==")
    procs = []
    fails = []
    try:
        procs.append(subprocess.Popen(master_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(replica_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not (wait_up(mport) and wait_up(rport)):
            return [(f"{name}/launch", "server did not start", "")]
        m, r = Conn(mport), Conn(rport)
        m.cmd("FLUSHALL")
        if not wait_link(r, mport):
            return [(f"{name}/link", "replica link never up", "")]
        # convergence
        wl_convergence(m); converge_check(m, r, f"{name}/convergence", fails)
        # hard cases
        fails.extend((f"{name}/{w}", a, b) for (w, a, b) in wl_hard(m, r, mport))
        # determinism fuzz
        for seed in range(1, seeds + 1):
            wl_fuzz(m, seed, 1200); converge_check(m, r, f"{name}/fuzz-seed{seed}", fails)
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()
    return fails


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    ap.add_argument("--seeds", type=int, default=2)
    args = ap.parse_args()
    fr = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr):
        print(f"SKIP: fr binary not found at {fr} (build with: cargo build --release -p fr-server)")
        return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_repl_gate_")
    os.makedirs(os.path.join(rdir, "a_r"), exist_ok=True)
    os.makedirs(os.path.join(rdir, "b_m"), exist_ok=True)
    a_m, a_r = free_port(), free_port()
    b_m, b_r = free_port(), free_port()
    all_fails = []
    try:
        # Topology A: fr master, redis replica
        all_fails += run_topology(
            "A:fr-master->redis-replica",
            [fr, "--port", str(a_m), "--enable-debug-command", "yes"],
            [redis, "--port", str(a_r), "--dir", os.path.join(rdir, "a_r"),
             "--save", "", "--appendonly", "no", "--enable-debug-command", "yes"],
            a_m, a_r, args.seeds)
        # Topology B: redis master, fr replica
        all_fails += run_topology(
            "B:redis-master->fr-replica",
            [redis, "--port", str(b_m), "--dir", os.path.join(rdir, "b_m"),
             "--save", "", "--appendonly", "no", "--enable-debug-command", "yes"],
            [fr, "--port", str(b_r), "--enable-debug-command", "yes"],
            b_m, b_r, args.seeds)
    finally:
        shutil.rmtree(rdir, ignore_errors=True)

    for label, a, b in all_fails:
        print(f"  [FAIL] {label}: {a} {b}")
    if all_fails:
        print(f"FAIL — {len(all_fails)} cross-impl replication convergence divergence(s)")
        return 1
    print("PASS — fr<->redis 7.2.4 replication converges byte-exact "
          "(both topologies: convergence + hard cases + determinism fuzz)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
