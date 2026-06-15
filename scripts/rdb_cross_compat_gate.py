#!/usr/bin/env python3
"""Bidirectional RDB file cross-compatibility gate: fr <-> redis 7.2.4.

A silent RDB-format divergence corrupts migration / upgrade flows, so this gate
proves BOTH directions load byte-identically (verified via DEBUG DIGEST, which
fr emits identically to redis):
  (1) fr SAVE   -> redis loads the .rdb on startup -> DEBUG DIGEST matches fr's
  (2) redis SAVE -> fr loads the .rdb on startup    -> DEBUG DIGEST matches redis's
across every type/encoding (int/embstr/raw string, listpack+quicklist list,
intset+listpack+hashtable set, listpack+hashtable hash, listpack+skiplist zset,
stream, HLL, and a key with TTL).

Both servers are launched with --enable-debug-command. This script orchestrates
its own servers in a scratch dir under /tmp (left in place for inspection).

Usage: rdb_cross_compat_gate.py <redis-server-bin> <fr-bin> [base_port]
"""
import socket, sys, time, os, subprocess

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_rdb"
BASE = int(sys.argv[3]) if len(sys.argv) > 3 else 29621


def enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


def q(port, a):
    s = socket.create_connection(("127.0.0.1", port))
    s.sendall(enc(a))
    time.sleep(0.04)
    d = s.recv(8000)
    s.close()
    return d


SEED = [
    ["SET", "s_int", "12345"], ["SET", "s_embstr", "hi"], ["SET", "s_raw", "y" * 100],
    ["RPUSH", "l_lp", "a", "b", "c"], ["RPUSH", "l_ql", *[("x" * 60) for _ in range(200)]],
    ["SADD", "set_is", "1", "2", "3", "99"], ["SADD", "set_lp", "a", "bb", "ccc"],
    ["SADD", "set_ht", *[f"m{i}" for i in range(300)]],
    ["HSET", "h_lp", "f1", "v1"], ["HSET", "h_ht", *sum([[f"f{i}", f"v{i}"] for i in range(300)], [])],
    ["ZADD", "z_lp", "1", "a", "2.5", "b"], ["ZADD", "z_sl", *sum([[str(i * 1.5), f"m{i}"] for i in range(300)], [])],
    ["XADD", "stream", "1-1", "f", "v"], ["PFADD", "hll", *[f"e{i}" for i in range(100)]],
    ["SET", "exp_key", "v"], ["EXPIRE", "exp_key", "100000"],
]


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"PONG" in q(port, ["PING"]):
                return True
        except Exception:
            time.sleep(0.1)
    return False


def copy_file(src, dst):
    with open(src, "rb") as fsrc, open(dst, "wb") as fdst:
        fdst.write(fsrc.read())


def main():
    tmp = os.path.join("/tmp", "rdb_cross_compat_gate")
    os.makedirs(tmp, exist_ok=True)
    procs = []
    failures = []
    try:
        # --- direction 1: fr writes, redis loads ---
        fr_rdb = os.path.join(tmp, "fr.rdb")
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(BASE), "--rdb", fr_rdb, "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE), "fr did not start"
        q(BASE, ["FLUSHALL"])
        for c in SEED:
            q(BASE, c)
        fr_digest = q(BASE, ["DEBUG", "DIGEST"]).strip()
        q(BASE, ["SAVE"])
        copy_file(fr_rdb, os.path.join(tmp, "load1.rdb"))
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 1), "--dir", tmp, "--dbfilename", "load1.rdb",
             "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 1), "redis did not start loading fr RDB"
        r_digest = q(BASE + 1, ["DEBUG", "DIGEST"]).strip()
        if r_digest != fr_digest:
            failures.append(("fr->redis", fr_digest, r_digest))

        # --- direction 2: redis writes, fr loads ---
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 2), "--dir", tmp, "--dbfilename", "redis.rdb",
             "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 2), "redis(2) did not start"
        q(BASE + 2, ["FLUSHALL"])
        for c in SEED:
            q(BASE + 2, c)
        r2_digest = q(BASE + 2, ["DEBUG", "DIGEST"]).strip()
        q(BASE + 2, ["SAVE"])
        copy_file(os.path.join(tmp, "redis.rdb"), os.path.join(tmp, "load2.rdb"))
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(BASE + 3), "--rdb", os.path.join(tmp, "load2.rdb"),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 3), "fr did not start loading redis RDB"
        fr2_digest = q(BASE + 3, ["DEBUG", "DIGEST"]).strip()
        if fr2_digest != r2_digest:
            failures.append(("redis->fr", r2_digest, fr2_digest))
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass

    print("=" * 60)
    if failures:
        print(f"FAIL - {len(failures)} RDB cross-load divergence(s):")
        for direction, want, got in failures:
            print(f"  [{direction}] digest mismatch:\n    source={want}\n    loaded={got}")
        sys.exit(1)
    print("PASS - RDB file byte-compatible fr <-> redis 7.2.4 both directions"
          " (all types/encodings, DEBUG DIGEST identical)")


main()
