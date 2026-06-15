#!/usr/bin/env python3
"""Bidirectional cross-impl replication gate: fr <-> redis 7.2.4.

Replication wire-compatibility is what makes zero-downtime migration possible
(make fr a replica of a live redis, let it sync, then promote fr) and what a
mixed fr/redis fleet relies on. A break = failed migration or silent replica
divergence. This gate proves BOTH roles work both ways, via DEBUG DIGEST:

  (1) fr master  <- redis replica : redis full-syncs the RDB from fr, then
                                    online writes to fr propagate to redis.
  (2) redis master <- fr replica  : fr full-syncs (PSYNC client side) the RDB
                                    from redis, then online writes to redis
                                    propagate to fr.

Both phases check DIGEST after full sync AND after a batch of online writes
(SET/INCR x2/LPUSH/EXPIRE/ZADD/HSET/SADD/DEL) across string/list/hash/zset/set
/stream types.

Both servers launched with --enable-debug-command. Self-orchestrating.

Usage: replication_cross_compat_gate.py <redis-server-bin> <fr-bin> [base_port]
"""
import socket, sys, time, subprocess

REDIS_BIN = sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server"
FR_BIN = sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_repl"
BASE = int(sys.argv[3]) if len(sys.argv) > 3 else 29821


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


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"PONG" in q(port, ["PING"]):
                return True
        except Exception:
            time.sleep(0.1)
    return False


def wait_link_up(port, deadline=10):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"master_link_status:up" in q(port, ["INFO", "replication"]):
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def digests_converge(p_master, p_replica, deadline=6):
    """Poll until replica digest == master digest (online propagation settled)."""
    t0 = time.time()
    last = (None, None)
    while time.time() - t0 < deadline:
        dm = q(p_master, ["DEBUG", "DIGEST"]).strip()
        dr = q(p_replica, ["DEBUG", "DIGEST"]).strip()
        last = (dm, dr)
        if dm == dr:
            return True, last
        time.sleep(0.3)
    return False, last


SEED = [
    ["SET", "s", "hi"], ["RPUSH", "l", *[str(i) for i in range(50)]],
    ["HSET", "h", *sum([[f"f{i}", str(i)] for i in range(300)], [])],
    ["ZADD", "z", *sum([[str(i * 1.5), f"m{i}"] for i in range(300)], [])],
    ["SADD", "st", *[f"m{i}" for i in range(300)]], ["XADD", "x", "1-1", "f", "v"],
]
ONLINE = [
    ["SET", "on", "v"], ["INCR", "c"], ["INCR", "c"], ["LPUSH", "ol", "a", "b"],
    ["EXPIRE", "on", "100000"], ["ZADD", "oz", "5", "m"], ["HSET", "oh", "f", "v"],
    ["SADD", "os", "x"], ["DEL", "s"],
]


def main():
    procs = []
    failures = []
    try:
        # ---- phase 1: fr master, redis replica ----
        procs.append(subprocess.Popen([FR_BIN, "--port", str(BASE), "--enable-debug-command", "yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE), "fr master did not start"
        q(BASE, ["FLUSHALL"])
        for c in SEED:
            q(BASE, c)
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 1), "--replicaof", "127.0.0.1", str(BASE),
             "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 1), "redis replica did not start"
        if not wait_link_up(BASE + 1):
            failures.append(("fr-master<-redis-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(BASE, BASE + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica full-sync", dm, dr))
            for c in ONLINE:
                q(BASE, c)
            ok, (dm, dr) = digests_converge(BASE, BASE + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica online", dm, dr))
            # WAIT durability-ack: fr master must report the 1 connected replica
            # acknowledging the latest write offset (clients gate write durability
            # on this). WAIT 1 <timeout> -> :1 once the redis replica ACKs.
            q(BASE, ["SET", "wait_probe", "v"])
            w = q(BASE, ["WAIT", "1", "2000"]).strip()
            if w != b":1":
                failures.append(("fr-master WAIT 1 (replica ack)", b":1", w))

        # ---- phase 2: redis master, fr replica ----
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(BASE + 2), "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 2), "redis master did not start"
        q(BASE + 2, ["FLUSHALL"])
        for c in SEED:
            q(BASE + 2, c)
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(BASE + 3), "--replicaof", "127.0.0.1", str(BASE + 2),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        assert wait_up(BASE + 3), "fr replica did not start"
        if not wait_link_up(BASE + 3):
            failures.append(("redis-master<-fr-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(BASE + 2, BASE + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica full-sync", dm, dr))
            for c in ONLINE:
                q(BASE + 2, c)
            ok, (dm, dr) = digests_converge(BASE + 2, BASE + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica online", dm, dr))
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
        print(f"FAIL - {len(failures)} replication divergence(s):")
        for phase, m, r in failures:
            print(f"  [{phase}]\n    master={m}\n    replica={r}")
        sys.exit(1)
    print("PASS - replication wire-compatible fr <-> redis 7.2.4 both roles"
          " (full PSYNC resync + online propagation + WAIT replica-ack,"
          " DEBUG DIGEST identical)")


main()
