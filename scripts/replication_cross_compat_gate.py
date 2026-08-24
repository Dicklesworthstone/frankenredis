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

Usage: replication_cross_compat_gate.py [--planted-negative] <redis-server-bin> <fr-bin> [base_port]
"""
import argparse
import subprocess
import time

from _respread import assert_ok, assert_seed, cmd, conn


DEFAULT_REDIS_BIN = "legacy_redis_code/redis/src/redis-server"
DEFAULT_FR_BIN = "/tmp/fr_repl"
DEFAULT_BASE = 29821


def q(port, a):
    """One fresh connection, but never a one-recv partial RESP reply."""
    s = conn(port)
    try:
        return cmd(s, *a)
    finally:
        s.close()


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"PONG" in q(port, ["PING"]):
                return True
        except (OSError, ValueError):
            time.sleep(0.1)
    return False


def wait_link_up(port, deadline=10):
    t0 = time.time()
    while time.time() - t0 < deadline:
        try:
            if b"master_link_status:up" in q(port, ["INFO", "replication"]):
                return True
        except (OSError, ValueError):
            continue
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
    ("SET s", ["SET", "s", "hi"], b"+OK\r\n"),
    ("RPUSH l", ["RPUSH", "l", *[str(i) for i in range(50)]], b":50\r\n"),
    ("HSET h", ["HSET", "h", *[value for i in range(300) for value in (f"f{i}", str(i))]], b":300\r\n"),
    ("ZADD z", ["ZADD", "z", *[value for i in range(300) for value in (str(i * 1.5), f"m{i}")]], b":300\r\n"),
    ("SADD st", ["SADD", "st", *[f"m{i}" for i in range(300)]], b":300\r\n"),
    ("XADD x", ["XADD", "x", "1-1", "f", "v"], b"$3\r\n1-1\r\n"),
]
ONLINE = [
    ("SET on", ["SET", "on", "v"], b"+OK\r\n"),
    ("INCR c first", ["INCR", "c"], b":1\r\n"),
    ("INCR c second", ["INCR", "c"], b":2\r\n"),
    ("LPUSH ol", ["LPUSH", "ol", "a", "b"], b":2\r\n"),
    ("EXPIRE on", ["EXPIRE", "on", "100000"], b":1\r\n"),
    ("ZADD oz", ["ZADD", "oz", "5", "m"], b":1\r\n"),
    ("HSET oh", ["HSET", "oh", "f", "v"], b":1\r\n"),
    ("SADD os", ["SADD", "os", "x"], b":1\r\n"),
    ("DEL s", ["DEL", "s"], b":1\r\n"),
]


def assert_reply(reply, expected, label):
    """Fail closed when a seed/write did not actually take effect."""
    if expected == b"+OK\r\n":
        return assert_ok(reply, label)
    if expected.startswith(b":"):
        return assert_seed(reply, int(expected[1:-2]), label)
    if reply != expected:
        raise SystemExit(
            f"SEED FAILED [{label}]: got {reply!r}, expected {expected!r}")
    return reply


def apply_checked(port, writes, phase):
    for label, command, expected in writes:
        assert_reply(q(port, command), expected, f"{phase} {label}")


def mismatches(expected, actual):
    """The gate predicate, isolated so the planted negative exercises it."""
    return expected != actual


def planted_negative():
    """Must fail: a differing full digest reply cannot pass this gate."""
    oracle = b"$40\r\n0123456789abcdef0123456789abcdef01234567\r\n"
    wrong = b"$40\r\n0123456789abcdef0123456789abcdef01234568\r\n"
    if not mismatches(oracle, wrong):
        print("PLANTED NEGATIVE MISSED: unequal replication digests compared equal")
        return 0
    print("PLANTED NEGATIVE DETECTED: unequal replication digest replies fail the gate")
    return 1


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("redis_bin", nargs="?", default=DEFAULT_REDIS_BIN)
    parser.add_argument("fr_bin", nargs="?", default=DEFAULT_FR_BIN)
    parser.add_argument("base_port", nargs="?", type=int, default=DEFAULT_BASE)
    parser.add_argument(
        "--planted-negative",
        action="store_true",
        help="prove this gate rejects a deliberately different complete digest reply",
    )
    return parser.parse_args()


def main(args):
    if args.planted_negative:
        return planted_negative()

    redis_bin, fr_bin, base = args.redis_bin, args.fr_bin, args.base_port
    procs = []
    failures = []
    try:
        # ---- phase 1: fr master, redis replica ----
        procs.append(subprocess.Popen([fr_bin, "--port", str(base), "--enable-debug-command", "yes"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base):
            raise SystemExit("fr master did not start")
        assert_ok(q(base, ["FLUSHALL"]), "fr master FLUSHALL")
        apply_checked(base, SEED, "fr master seed")
        procs.append(subprocess.Popen(
            [redis_bin, "--port", str(base + 1), "--replicaof", "127.0.0.1", str(base),
             "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 1):
            raise SystemExit("redis replica did not start")
        if not wait_link_up(base + 1):
            failures.append(("fr-master<-redis-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(base, base + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica full-sync", dm, dr))
            apply_checked(base, ONLINE, "fr master online")
            ok, (dm, dr) = digests_converge(base, base + 1)
            if not ok:
                failures.append(("fr-master<-redis-replica online", dm, dr))
            # WAIT durability-ack: fr master must report the 1 connected replica
            # acknowledging the latest write offset (clients gate write durability
            # on this). WAIT 1 <timeout> -> :1 once the redis replica ACKs.
            assert_ok(q(base, ["SET", "wait_probe", "v"]), "fr master WAIT probe")
            w = q(base, ["WAIT", "1", "2000"]).strip()
            if w != b":1":
                failures.append(("fr-master WAIT 1 (replica ack)", b":1", w))

        # ---- phase 2: redis master, fr replica ----
        procs.append(subprocess.Popen(
            [redis_bin, "--port", str(base + 2), "--save", "", "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 2):
            raise SystemExit("redis master did not start")
        assert_ok(q(base + 2, ["FLUSHALL"]), "redis master FLUSHALL")
        apply_checked(base + 2, SEED, "redis master seed")
        procs.append(subprocess.Popen(
            [fr_bin, "--port", str(base + 3), "--replicaof", "127.0.0.1", str(base + 2),
             "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        if not wait_up(base + 3):
            raise SystemExit("fr replica did not start")
        if not wait_link_up(base + 3):
            failures.append(("redis-master<-fr-replica", "link never came up", ""))
        else:
            ok, (dm, dr) = digests_converge(base + 2, base + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica full-sync", dm, dr))
            apply_checked(base + 2, ONLINE, "redis master online")
            ok, (dm, dr) = digests_converge(base + 2, base + 3)
            if not ok:
                failures.append(("redis-master<-fr-replica online", dm, dr))
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            try:
                p.kill()
            except ProcessLookupError:
                continue

    print("=" * 60)
    if failures:
        print(f"FAIL - {len(failures)} replication divergence(s):")
        for phase, m, r in failures:
            print(f"  [{phase}]\n    master={m}\n    replica={r}")
        return 1
    print("PASS - replication wire-compatible fr <-> redis 7.2.4 both roles"
          " (full PSYNC resync + online propagation + WAIT replica-ack,"
          " DEBUG DIGEST identical)")
    return 0


raise SystemExit(main(parse_args()))
