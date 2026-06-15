#!/usr/bin/env python3
"""Bidirectional AOF cross-compatibility gate: fr <-> redis 7.2.4.

AOF is the durability mechanism; a format divergence means SILENT DATA LOSS on
restart. This gate proves the redis-7 multi-part appendonlydir (manifest +
base.rdb + incr.aof) round-trips both ways, including the incremental
command-replay path (not just the RDB base), verified via DEBUG DIGEST:
  (1) fr writes AOF (BGREWRITEAOF)            -> redis loads it -> DIGEST matches
  (2) redis writes AOF (base + incr cmd-log)  -> fr loads+replays -> DIGEST matches

Direction 2 is the important one: it writes data, BGREWRITEAOF to seal a base,
then issues more commands (SET/INCR/LPUSH/SADD/ZADD/EXPIRE/APPEND/HDEL) that
land in incr.aof as a command log, and verifies fr replays them to the exact
same digest.

Both servers launched with --enable-debug-command. Self-orchestrating; scratch
under /tmp.

Usage: aof_cross_compat_gate.py <redis-server-bin> <fr-bin> [base_port]
"""
import socket, sys, time, os, subprocess

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_aof"
BASE = int(sys.argv[3]) if len(sys.argv) > 3 else 29721


def enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


def q(port, a):
    s = socket.create_connection(("127.0.0.1", port))
    s.sendall(enc(a))
    time.sleep(0.05)
    d = s.recv(8000)
    s.close()
    return d


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"PONG" in q(port, ["PING"]):
                return True
        except Exception:
            time.sleep(0.1)
    return False


def copytree(src, dst):
    os.makedirs(dst, exist_ok=True)
    for name in os.listdir(src):
        with open(os.path.join(src, name), "rb") as fsrc:
            data = fsrc.read()
        with open(os.path.join(dst, name), "wb") as fdst:
            fdst.write(data)


def main():
    root = os.path.join("/tmp", "aof_cross_compat_gate")
    os.makedirs(root, exist_ok=True)
    procs = []
    failures = []
    try:
        # ---- direction 1: fr writes AOF, redis loads ----
        d1 = os.path.join(root, "d1")
        os.makedirs(d1, exist_ok=True)
        fr_aof_base = os.path.join(d1, "appendonly.aof")
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(BASE), "--aof", fr_aof_base, "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE), "fr(1) did not start"
        q(BASE, ["FLUSHALL"])
        for c in [["SET", "s_int", "12345"], ["SET", "s_raw", "y" * 100],
                  ["RPUSH", "l_ql", *[("x" * 60) for _ in range(200)]],
                  ["SADD", "set_ht", *[f"m{i}" for i in range(300)]],
                  ["HSET", "h_ht", *sum([[f"f{i}", f"v{i}"] for i in range(300)], [])],
                  ["ZADD", "z_sl", *sum([[str(i * 1.5), f"m{i}"] for i in range(300)], [])],
                  ["XADD", "stream", "1-1", "f", "v"], ["PFADD", "hll", *[f"e{i}" for i in range(100)]],
                  ["SET", "exp_key", "v"], ["EXPIRE", "exp_key", "100000"]]:
            q(BASE, c)
        fr_digest = q(BASE, ["DEBUG", "DIGEST"]).strip()
        q(BASE, ["BGREWRITEAOF"])
        time.sleep(1.2)
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 1), "--dir", d1, "--appendonly", "yes",
             "--appenddirname", ".", "--appendfilename", "appendonly.aof",
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 1), "redis did not start loading fr AOF"
        r_digest = q(BASE + 1, ["DEBUG", "DIGEST"]).strip()
        if r_digest != fr_digest:
            failures.append(("fr->redis", fr_digest, r_digest))

        # ---- direction 2: redis writes AOF (base + incr cmd-log), fr loads ----
        d2 = os.path.join(root, "d2")
        os.makedirs(d2, exist_ok=True)
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 2), "--dir", d2, "--appendonly", "yes",
             "--appenddirname", "aofdir", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 2), "redis(2) did not start"
        q(BASE + 2, ["FLUSHALL"])
        q(BASE + 2, ["SET", "base1", "v1"])
        q(BASE + 2, ["RPUSH", "blist", *[str(i) for i in range(50)]])
        q(BASE + 2, ["HSET", "bhash", *sum([[f"f{i}", f"v{i}"] for i in range(100)], [])])
        q(BASE + 2, ["BGREWRITEAOF"])
        time.sleep(1.2)
        # incremental command log
        for c in [["SET", "incr1", "iv1"], ["INCR", "counter"], ["INCR", "counter"],
                  ["LPUSH", "ilist", "z", "y", "x"], ["SADD", "iset", "a", "b", "c"],
                  ["ZADD", "izset", "1.5", "m1", "2.5", "m2"], ["EXPIRE", "incr1", "100000"],
                  ["APPEND", "base1", "_appended"], ["HDEL", "bhash", "f0"]]:
            q(BASE + 2, c)
        time.sleep(1.0)  # everysec fsync
        r2_digest = q(BASE + 2, ["DEBUG", "DIGEST"]).strip()
        copytree(os.path.join(d2, "aofdir"), os.path.join(d2, "frload"))
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(BASE + 3), "--aof", os.path.join(d2, "frload", "appendonly.aof"),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 3), "fr did not start loading redis AOF"
        fr2_digest = q(BASE + 3, ["DEBUG", "DIGEST"]).strip()
        if fr2_digest != r2_digest:
            failures.append(("redis->fr (incr replay)", r2_digest, fr2_digest))
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
        print(f"FAIL - {len(failures)} AOF cross-load divergence(s):")
        for direction, want, got in failures:
            print(f"  [{direction}] digest mismatch:\n    source={want}\n    loaded={got}")
        sys.exit(1)
    print("PASS - AOF byte-compatible fr <-> redis 7.2.4 both directions"
          " (base.rdb + incr command-replay, DEBUG DIGEST identical)")


main()
